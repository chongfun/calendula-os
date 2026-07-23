use crate::{
    catalog, Button, DisplayCommand, DisplayEvent, InputEvent, PowerEvent, ReaderSource,
    RenderKind, StorageCommand, SyncCommand, DISPLAY_COMMANDS, DISPLAY_EVENTS, INPUT_EVENTS,
    LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS, SYNC_COMMANDS,
    SYNC_EVENTS,
};
use app_core::{
    extend_section_command, storage_command_for_transition, AppView, BookOpenRollback,
    ParkedStorage, ReaderState, ReducerContext, SleepGate, StorageDispatch, SyncStatus,
};
use core::sync::atomic::Ordering;
use embassy_futures::select::{select4, Either4};
use embassy_time::{Duration, Instant};

const POST_OPEN_CONFIRM_BLOCK_MS: u64 = 700;

#[embassy_executor::task]
pub async fn run() {
    esp_println::println!("app: started");
    let ctx = reducer_context();
    let mut state = ReaderState::boot();
    // Compile-time dev credentials name the network immediately; the
    // display task's boot probe of /XTEINK/WIFI.BIN arrives later and
    // overrides, matching the wifi task's stored-beats-built-in order.
    if let Some((ssid, _)) = crate::tasks::wifi::credentials() {
        if let Some(ssid) = app_core::WifiSsid::new(ssid) {
            state = state.apply_sync_event(app_core::SyncEvent::NetworkSaved(ssid));
        }
    }
    let mut rendering = false;
    let mut render_pending = false;
    let mut catalog_refresh_requested = true;
    let mut pending_storage = ParkedStorage::new();
    // Type settings changed while away from Reading: the loaded section is
    // paginated under the old layout, so the next entry into Reading must
    // send an extend even though page and chapter are unchanged.
    let mut reader_relayout_pending = false;
    let mut opening_book: Option<u32> = None;
    // Where to put the reader back if the inflight open aborts. Set only for
    // a book change, the one open that has somewhere else to return to.
    let mut open_rollback: Option<BookOpenRollback> = None;
    // A Power press arriving while the app still owes the storage task work.
    let mut sleep_gate = SleepGate::new();
    let mut suppress_input_until_open_settled = false;
    let mut block_confirm_until: Option<Instant> = None;
    // Defer the first paint. ReaderState::boot() defaults to the built-in guide
    // (book_id 1), so drawing it now flashes "About This Reader" until the saved
    // book loads from SD ~1.5s later. Instead, kick the catalog + saved-position
    // restore now and leave the retained image (the sleep screen, on a deep-sleep
    // wake) up until it resolves; the first render is sent when Restored/Scanned
    // lands (see first_render_kind), so wake is a single refresh straight onto the
    // restored book. Now that sleep is terminal, wake is a full reboot — before
    // that this cold-boot path never showed, the real book stayed resident.
    let mut boot_render_pending = true;
    if STORAGE_COMMANDS
        .try_send(StorageCommand::LoadCatalogCache)
        .is_err()
    {
        esp_println::println!("app: storage queue full for catalog cache");
    }

    loop {
        match select4(
            INPUT_EVENTS.receive(),
            DISPLAY_EVENTS.receive(),
            LIBRARY_EVENTS.receive(),
            SYNC_EVENTS.receive(),
        )
        .await
        {
            Either4::First(event) => {
                if matches!(event, InputEvent::Sample { button: None, .. }) {
                    // A button-less sample is a pure battery reading (the input
                    // task emits one at boot, before the first paint). Fold the
                    // charge into state but spend no panel refresh on it -- the
                    // value rides out on the next real paint. At boot that's the
                    // deferred Restored paint (see boot_render_pending), so the
                    // first screen shows the true charge instead of boot()'s
                    // 100% placeholder.
                    state = state.apply_input(ctx, event);
                    continue;
                }
                if matches!(
                    event,
                    InputEvent::Sample {
                        button: Some(Button::Power),
                        ..
                    }
                ) {
                    // Hand off to the power task, which drives the display to its
                    // sleep image and then deep-sleeps the SoC with the Power
                    // button armed as the wake source. Waking is a fresh boot
                    // (deep sleep is terminal), so there is no in-app "asleep"
                    // state to toggle back out of here.
                    //
                    // Unless the app is still holding storage work. Sleep would
                    // reach the display task down its own channel and be picked
                    // up ahead of anything queued behind it, and the pre-sleep
                    // flush there cannot see a command parked in this task -- so
                    // a book open in either position would go down with the
                    // reader's place in the book it was leaving.
                    if sleep_gate.press(opening_book.is_some() || !pending_storage.is_empty()) {
                        esp_println::println!("app: sleep requested");
                        let _ = POWER_EVENTS.send(PowerEvent::SleepNow).await;
                    } else {
                        esp_println::println!("app: sleep deferred until book open settles");
                    }
                    continue;
                }

                if state.view == AppView::Reading
                    && should_block_post_open_confirm(event, &mut block_confirm_until)
                {
                    esp_println::println!("app: confirm ignored after book open");
                    continue;
                }

                if opening_book.is_some() || suppress_input_until_open_settled {
                    esp_println::println!("app: input ignored while book open pending");
                    continue;
                }

                let previous = state;
                let previous_persisted = state.persisted();
                state = state.apply_input(ctx, event);
                // Activity carries the post-input view so entering a view
                // immediately gets that view's idle leash (e.g. opening a
                // book starts the long Reading timeout right away).
                let _ = POWER_EVENTS.try_send(PowerEvent::Activity(state.view));
                if previous.type_settings() != state.type_settings()
                    || app_core::is_portrait(previous.orientation)
                        != app_core::is_portrait(state.orientation)
                {
                    reader_relayout_pending = true;
                }
                // Allocate the id speculatively and only commit it if a command
                // actually goes out: bumping the counter for a transition that
                // sends nothing would make an inflight open look stale, and the
                // storage task would skip it without ever answering.
                let request_id = peek_reader_request_id();
                let mut storage_command =
                    storage_command_for_transition(&previous, &state, request_id);
                if storage_command.is_none()
                    && reader_relayout_pending
                    && state.view == AppView::Reading
                {
                    if let Some(index) = ReaderSource::from_book_id(state.book_id).sd_index() {
                        storage_command = Some(extend_section_command(&state, index, request_id));
                    }
                }
                // A book change closes out the departing book inside its own
                // open. The separate progress record that used to follow named
                // the *new* book, so it wrote that book's position file at the
                // page the open had not resolved yet — erasing the very place
                // the reader was about to resume from.
                let open_owns_the_switch = matches!(
                    storage_command,
                    Some(StorageCommand::OpenBook {
                        previous: Some(_),
                        ..
                    })
                );
                // The chapter overview can't paint its rows until the on-disk
                // list lands; hold the current frame and let the Loaded event
                // render once, rather than flashing a partial first frame and
                // spending an extra panel refresh. Only when the command is
                // truly in flight -- a queued command relies on the render's
                // Settled to be drained, so it must still render.
                let mut awaiting_chapter_list = false;
                let mut switch_dispatched = false;
                if let Some(command) = storage_command {
                    match dispatch_storage(&mut pending_storage, command) {
                        StorageDispatch::Rejected => {
                            // Nothing reached the storage task, so nothing is
                            // coming back. Arming the open lock here would wait
                            // on a Loaded that cannot arrive and ignore every
                            // button until the battery is pulled; put the reader
                            // back on the book it never actually left instead.
                            // The render below then redraws that book.
                            if open_book_id(command).is_some() {
                                state = state.restore_after_failed_open(previous.open_rollback());
                            }
                        }
                        outcome => {
                            // Open/extend commands carry the current type
                            // settings, so any dispatched command syncs the
                            // reader store.
                            reader_relayout_pending = false;
                            commit_reader_request_id(request_id);
                            switch_dispatched = open_owns_the_switch;
                            if let Some(book_id) = open_book_id(command) {
                                opening_book = Some(book_id);
                                suppress_input_until_open_settled = true;
                                // Only a switch can abort, and only a switch
                                // has a book to go back to.
                                open_rollback =
                                    open_owns_the_switch.then(|| previous.open_rollback());
                            }
                            if outcome == StorageDispatch::Sent
                                && matches!(command, StorageCommand::LoadChapters { .. })
                            {
                                awaiting_chapter_list = true;
                            }
                        }
                    }
                }
                // Read back after the dispatch: a rejected open has rolled the
                // state to where it started, which leaves nothing to persist
                // and no risk of writing the arriving book's position for a
                // switch that never happened.
                let next_persisted = state.persisted();
                if previous_persisted != next_persisted && !switch_dispatched {
                    dispatch_storage(
                        &mut pending_storage,
                        StorageCommand::StoreProgress(next_persisted),
                    );
                }
                if let Some(command) = forget_command_for_transition(&previous, &state) {
                    dispatch_storage(&mut pending_storage, command);
                }
                if let Some(command) = sync_command_for_transition(&previous, &state) {
                    esp_println::println!("app: sync command {:?}", command);
                    if SYNC_COMMANDS.try_send(command).is_err() {
                        esp_println::println!("app: sync command queue full");
                    }
                }
                // We used to suppress the render when an open was inflight
                // and wait for the Loaded event. That's fine when the cache
                // hits and the open returns in milliseconds, but on a cache
                // miss the rebuild can take a minute and the UI looks frozen
                // on the previous screen. Let the render through immediately:
                // the Reading view draws "OPENING EPUB" while sd_library's
                // loaded_index doesn't match the requested book. The chapter
                // overview is the exception: its list arrives in a beat, so it
                // waits for that Loaded rather than painting a partial frame.
                if awaiting_chapter_list {
                    render_pending = false;
                } else if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, &state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
            Either4::Second(event) => match event {
                DisplayEvent::Settled => {
                    rendering = false;
                    if !catalog_refresh_requested {
                        catalog_refresh_requested = true;
                        if STORAGE_COMMANDS
                            .try_send(StorageCommand::LoadCatalogCache)
                            .is_err()
                        {
                            esp_println::println!("app: storage queue full for catalog cache");
                        }
                    }
                    drain_parked_storage(
                        &mut pending_storage,
                        &mut opening_book,
                        &mut suppress_input_until_open_settled,
                    )
                    .await;
                    release_deferred_sleep(&mut sleep_gate, opening_book, &pending_storage).await;
                    if render_pending {
                        send_render(RenderKind::Page, &state).await;
                        rendering = true;
                        render_pending = false;
                    } else if suppress_input_until_open_settled && opening_book.is_none() {
                        suppress_input_until_open_settled = false;
                        block_confirm_until = Some(
                            Instant::now() + Duration::from_millis(POST_OPEN_CONFIRM_BLOCK_MS),
                        );
                    }
                }
                DisplayEvent::Asleep => {
                    // Informational only. When the power task's handshake is
                    // still active, deep sleep follows and reboots the chip,
                    // so app state is moot. When that handshake was abandoned
                    // on Activity, the very input that abandoned it queued
                    // render/open work behind the Sleep command — resetting
                    // the render lock or open suppression here would erase
                    // that work's bookkeeping mid-flight. Every render is
                    // answered by its own Settled/RefreshFailed regardless of
                    // interleaved sleeps, so those acks alone advance state.
                    esp_println::println!("app: display asleep");
                }
                DisplayEvent::SleepFailed => {
                    // A sleep transition failed, not the current render: a
                    // render sent after the input that aborted the sleep
                    // handshake may still be queued behind that Sleep
                    // command, and its own Settled/RefreshFailed is coming.
                    // Clearing the render lock here would double-render and
                    // drop the coalesced pending frame, so leave both.
                    esp_println::println!("app: display sleep failed");
                }
                DisplayEvent::RefreshFailed => {
                    // The frame never reached the panel. Clear the render
                    // lock so the next input re-renders instead of queueing
                    // behind an acknowledgement that will never arrive, but
                    // drop the coalesced pending render: it described a
                    // frame for a panel state that no longer holds.
                    esp_println::println!("app: display refresh failed");
                    rendering = false;
                    render_pending = false;
                    // This failure ends the display cycle the same way
                    // Settled would, and it is the only other drain point
                    // for the parked storage commands: a queued book open
                    // left in pending_storage would otherwise hold
                    // opening_book forever and suppress every input.
                    drain_parked_storage(
                        &mut pending_storage,
                        &mut opening_book,
                        &mut suppress_input_until_open_settled,
                    )
                    .await;
                    release_deferred_sleep(&mut sleep_gate, opening_book, &pending_storage).await;
                    // Loaded may already have cleared opening_book before
                    // this failure discarded its render; without Settled
                    // ever arriving, the suppression flag must be released
                    // here or input stays ignored for good.
                    if suppress_input_until_open_settled && opening_book.is_none() {
                        suppress_input_until_open_settled = false;
                        block_confirm_until = Some(
                            Instant::now() + Duration::from_millis(POST_OPEN_CONFIRM_BLOCK_MS),
                        );
                    }
                }
                DisplayEvent::Library(event) => {
                    if !fold_library_event(
                        ctx,
                        &mut state,
                        &mut opening_book,
                        &mut open_rollback,
                        boot_render_pending,
                        &event,
                    ) {
                        release_deferred_sleep(&mut sleep_gate, opening_book, &pending_storage)
                            .await;
                        continue;
                    }
                    release_deferred_sleep(&mut sleep_gate, opening_book, &pending_storage).await;
                    if rendering {
                        render_pending = true;
                    } else {
                        send_render(first_render_kind(&mut boot_render_pending), &state).await;
                        rendering = true;
                        render_pending = false;
                    }
                }
            },
            Either4::Third(event) => {
                if !fold_library_event(
                    ctx,
                    &mut state,
                    &mut opening_book,
                    &mut open_rollback,
                    boot_render_pending,
                    &event,
                ) {
                    release_deferred_sleep(&mut sleep_gate, opening_book, &pending_storage).await;
                    continue;
                }
                release_deferred_sleep(&mut sleep_gate, opening_book, &pending_storage).await;
                if rendering {
                    render_pending = true;
                } else {
                    send_render(first_render_kind(&mut boot_render_pending), &state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
            Either4::Fourth(event) => {
                state = state.apply_sync_event(event);
                if state.view != AppView::Wireless {
                    continue;
                }
                if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, &state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
        }
    }
}

/// The first paint after boot uses `RenderKind::Boot` — a full refresh that
/// re-initialises the panel from its post-deep-sleep off state. Every paint
/// after that is an ordinary page. Consumes the one-shot flag.
fn first_render_kind(boot_render_pending: &mut bool) -> RenderKind {
    if core::mem::take(boot_render_pending) {
        RenderKind::Boot
    } else {
        RenderKind::Page
    }
}

/// Folds a library event into reader state and reports whether it owes a
/// render. Shared by the two arms that deliver library events — the display
/// event channel and the direct library channel — which must treat them
/// identically.
fn fold_library_event(
    ctx: ReducerContext,
    state: &mut ReaderState,
    opening_book: &mut Option<u32>,
    open_rollback: &mut Option<BookOpenRollback>,
    boot_render_pending: bool,
    event: &crate::LibraryEvent,
) -> bool {
    if let crate::LibraryEvent::BookOpenFailed { book_id } = *event {
        // The book was never opened, so the reader has to land back on the
        // one it was reading rather than sit on a title the storage task
        // refused. Always repaints: the screen is currently showing the open
        // that is not going to happen.
        if *opening_book == Some(book_id) {
            *opening_book = None;
        }
        if let Some(rollback) = open_rollback.take() {
            esp_println::println!(
                "app: book open failed book_id={book_id}; back to book_id={}",
                rollback.book_id
            );
            *state = state.restore_after_failed_open(rollback);
        }
        return true;
    }
    if let Some(book_id) = loaded_book_id(event) {
        if *opening_book == Some(book_id) {
            *opening_book = None;
            *open_rollback = None;
        }
    }
    let should_render = if boot_render_pending {
        library_event_allows_first_render(event)
    } else {
        library_event_affects_view(state, event)
    };
    *state = state.apply_library_event(ctx, *event);
    should_render
}

fn library_event_affects_view(state: &ReaderState, event: &crate::LibraryEvent) -> bool {
    match *event {
        crate::LibraryEvent::Scanned { count } => {
            state.view == AppView::Library && state.library_count != count
        }
        crate::LibraryEvent::Loaded {
            book_id,
            pages: _,
            chapters: _,
            current_chapter: _,
            chapter_pages: _,
            position: _,
        } => state.book_id == book_id,
        // Handled before the reducer; never reaches here.
        crate::LibraryEvent::BookOpenFailed { .. } => true,
        crate::LibraryEvent::ChapterPage {
            book_id,
            chapter,
            page,
        } => {
            state.book_id == book_id
                && state
                    .sd_chapter_pages
                    .get(chapter as usize)
                    .map(|stored| *stored != page.min(u16::MAX as u32) as u16)
                    .unwrap_or(false)
        }
        // The reducer adopts the new chapter without a repaint (Reading shows
        // page-within-chapter, not the chapter), so it never forces a render.
        crate::LibraryEvent::ChapterCursor { .. } => false,
        crate::LibraryEvent::CustomFont { .. } => state.view == AppView::Settings,
        crate::LibraryEvent::Restored { .. } => true,
    }
}

fn library_event_allows_first_render(event: &crate::LibraryEvent) -> bool {
    matches!(
        event,
        crate::LibraryEvent::Restored { .. } | crate::LibraryEvent::Scanned { .. }
    )
}

/// Sends `command`, parks it behind whatever is already waiting, or reports
/// that it could do neither. See [`ParkedStorage`] for why an open is never
/// the thing that gets dropped.
fn dispatch_storage(parked: &mut ParkedStorage, command: StorageCommand) -> StorageDispatch {
    let outcome = parked.dispatch(command, |command| {
        STORAGE_COMMANDS.try_send(command).is_ok()
    });
    match outcome {
        StorageDispatch::Sent => log_storage_command("send", command),
        StorageDispatch::Parked => log_storage_command("queue", command),
        StorageDispatch::Rejected => log_storage_command("rejected", command),
    }
    outcome
}

/// Sends a Power press that was held back, once the app owes the storage task
/// nothing. Called wherever an open resolves or the parked queue drains, so a
/// deferred press is never left waiting on an event that already happened.
async fn release_deferred_sleep(
    gate: &mut SleepGate,
    opening_book: Option<u32>,
    parked: &ParkedStorage,
) {
    if gate.release(opening_book.is_some() || !parked.is_empty()) {
        esp_println::println!("app: deferred sleep released");
        let _ = POWER_EVENTS.send(PowerEvent::SleepNow).await;
    }
}

/// Hands every parked command to the storage task in arrival order, blocking
/// on each so a full channel defers the drain rather than losing it.
async fn drain_parked_storage(
    parked: &mut ParkedStorage,
    opening_book: &mut Option<u32>,
    suppress_input_until_open_settled: &mut bool,
) {
    while let Some(command) = parked.pop_front() {
        log_storage_command("send", command);
        if let Some(book_id) = open_book_id(command) {
            *opening_book = Some(book_id);
            *suppress_input_until_open_settled = true;
        }
        STORAGE_COMMANDS.send(command).await;
    }
}

fn open_book_id(command: StorageCommand) -> Option<u32> {
    match command {
        StorageCommand::OpenBook { book_id, .. } => Some(book_id),
        _ => None,
    }
}

fn loaded_book_id(event: &crate::LibraryEvent) -> Option<u32> {
    match *event {
        crate::LibraryEvent::Loaded { book_id, .. } => Some(book_id),
        _ => None,
    }
}

fn should_block_post_open_confirm(event: InputEvent, block_until: &mut Option<Instant>) -> bool {
    let Some(until) = *block_until else {
        return false;
    };
    if Instant::now() >= until {
        *block_until = None;
        return false;
    }
    matches!(
        event,
        InputEvent::Sample {
            button: Some(Button::Confirm),
            ..
        }
    )
}

async fn send_render(kind: RenderKind, state: &ReaderState) {
    DISPLAY_COMMANDS
        .send(DisplayCommand::Render(state.render_request(kind)))
        .await;
}

fn log_storage_command(label: &str, command: StorageCommand) {
    match command {
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            previous,
            ..
        } => esp_println::println!(
            "app: storage {label} open request={request_id} book_id={book_id} index={index} chapter={chapter} target={target_pages} closing={}",
            previous.map_or(0, |state| state.book_id)
        ),
        StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            ..
        } => esp_println::println!(
            "app: storage {label} extend request={request_id} book_id={book_id} index={index} chapter={chapter} target={target_pages}"
        ),
        StorageCommand::StoreProgress(_) => {
            esp_println::println!("app: storage {label} progress")
        }
        StorageCommand::LoadCatalogCache => {
            esp_println::println!("app: storage {label} load catalog cache")
        }
        StorageCommand::RefreshCatalog => {
            esp_println::println!("app: storage {label} refresh catalog")
        }
        StorageCommand::LoanSyncMemory => {
            esp_println::println!("app: storage {label} loan sync memory")
        }
        StorageCommand::StoreWifiCredentials(_) => {
            esp_println::println!("app: storage {label} wifi credentials")
        }
        StorageCommand::ForgetWifiCredentials => {
            esp_println::println!("app: storage {label} forget wifi credentials")
        }
        StorageCommand::ReceiveUpload => {
            esp_println::println!("app: storage {label} receive upload")
        }
        StorageCommand::LoadChapters {
            request_id,
            book_id,
            index,
        } => esp_println::println!(
            "app: storage {label} load chapters request={request_id} book_id={book_id} index={index}"
        ),
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            ..
        } => esp_println::println!(
            "app: storage {label} jump chapter request={request_id} book_id={book_id} index={index} chapter={chapter}"
        ),
    }
}

fn reducer_context() -> ReducerContext {
    ReducerContext::new(catalog::book_count(), catalog::chapter_count())
}

/// Confirm on the Wireless screen arms `Starting`; leaving the screen after
/// the radio ran has to reset the device because the loaned memory can
/// never come back.
fn sync_command_for_transition(previous: &ReaderState, next: &ReaderState) -> Option<SyncCommand> {
    if previous.sync_status != SyncStatus::Starting && next.sync_status == SyncStatus::Starting {
        return Some(SyncCommand::Start);
    }
    if previous.view == AppView::Wireless && next.view != AppView::Wireless {
        let radio_ran = !matches!(
            previous.sync_status,
            SyncStatus::NotConfigured | SyncStatus::Idle | SyncStatus::ForgetPending
        );
        if radio_ran {
            return Some(SyncCommand::Exit);
        }
    }
    None
}

/// Confirming the pending forget on the Wireless screen deletes the saved
/// credentials from the card. Both states live before any radio work, so
/// the storage path is still whole.
fn forget_command_for_transition(
    previous: &ReaderState,
    next: &ReaderState,
) -> Option<StorageCommand> {
    (previous.sync_status == SyncStatus::ForgetPending
        && next.sync_status == SyncStatus::NotConfigured
        && next.view == AppView::Wireless)
        .then_some(StorageCommand::ForgetWifiCredentials)
}

/// Reserves the next reader request id without publishing it.
///
/// Publishing is a separate step because the id is how the storage task
/// recognises a stale request: bumping the counter for a transition that ends
/// up sending nothing would strand an open already in flight, which would be
/// skipped as stale and never answer with the `Loaded` the app is waiting on.
fn peek_reader_request_id() -> u32 {
    LATEST_READER_REQUEST_ID
        .load(Ordering::Relaxed)
        .wrapping_add(1)
        .max(1)
}

fn commit_reader_request_id(request_id: u32) {
    LATEST_READER_REQUEST_ID.store(request_id, Ordering::Relaxed);
}
