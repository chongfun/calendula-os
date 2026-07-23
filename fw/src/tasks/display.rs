use crate::display_flush::{self, Epd};
use crate::reader_cache::{
    self, ReaderCacheScratch, READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH,
    READER_HEADER_SCRATCH, READER_OPF_SCRATCH, READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use crate::reader_store::{
    BookLoadStatus, ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS,
};
use crate::{
    DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, StorageCommand, DISPLAY_COMMANDS,
    DISPLAY_EVENTS, LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS,
};
use app_core::storage_loop::{
    loop_arm, Drained, LoopArm, OpenAction, OpenSequence, SleepAction, SleepRefusal, SleepSequence,
};
use app_core::{
    book_open_outcome, display_orientation_from_u8, refresh_policy_from_u8, AppView,
    DisplayOrientation, PersistedAppState, ReaderSource, RefreshPlanner, RenderKind, RenderRequest,
    SyncSession, SyncStatus,
};
use core::sync::atomic::Ordering;
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::BAND_BYTES;
use embassy_futures::select::{select, Either};
use embassy_time::Instant;
use esp_hal::gpio::Output;
use proto::nvm::AppStateRecord;
use static_cell::ConstStaticCell;

/// Same-book page-turn progress is coalesced: at most one durable state write
/// per this interval, with a guaranteed flush before display sleep. A
/// battery pull can lose at most this many seconds of reading position.
const PROGRESS_WRITE_MIN_SECS: u64 = 15;

static EPUB_TAIL: ConstStaticCell<[u8; READER_TAIL_SCRATCH]> =
    ConstStaticCell::new([0; READER_TAIL_SCRATCH]);
static EPUB_HEADER: ConstStaticCell<[u8; READER_HEADER_SCRATCH]> =
    ConstStaticCell::new([0; READER_HEADER_SCRATCH]);
static EPUB_NAME: ConstStaticCell<[u8; proto::epub::MAX_ENTRY_NAME_BYTES]> =
    ConstStaticCell::new([0; proto::epub::MAX_ENTRY_NAME_BYTES]);
static EPUB_COMPRESSED: ConstStaticCell<[u8; READER_COMPRESSED_SCRATCH]> =
    ConstStaticCell::new([0; READER_COMPRESSED_SCRATCH]);
static EPUB_CONTAINER: ConstStaticCell<[u8; READER_CONTAINER_SCRATCH]> =
    ConstStaticCell::new([0; READER_CONTAINER_SCRATCH]);
static EPUB_OPF: ConstStaticCell<[u8; READER_OPF_SCRATCH]> =
    ConstStaticCell::new([0; READER_OPF_SCRATCH]);
static EPUB_XHTML: ConstStaticCell<[u8; READER_XHTML_SCRATCH]> =
    ConstStaticCell::new([0; READER_XHTML_SCRATCH]);
static EPUB_BOOK_SECTIONS: ConstStaticCell<[proto::cache::BookV2SectionRecord; MAX_BOOK_SECTIONS]> =
    ConstStaticCell::new([EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS]);
static EPUB_SCRATCH: static_cell::StaticCell<ReaderCacheScratch<'static>> =
    static_cell::StaticCell::new();

#[embassy_executor::task]
pub async fn run(mut epd: Epd, mut sd_cs: Output<'static>, deep_sleep_wake: bool) {
    esp_println::println!("display: started");

    static FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB.init(Framebuffer::new());
    // The previous-frame buffer sits in dram2 so the radio's statics fit
    // in main DRAM; same exclusive &'static mut as the old local cell.
    let prev_fb = crate::sync_mem::take_prev_fb().expect("prev_fb claimed once");
    static TX_BAND: static_cell::StaticCell<[u8; BAND_BYTES]> = static_cell::StaticCell::new();
    let tx_band = TX_BAND.init([0; BAND_BYTES]);
    let mut epub_scratch = None;
    // Storage-command admission for the sync session lifecycle; the loan
    // transition and refusal rules live in app-core with the contracts.
    let mut sync_session = SyncSession::default();
    // On a deep-sleep (Power button) wake the panel still shows the sleep
    // screen: deep_sleep_wake is true only when the RTC wake cause is the
    // armed GPIO *and* the pre-sleep handshake recorded that the sleep frame
    // settled on the panel (see sleep_marker). The seeded planner then picks
    // the ~1.5 s one-flicker FastClean for the wake render instead of the
    // ~3.5 s multi-flash Full. Any other boot — battery pull, crash, software
    // reset, or a sleep whose final flush failed — leaves the seed false and
    // keeps the full waveform for unknown panel contents.
    let mut refresh_planner = RefreshPlanner::new().with_panel_shows_sleep_screen(deep_sleep_wake);
    let mut pending_progress: Option<AppStateRecord> = None;
    let mut last_progress_write: Option<Instant> = None;
    // Durable state is consulted once per boot, after the first catalog with
    // entries lands; later catalog refreshes must not yank reading state.
    let mut state_restored = false;
    // True while RED RAM is known to hold exactly prev_fb's content, letting
    // a fast refresh skip its previous-frame stream. Reset on any failure,
    // sleep, or panel re-init; false just means the next flush writes RED.
    let mut prev_prestaged = false;
    static SD_LIBRARY: ConstStaticCell<ReaderStore> = ConstStaticCell::new(ReaderStore::new());
    let sd_library = SD_LIBRARY.take();
    // ReaderStore::new() is all-zero bytes so the 47 KB static lives in
    // .bss (not a flashed .data image); fill in the non-zero defaults once,
    // in place, before anything reads the store.
    sd_library.init_runtime_defaults();
    // ASCII glyph metrics for the custom font pack; shared by the build's
    // line measurement and the reading-page draw so both stay off the card.
    static FONT_METRICS: ConstStaticCell<crate::custom_font::MetricCache> =
        ConstStaticCell::new(crate::custom_font::MetricCache::new());
    let font_metrics = FONT_METRICS.take();

    // No panel init here: the first-render guard in the loop below (fresh
    // planner — screen off, no last request) owns the boot init, exactly as
    // it already owned re-init after a display sleep. Initializing at task
    // start too made every boot's first render pay reset + init twice (on
    // the X3 that second pass re-whitens both ~52 KB DTM planes).

    // One-shot firmware self-update: if the card holds a pending image, flash it
    // into the inactive OTA slot and reboot into it before the reader starts.
    // Runs here because SD access lives behind this task's shared SPI bus, and
    // the radio is still idle so the flash writes are safe. Runs on every boot,
    // deep-sleep wakes included: the card is user-removable, so an update can
    // be staged offline while the device sleeps and arrive through a Power-
    // button wake — wifi-staged updates are not the only source. The no-
    // trigger probe costs one failed open on the mounted root, and the cold
    // card init it pays is one the first render's SD reads would pay anyway.
    match crate::sd_session::with_root(
        &mut epd,
        &mut sd_cs,
        crate::ota_update::apply_pending_update,
    ) {
        Ok(true) => {
            esp_println::println!("display: firmware update staged; resetting");
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
            esp_hal::system::software_reset();
        }
        Ok(false) => {}
        Err(e) => esp_println::println!("display: update check skipped: {:?}", e),
    }

    // Flash-path self-test (feature `ota-selftest` only, off in release): copy
    // the running image into the inactive slot and boot into it, once. A card-
    // reader-free way to re-validate the esp-storage + otadata path on device.
    #[cfg(feature = "ota-selftest")]
    if crate::ota_update::run_selftest() {
        esp_println::println!("selftest: staged; resetting");
        embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
        esp_hal::system::software_reset();
    }

    loop {
        match select(DISPLAY_COMMANDS.receive(), STORAGE_COMMANDS.receive()).await {
            Either::First(DisplayCommand::Render(request)) => {
                let content_context_changed = refresh_planner
                    .last_request()
                    .map(|last| (last.view, last.book_id))
                    != Some((request.view, request.book_id));
                // The catalog is streamed from the card, so make the slice this
                // view needs resident before the (pure) render reads it. Library
                // pulls the list window around the selection; other views need
                // the active book's entry, refreshed only when the book changes.
                // Skipped once the sync session is running.
                if !sync_session.active() {
                    if request.view == AppView::Library {
                        crate::library_sd::ensure_library_window(
                            &mut epd,
                            &mut sd_cs,
                            sd_library,
                            request.selection,
                            app_core::is_portrait(request.orientation),
                        );
                    } else if ReaderSource::from_book_id(request.book_id).is_sd() {
                        if let Some(index) = ReaderStore::selected_book_index(request.book_id) {
                            if content_context_changed {
                                crate::library_sd::load_active_entry(
                                    &mut epd, &mut sd_cs, sd_library, index,
                                );
                            }
                            // Long TOCs are windowed like the catalog; slide
                            // the window over the rows this render will show.
                            if request.view == AppView::Chapters && sd_library.text_holds_toc() {
                                reader_cache::ensure_toc_window(
                                    &mut epd,
                                    &mut sd_cs,
                                    sd_library,
                                    index,
                                    request.selection as usize,
                                    app_core::is_portrait(request.orientation),
                                );
                            }
                        }
                    }
                }
                let layout_start = Instant::now();
                if !render_custom_reader(
                    &mut epd,
                    &mut sd_cs,
                    fb,
                    request,
                    sd_library,
                    font_metrics,
                ) {
                    crate::views::render(fb, request, sd_library);
                }
                let layout_ms = layout_start.elapsed().as_millis();

                // Sole panel-init site: true for a boot's first render (fresh
                // planner) and again after any display sleep — record_sleep
                // clears last_request, which also covers the aborted-sleep
                // path where a late button press interrupts the handshake
                // after the panel already powered down.
                if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                    esp_println::println!("display: wake init start");
                    if let Err(error) = display_flush::init_panel(&mut epd).await {
                        // The panel never came up; flushing into it would
                        // stream into a dead controller. Fail this render —
                        // the app clears its render lock and the next
                        // request retries init from scratch.
                        esp_println::println!("display: wake init failed: {:?}", error);
                        prev_prestaged = false;
                        let (display_event, power_event) = app_core::display_refresh_outcome(false);
                        send_required_display_event(&display_event);
                        send_required_power_event(power_event).await;
                        continue;
                    }
                    esp_println::println!("display: wake init complete");
                    prev_prestaged = false;
                }

                let mode = refresh_planner.mode_for(request);
                if content_context_changed {
                    esp_println::println!(
                        "display: context changed, refresh policy {:?} -> {:?}",
                        request.refresh_policy,
                        mode
                    );
                }
                let flush_start = Instant::now();
                if display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    tx_band,
                    refresh_planner.screen_on(),
                    mode,
                    prev_prestaged,
                )
                .await
                .is_ok()
                {
                    let flush_ms = flush_start.elapsed().as_millis();
                    refresh_planner.record_render(request, mode);
                    prev_fb.copy_from(fb);
                    // Keep the current chapter tracking the page just shown, past
                    // the reducer's 128-chapter cap. Cheap in-RAM check; only the
                    // loaded SD reader has an uncapped page map, so this no-ops on
                    // other views and reads SD only when the chapter changes. Must
                    // run before Settled: the app clears its render lock on Settled
                    // and may immediately render or navigate, so the ChapterCursor
                    // correction has to be queued ahead of it or that next action
                    // uses the stale chapter.
                    if request.view == AppView::Reading {
                        if let Some(current) = reader_cache::track_reading_chapter(
                            &mut epd,
                            &mut sd_cs,
                            request.page,
                            sd_library,
                        ) {
                            send_loaded_library_event(&LibraryEvent::ChapterCursor {
                                book_id: request.book_id,
                                current_chapter: current,
                            });
                        }
                    }
                    // Settle before the ~23 ms RED prestage: the panel is visually
                    // done, so unblock the input/power pipeline. The prestage still
                    // runs on this task before the next command is dequeued, so
                    // `prev_prestaged` is always current by the next flush, and a
                    // Sleep queued by power_task after DisplaySettled waits behind it.
                    let (display_event, power_event) = app_core::display_refresh_outcome(true);
                    send_required_display_event(&display_event);
                    send_required_power_event(power_event).await;
                    let prestage_start = Instant::now();
                    prev_prestaged = display_flush::prestage_previous(&mut epd, fb, tx_band)
                        .await
                        .is_ok();
                    esp_println::println!(
                        "bench: render view={:?} mode={:?} page={} chapter={} layout_ms={} flush_ms={} prestage_ms={} t_ms={}",
                        request.view,
                        mode,
                        request.page,
                        request.chapter,
                        layout_ms,
                        flush_ms,
                        prestage_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                } else {
                    esp_println::println!("display: SPI transfer failed");
                    prev_prestaged = false;
                    // The flush may have run partially, so the panel's RAM
                    // and waveform state no longer match the planner's model;
                    // forget it so the next render re-inits the panel and
                    // takes the full waveform instead of fast-diffing
                    // against a frame that may never have landed.
                    refresh_planner.record_failure();
                    let (display_event, power_event) = app_core::display_refresh_outcome(false);
                    send_required_display_event(&display_event);
                    send_required_power_event(power_event).await;
                }
            }
            Either::First(DisplayCommand::Sleep { generation }) => {
                let sleep_start = Instant::now();
                esp_println::println!(
                    "bench: sleep phase=requested screen_on={} t_ms={}",
                    refresh_planner.screen_on(),
                    sleep_start.as_millis(),
                );
                // Everything owed to the card, in order, before the panel goes
                // down. The ordering rules live in `SleepSequence` so they can
                // be driven from a host test; this arm only does what it is
                // told and reports back what the hardware said.
                let mut sleep = SleepSequence::new(STORAGE_COMMANDS.capacity());
                let refusal = loop {
                    match sleep.next() {
                        SleepAction::TakeQueued => match STORAGE_COMMANDS.try_receive() {
                            Err(_) => sleep.queue_empty(),
                            Ok(command) => match sleep.drained(&command) {
                                Drained::Apply => {
                                    esp_println::println!("storage: draining before sleep");
                                    handle_storage_command(
                                        command,
                                        &mut epd,
                                        &mut sd_cs,
                                        sd_library,
                                        font_metrics,
                                        &mut epub_scratch,
                                        &mut sync_session,
                                        &mut pending_progress,
                                        &mut last_progress_write,
                                        &mut state_restored,
                                    );
                                    sleep.applied();
                                }
                                Drained::RequeueAndRefuse => {
                                    let _ = STORAGE_COMMANDS.try_send(command);
                                    sleep.requeued();
                                }
                            },
                        },
                        SleepAction::FlushProgress => {
                            let stored = flush_pending_progress(
                                &mut epd,
                                &mut sd_cs,
                                sd_library,
                                &mut pending_progress,
                                &mut last_progress_write,
                            );
                            sleep.flushed(stored);
                        }
                        SleepAction::Refuse(refusal) => break Some(refusal),
                        SleepAction::Proceed => break None,
                    }
                };
                if let Some(refusal) = refusal {
                    // Stay awake. The power task's idle clock re-requests sleep
                    // once this failure releases its handshake wait, by which
                    // time the upload session has run or the pending record has
                    // been retried by the next flush.
                    match refusal {
                        SleepRefusal::UploadQueued => {
                            esp_println::println!("display: sleep deferred; upload session pending")
                        }
                        SleepRefusal::ProgressUnwritten => esp_println::println!(
                            "display: sleep deferred; progress persistence failed"
                        ),
                    }
                    send_required_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                    continue;
                }
                let request = refresh_planner.last_request().or_else(|| {
                    sleep_request_from_saved_state(
                        &mut epd,
                        &mut sd_cs,
                        sd_library,
                        &pending_progress,
                    )
                });
                if let Some(request) = request {
                    crate::views::render_sleep(fb, request, sd_library);
                } else {
                    crate::views::render_sleep_blank(fb);
                }
                let sleep_frame_settled = if display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    tx_band,
                    refresh_planner.screen_on(),
                    RefreshMode::Full,
                    prev_prestaged,
                )
                .await
                .is_ok()
                {
                    prev_fb.copy_from(fb);
                    esp_println::println!(
                        "bench: sleep phase=refresh ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    true
                } else {
                    esp_println::println!("display: sleep framebuffer flush failed");
                    esp_println::println!(
                        "bench: sleep phase=refresh ok=false elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    false
                };
                prev_prestaged = false;
                let panel_slept = display_flush::sleep_panel(&mut epd).await.is_ok();
                // Whenever the panel actually slept the planner must know the
                // screen is off — an aborted handshake (a late button press
                // beating DisplayAsleep) otherwise renders to a powered-down
                // panel without re-init. The settled flag rides along so a
                // failed flush wakes with the deep full waveform, not a fast
                // clean over stale pixels.
                if panel_slept {
                    refresh_planner.record_sleep(sleep_frame_settled);
                }
                // Persist whether the panel really holds the sleep frame
                // before DisplayAsleep releases the power task to cut power:
                // the next boot's GPIO wake seeds its fast-wake planner from
                // this marker, and a flush or panel-sleep failure must leave
                // it false so that boot falls back to the full waveform.
                crate::sleep_marker::record_sleep_image(panel_slept && sleep_frame_settled);
                if panel_slept {
                    send_required_display_event(&DisplayEvent::Asleep);
                    send_required_power_event(PowerEvent::DisplayAsleep(generation)).await;
                } else {
                    // The panel never acknowledged the sleep sequence, so it
                    // may still be mid-refresh. Cutting power now would
                    // freeze whatever is on screen; report failure so the
                    // power task stays awake and retries on its idle clock.
                    // The handshake may also have partially powered the
                    // controller down, so the planner's screen model is no
                    // longer trustworthy: forget it so the next render
                    // re-inits the panel with the full waveform.
                    refresh_planner.record_failure();
                    esp_println::println!("display: sleep transition failed");
                    send_required_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                }
                esp_println::println!(
                    "bench: sleep phase=complete ok={} elapsed_ms={} t_ms={}",
                    panel_slept,
                    sleep_start.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
            }
            Either::Second(command) => match loop_arm(&command, sync_session) {
                // The display task is the upload writer until Sleep or
                // wireless Exit closes the session; a Sleep exit has
                // already been re-queued on DISPLAY_COMMANDS.
                LoopArm::UploadSession => {
                    crate::sd_session::upload_session(&mut epd, &mut sd_cs).await;
                }
                LoopArm::RefusedUpload => {
                    esp_println::println!("storage: upload refused outside sync");
                }
                LoopArm::Apply => {
                    // A layout change re-paginates the book, which blocks this
                    // task for the whole rebuild. Paint the title/author plate
                    // first so the wait reads as loading, not frozen: the store
                    // still reports the old settings here, so the reader view
                    // lands on the loading branch. A same-layout open already
                    // shows the plate through the normal render path (the book
                    // isn't loaded yet), so it is skipped here.
                    if refresh_planner.screen_on() {
                        if let Some(loading_request) = open_loading_plate_request(
                            &command,
                            sd_library,
                            refresh_planner.last_request(),
                        ) {
                            crate::views::render(fb, loading_request, sd_library);
                            let mode = refresh_planner.mode_for(loading_request);
                            if display_flush::flush(
                                &mut epd,
                                fb,
                                prev_fb,
                                tx_band,
                                refresh_planner.screen_on(),
                                mode,
                                prev_prestaged,
                            )
                            .await
                            .is_ok()
                            {
                                refresh_planner.record_render(loading_request, mode);
                                prev_fb.copy_from(fb);
                                prev_prestaged = false;
                            } else {
                                // No Settled/Failed events here — the app isn't
                                // waiting on this opportunistic plate — but the
                                // panel state is as unknown as after any failed
                                // flush: drop the prestage claim and the
                                // planner's screen model.
                                esp_println::println!("display: loading plate flush failed");
                                prev_prestaged = false;
                                refresh_planner.record_failure();
                            }
                        }
                    }
                    handle_storage_command(
                        command,
                        &mut epd,
                        &mut sd_cs,
                        sd_library,
                        font_metrics,
                        &mut epub_scratch,
                        &mut sync_session,
                        &mut pending_progress,
                        &mut last_progress_write,
                        &mut state_restored,
                    );
                }
            },
        }
    }
}

pub(crate) fn send_library_event(event: &LibraryEvent) {
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: library event queue full");
    }
}

fn render_custom_reader(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> bool {
    if request.view != AppView::Reading
        || !ReaderSource::from_book_id(request.book_id).is_sd()
        || request.font_family != display::font::FontFamily::Custom
        || display::font::builtin_custom_available()
        || !sd_library.custom_font_available()
    {
        return false;
    }
    crate::sd_session::with_root(epd, sd_cs, |root| {
        crate::views::render_custom_reader_from_root(fb, request, sd_library, font_metrics, root)
    })
    .unwrap_or(false)
}

/// The reader-view render to paint as a loading plate before an open/extend
/// that cannot be answered from the already loaded RAM section. The app sends
/// a normal Reading render around the same time, but the storage receiver can
/// win that race; painting here keeps a first cache build from looking frozen
/// on the previous screen.
fn open_loading_plate_request(
    command: &StorageCommand,
    sd_library: &ReaderStore,
    last_request: Option<RenderRequest>,
) -> Option<RenderRequest> {
    let (book_id, index, target_pages, type_settings, portrait) = match *command {
        StorageCommand::OpenBook {
            book_id,
            index,
            target_pages,
            type_settings,
            portrait,
            ..
        } => (book_id, index, target_pages, type_settings, portrait),
        StorageCommand::ExtendSection {
            book_id,
            index,
            target_pages,
            type_settings,
            portrait,
            ..
        } => (book_id, index, target_pages, type_settings, portrait),
        _ => return None,
    };
    // Only SD books re-paginate and route to the reader loading plate; the
    // built-in book renders from embedded content and never rebuilds.
    if !ReaderSource::from_book_id(book_id).is_sd() {
        return None;
    }
    if sd_library.type_settings() == type_settings
        && sd_library.portrait() == portrait
        && sd_library.covers_global_page(index as usize, target_pages as u32)
    {
        return None;
    }
    let mut request = last_request?;
    request.view = AppView::Reading;
    request.book_id = book_id;
    request.page = target_pages as u32;
    request.font_size = type_settings.size;
    request.line_spacing = type_settings.spacing;
    request.font_weight = type_settings.weight;
    request.font_family = type_settings.family;
    Some(request)
}

/// Kept out of line so the task loop's poll frame stays small; the storage
/// arms below carry multi-KB scratch and run near the stack floor.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn handle_storage_command(
    command: StorageCommand,
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    sync_session: &mut SyncSession,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    state_restored: &mut bool,
) {
    // The session decides what may run: progress writes stay alive during a
    // sync session (they are cheap and harmless); everything
    // that touches the EPUB scratch is gone until the session's reset.
    if !sync_session.admits(&command) {
        esp_println::println!("storage: refused during sync session");
        return;
    }
    match command {
        StorageCommand::LoanSyncMemory => {
            // The session only ends in a reset, so any coalesced position
            // must reach the card before the scratch is dismantled.
            if !flush_pending_progress(
                epd,
                sd_cs,
                sd_library,
                pending_progress,
                last_progress_write,
            ) {
                // The wifi task is blocked on this answer; a silent return
                // would strand it (and the Wireless screen) forever. Refuse
                // observably so it can report the failure and re-park.
                esp_println::println!("storage: sync loan refused; progress persistence failed");
                let _ = crate::SYNC_LOANS.try_send(Err(app_core::SyncError::Storage));
                return;
            }
            ensure_epub_scratch(epub_scratch);
            let Some(scratch) = epub_scratch.take() else {
                let _ = crate::SYNC_LOANS.try_send(Err(app_core::SyncError::Storage));
                return;
            };
            sync_session.loan_granted();
            let mut loan = reader_cache::dismantle_scratch(scratch);
            loan.wifi = reader_cache::load_wifi_credentials(epd, sd_cs).map(|record| {
                app_core::WifiCredentials {
                    ssid: record.ssid,
                    ssid_len: record.ssid_len,
                    password: record.password,
                    password_len: record.password_len,
                }
            });
            loan.catalog_len = crate::library_sd::write_catalog_listing(epd, sd_cs, loan.http_b);
            if crate::SYNC_LOANS.try_send(Ok(loan)).is_err() {
                // Unreachable in practice: the wifi task blocks on each
                // answer before it can request again. The memory is gone
                // either way.
                esp_println::println!("storage: sync loan channel full");
            }
        }
        StorageCommand::LoadCatalogCache => {
            // Boot-time probe: name the saved network so the Wireless
            // screen can offer connect/forget honestly. The command runs
            // once per boot, before any session can start.
            if let Some(record) = reader_cache::load_wifi_credentials(epd, sd_cs) {
                let ssid = app_core::WifiSsid {
                    bytes: record.ssid,
                    len: record.ssid_len,
                };
                esp_println::println!("wifi: saved network '{}'", ssid.as_str());
                let _ = crate::SYNC_EVENTS.try_send(crate::SyncEvent::NetworkSaved(ssid));
            } else {
                esp_println::println!("wifi: no saved network");
            }
            reader_cache::load_custom_font_manifest(epd, sd_cs, sd_library);
            send_library_event(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            if crate::library_sd::load_catalog_cache(epd, sd_cs, sd_library) {
                // Restored goes out first so the very next Home repaint
                // already shows the saved book; the Scanned default then
                // sees an SD book active and leaves it alone.
                restore_saved_state(epd, sd_cs, sd_library, state_restored);
                let count = sd_library.catalog_count_u16();
                send_library_event(&LibraryEvent::Scanned { count });
            } else {
                let _ = STORAGE_COMMANDS.try_send(StorageCommand::RefreshCatalog);
            }
        }
        StorageCommand::RefreshCatalog => {
            reader_cache::load_custom_font_manifest(epd, sd_cs, sd_library);
            send_library_event(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            crate::library_sd::scan_books(epd, sd_cs, sd_library);
            restore_saved_state(epd, sd_cs, sd_library, state_restored);
            send_library_event(&LibraryEvent::Scanned {
                count: sd_library.catalog_count_u16(),
            });
        }
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            ..
        }
        | StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            ..
        } => {
            let storage_start = Instant::now();
            let latest_request_id = LATEST_READER_REQUEST_ID.load(Ordering::Relaxed);
            // The transaction's order lives in `OpenSequence` so a host test can
            // drive it against a card model that fails whichever write it likes;
            // this arm supplies a real card and reports back what it did.
            let Some(mut open) = OpenSequence::begin(&command, latest_request_id) else {
                esp_println::println!(
                    "storage: stale open skipped request={} latest={} book_id={} index={}",
                    request_id,
                    latest_request_id,
                    book_id,
                    index
                );
                return;
            };
            // `Some(ram_hit)` once a section load was reached; a transaction the
            // close-out refused never gets that far and must not report an open
            // that did not happen.
            let mut section_loaded = None;
            loop {
                match open.next() {
                    OpenAction::CloseOutDeparting(previous) => {
                        let stored = close_out_departing_book(
                            epd,
                            sd_cs,
                            sd_library,
                            pending_progress,
                            last_progress_write,
                            previous,
                        );
                        if !stored {
                            esp_println::println!(
                                "storage: book open {:?} book_id={} departing={}",
                                book_open_outcome(false, false),
                                book_id,
                                previous.book_id,
                            );
                        }
                        open.departing_stored(stored);
                    }
                    OpenAction::Refuse { book_id } => {
                        // Nothing has been opened, so the reader is still whole
                        // on the book it was reading. Announcing the new one
                        // would strand that page: the app has already left the
                        // book that owns it and will never reissue it.
                        send_required_library_event(&LibraryEvent::BookOpenFailed { book_id });
                        open.refused();
                    }
                    OpenAction::StageBook {
                        index,
                        type_settings,
                        portrait,
                    } => {
                        // Read this book's catalog record into the active-entry
                        // slot so the reader pipeline (load_position,
                        // build_or_load) resolves it from the card rather than
                        // the list window. A failure leaves the entry unset and
                        // the open falls through to the usual bad-index error.
                        crate::library_sd::load_active_entry(
                            epd,
                            sd_cs,
                            sd_library,
                            index as usize,
                        );
                        // Adopt the command's type settings before the RAM fast
                        // path: a settings change drops the loaded page
                        // coverage, so the request falls through to the cache
                        // load/rebuild below.
                        sd_library.set_layout(type_settings, portrait);
                        open.staged();
                    }
                    OpenAction::LoadSavedPosition { index } => {
                        let saved =
                            reader_cache::load_position(epd, sd_cs, sd_library, index as usize);
                        open.saved_position(saved);
                        if open.resumed() {
                            esp_println::println!(
                                "storage: resume book {} at chapter {} screen {}",
                                book_id,
                                open.target_chapter(),
                                open.target_page()
                            );
                        }
                    }
                    OpenAction::LoadSection {
                        index,
                        chapter,
                        page,
                    } => {
                        // The requested page is usually inside the section
                        // window that is already loaded; answering from RAM
                        // keeps ordinary page turns free of card init, FAT, and
                        // cache-file traffic.
                        let ram_hit = sd_library.covers_global_page(index as usize, page as u32);
                        section_loaded = Some(ram_hit);
                        if ram_hit {
                            esp_println::println!(
                                "storage: open hit in RAM request={} book_id={} page={}",
                                request_id,
                                book_id,
                                page
                            );
                        } else {
                            esp_println::println!(
                                "storage: open command request={} book_id={} index={} chapter={} target={}",
                                request_id,
                                book_id,
                                index,
                                chapter,
                                page
                            );
                            sd_library.set_reader_status(BookLoadStatus::Loading);
                            let scratch = ensure_epub_scratch(epub_scratch);
                            reader_cache::build_or_load_book_cache(
                                epd,
                                sd_cs,
                                sd_library,
                                index as usize,
                                chapter,
                                page as usize,
                                scratch,
                                font_metrics,
                            );
                        }
                        open.section_loaded();
                    }
                    OpenAction::StorePointer(state) => {
                        let record = record_for_persisted(sd_library, state);
                        let stored = reader_cache::store_global_state(epd, sd_cs, record);
                        if stored {
                            *pending_progress = None;
                            *last_progress_write = Some(Instant::now());
                        } else {
                            // Left owed rather than retried here: the book is
                            // open and the reader is in it, so the only cost of
                            // waiting for the next flush is a reboot in that
                            // window landing back on the old book.
                            *pending_progress = Some(record);
                        }
                        let outcome = book_open_outcome(true, stored);
                        debug_assert!(outcome.book_changed());
                        esp_println::println!(
                            "storage: book open {:?} book_id={} page={}",
                            outcome,
                            record.book_id,
                            record.screen,
                        );
                        esp_println::println!(
                            "bench: store_global_state ok={} book_id={} page={} t_ms={}",
                            stored,
                            record.book_id,
                            record.screen,
                            Instant::now().as_millis(),
                        );
                        open.pointer_stored(stored);
                    }
                    OpenAction::Announce { book_id, position } => {
                        send_loaded_library_event(&LibraryEvent::Loaded {
                            book_id,
                            pages: sd_library.advertised_page_count(),
                            chapters: sd_library.chapter_count_for_ui(),
                            current_chapter: sd_library.current_chapter(),
                            chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                            position,
                        });
                        open.announced();
                    }
                    OpenAction::Done => break,
                }
            }
            if let Some(ram_hit) = section_loaded {
                if !ram_hit {
                    // Also the bench harness's legacy parse of a completed open.
                    esp_println::println!(
                        "storage: open complete status={:?} pages={} chapters={}",
                        sd_library.reader_status(),
                        sd_library.advertised_page_count(),
                        sd_library.chapter_count_for_ui()
                    );
                }
                esp_println::println!(
                    "bench: storage_open request={} book_id={} index={} ram_hit={} elapsed_ms={} status={:?} pages={} chapters={}",
                    request_id,
                    book_id,
                    index,
                    ram_hit,
                    storage_start.elapsed().as_millis(),
                    sd_library.reader_status(),
                    sd_library.advertised_page_count(),
                    sd_library.chapter_count_for_ui(),
                );
            }
        }
        StorageCommand::LoadChapters {
            request_id,
            book_id,
            index,
        } => {
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            // The overview opens with the cursor on the current chapter, so
            // center the first TOC window there.
            let ok = reader_cache::load_chapters_into_store(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                sd_library.current_chapter() as usize,
            );
            esp_println::println!(
                "storage: chapters loaded book_id={} ok={} count={}",
                book_id,
                ok,
                sd_library.overview_chapter_count()
            );
            // Re-render the overview with the full list resident, syncing the
            // selection range to the full chapter count. The reader has not
            // moved, so the app's own page stands.
            send_loaded_library_event(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                position: None,
            });
        }
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            type_settings,
            portrait,
        } => {
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            sd_library.set_layout(type_settings, portrait);
            // The TOC is still in the buffer; resolve the chapter's start page
            // before loading the section overwrites it. Re-ensure the window
            // covers the selection in case it slid since the overview render.
            reader_cache::ensure_toc_window(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter as usize,
                portrait,
            );
            let target_page = sd_library.overview_page_at(chapter as usize);
            let scratch = ensure_epub_scratch(epub_scratch);
            reader_cache::build_or_load_book_cache(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter,
                target_page as usize,
                scratch,
                font_metrics,
            );
            // The page came from the on-disk TOC, not from the app, so it
            // rides with the load rather than following as a second event.
            send_loaded_library_event(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                position: Some(target_page as u32),
            });
        }
        StorageCommand::ReceiveUpload => {
            // Handled in the task loop before dispatch; reaching here means
            // the loop refused it already.
        }
        StorageCommand::StoreWifiCredentials(credentials) => {
            let record = proto::nvm::WifiCredentialsRecord {
                ssid: credentials.ssid,
                ssid_len: credentials.ssid_len,
                password: credentials.password,
                password_len: credentials.password_len,
            };
            let written = reader_cache::store_wifi_credentials(epd, sd_cs, record);
            // Reacquire the card and use the exact boot-time read path before
            // telling the portal it may show success. This proves the record
            // survived handle/volume closure, closing the race where the
            // portal's success page beat a write that never actually landed
            // and the session-ending reset lost the credentials.
            let confirmed = written
                && reader_cache::load_wifi_credentials(epd, sd_cs)
                    .is_some_and(|stored| stored == record);
            esp_println::println!(
                "storage: wifi credentials written={} confirmed={}",
                written,
                confirmed
            );
            let _ = crate::WIFI_STORAGE_RESULTS.try_send(confirmed);
        }
        StorageCommand::ForgetWifiCredentials => {
            let forgotten = reader_cache::forget_wifi_credentials(epd, sd_cs);
            esp_println::println!("storage: wifi credentials forgotten={}", forgotten);
        }
        StorageCommand::StoreProgress(record) => {
            let record = record_for_persisted(sd_library, record);
            // Coalesce same-context page turns; anything beyond the screen
            // number changing (book, chapter, orientation, policy) is rare
            // and worth landing immediately. A pending record for the same
            // book is superseded by the new one; only a different book's
            // pending position must be preserved first.
            let context_changed = pending_progress
                .map(|pending| {
                    AppStateRecord {
                        screen: record.screen,
                        ..pending
                    } != record
                })
                .unwrap_or(false);
            let due = last_progress_write
                .map(|written| written.elapsed().as_secs() >= PROGRESS_WRITE_MIN_SECS)
                .unwrap_or(true);
            if pending_progress
                .map(|pending| pending.book_id != record.book_id)
                .unwrap_or(false)
                && !flush_pending_progress(
                    epd,
                    sd_cs,
                    sd_library,
                    pending_progress,
                    last_progress_write,
                )
            {
                // The other book's position couldn't land; overwriting the
                // pending record now would silently discard it.
                esp_println::println!(
                    "storage: progress context switch deferred after write failure"
                );
                return;
            }
            if context_changed || due {
                let progress_start = Instant::now();
                let stored = reader_cache::store_app_state(epd, sd_cs, sd_library, record);
                if stored {
                    *pending_progress = None;
                    *last_progress_write = Some(Instant::now());
                } else {
                    *pending_progress = Some(record);
                }
                esp_println::println!(
                    "bench: storage_progress action=write ok={} book_id={} page={} elapsed_ms={} t_ms={}",
                    stored,
                    record.book_id,
                    record.screen,
                    progress_start.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
            } else {
                *pending_progress = Some(record);
                esp_println::println!(
                    "bench: storage_progress action=coalesce book_id={} page={} t_ms={}",
                    record.book_id,
                    record.screen,
                    Instant::now().as_millis(),
                );
            }
        }
    }
}

fn send_required_library_event(event: &LibraryEvent) {
    const RETRIES: usize = 8;
    for _ in 0..RETRIES {
        if LIBRARY_EVENTS.try_send(*event).is_ok() {
            return;
        }
        let _ = LIBRARY_EVENTS.try_receive();
    }
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: required library event queue full");
    }
}

fn send_loaded_library_event(event: &LibraryEvent) {
    if DISPLAY_EVENTS
        .try_send(DisplayEvent::Library(*event))
        .is_ok()
    {
        return;
    }
    send_required_library_event(event);
}

/// Power acknowledgements get a bounded-wait send instead of a silent
/// try_send drop: the power task's sleep handshake blocks on the matching
/// `DisplayAsleep`/`DisplaySleepFailed`, and losing one on a momentarily
/// full queue would leave the MCU awake behind a dark panel until the next
/// input. The wait must stay bounded rather than fully blocking — the power
/// task stops draining `POWER_EVENTS` while it is itself blocked sending a
/// Sleep command into a full `DISPLAY_COMMANDS` queue, which only this task
/// drains, so an unbounded send here could deadlock both tasks. In that
/// window the acks being sent are refresh acks the power task ignores, so
/// timing out and logging the drop is safe; sleep acks are only sent after
/// the power task's Sleep send completed, when it is back in its receive
/// loop and drains the queue within the bound.
async fn send_required_power_event(event: PowerEvent) {
    if embassy_time::with_timeout(
        embassy_time::Duration::from_millis(20),
        POWER_EVENTS.send(event),
    )
    .await
    .is_err()
    {
        esp_println::println!("display: power event queue full, dropped {:?}", event);
    }
}

fn send_required_display_event(event: &DisplayEvent) {
    const RETRIES: usize = 8;
    for _ in 0..RETRIES {
        if DISPLAY_EVENTS.try_send(*event).is_ok() {
            return;
        }
        if let Ok(DisplayEvent::Library(library_event)) = DISPLAY_EVENTS.try_receive() {
            send_required_library_event(&library_event);
        }
    }
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: required display event queue full");
    }
}

/// Step one of a book-open transaction: get the departing book's page onto
/// the card, and clear anything the coalescer was still holding for it.
///
/// Returns whether the open may proceed. A refusal leaves the reader entirely
/// on the old book, with that book's position still owed and retried by the
/// next flush — there is no half-applied switch to reconcile later, which is
/// what lets the pending state stay a single latest-value slot.
fn close_out_departing_book(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    previous: PersistedAppState,
) -> bool {
    // A coalesced record for another book still has to land: it names that
    // book in the global state file, and this transaction is about to point
    // that file somewhere else.
    if pending_progress.is_some_and(|pending| pending.book_id != previous.book_id)
        && !flush_pending_progress(
            epd,
            sd_cs,
            sd_library,
            pending_progress,
            last_progress_write,
        )
    {
        return false;
    }
    let record = record_for_persisted(sd_library, previous);
    let start = Instant::now();
    let stored = reader_cache::store_book_position(epd, sd_cs, sd_library, record);
    esp_println::println!(
        "bench: store_book_position ok={} book_id={} page={} elapsed_ms={} t_ms={}",
        stored,
        record.book_id,
        record.screen,
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    if stored {
        // Whatever the coalescer held for this book is now on the card, and
        // the global half of it is about to be rewritten by step three.
        *pending_progress = None;
    } else {
        // Keep it owed so the next flush retries it; the reader is staying on
        // this book, so the record is still the right one to write.
        *pending_progress = Some(record);
    }
    stored
}

/// Kept out of line: first-call initialization moves a multi-KB scratch
/// value into the static; that spike must not sit at the base of the EPUB
/// open call chain's frame.
#[inline(never)]
fn ensure_epub_scratch<'a>(
    epub_scratch: &'a mut Option<&'static mut ReaderCacheScratch<'static>>,
) -> &'a mut ReaderCacheScratch<'static> {
    if epub_scratch.is_none() {
        esp_println::println!("storage: init epub scratch");
        *epub_scratch = Some(EPUB_SCRATCH.init_with(|| {
            ReaderCacheScratch::new(
                EPUB_TAIL.take(),
                EPUB_HEADER.take(),
                EPUB_NAME.take(),
                EPUB_COMPRESSED.take(),
                EPUB_CONTAINER.take(),
                EPUB_OPF.take(),
                EPUB_XHTML.take(),
                EPUB_BOOK_SECTIONS.take(),
            )
        }));
    }
    epub_scratch.as_deref_mut().unwrap()
}

fn source_identity(library: &ReaderStore, book_id: u32) -> (u32, u32) {
    library.source_identity(book_id)
}

/// The on-card record for a state the app persisted, with the fields only the
/// firmware knows filled in.
///
/// The reducer derives chapter from the 128-capped `sd_chapter_for_page`, so a
/// deep position would save a stuck chapter that the sleep/boot colophon then
/// shows wrong until the book reopens. The firmware tracks the true chapter
/// over the whole book; adopt it for the loaded SD book so saved and restored
/// state name the chapter right.
fn record_for_persisted(library: &ReaderStore, state: PersistedAppState) -> AppStateRecord {
    let (source_hash, source_size) = source_identity(library, state.book_id);
    let chapter = if ReaderSource::from_book_id(state.book_id).is_sd()
        && library.loaded_index == ReaderStore::selected_book_index(state.book_id)
    {
        library.current_chapter()
    } else {
        state.chapter
    };
    AppStateRecord {
        book_id: state.book_id,
        chapter,
        screen: state.screen,
        shell_orientation: state.shell_orientation,
        reading_orientation: state.reading_orientation,
        refresh_policy: state.refresh_policy,
        font_size: state.font_size,
        line_spacing: state.line_spacing,
        font_weight: state.font_weight,
        font_family: state.font_family,
        front_buttons: state.front_buttons,
        source_hash,
        source_size,
    }
}

/// Where the reader is in the book at `index`.
///
/// The book's own position file is authoritative. It is written when that book
/// is left and read when it is opened, so it can only ever describe this book —
/// which is the whole point of keeping it: the global state record is a single
/// slot that names one book, and reading position out of it is what let a stale
/// record hand one book's page to another.
///
/// The record's own `chapter`/`screen` are a mirror, still written so MarigoldOS
/// (which reads position from the global file) keeps resuming from cards this
/// firmware wrote. They are consulted only when the per-book file is missing or
/// fails its checksum, and they are safe in that role because the identity that
/// selected this book came from the very same record.
fn book_position(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    index: u16,
    mirror: AppStateRecord,
) -> (u16, u32) {
    match reader_cache::load_position(epd, sd_cs, library, usize::from(index)) {
        Some(position) => position,
        None => {
            esp_println::println!(
                "restore: no per-book position for index={}; using the global mirror",
                index
            );
            (mirror.chapter, mirror.screen)
        }
    }
}

/// One boot-time attempt to map durable reader state back onto the scanned
/// catalog by stable source identity (path hash + byte size) and hand the
/// saved position to the app as a `Restored` event. The volatile book id
/// stored in the record is never trusted directly.
fn restore_saved_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    state_restored: &mut bool,
) {
    if *state_restored || library.catalog_is_empty() {
        return;
    }
    *state_restored = true;
    let Some(record) = reader_cache::load_app_state(epd, sd_cs) else {
        esp_println::println!("restore: no usable durable state");
        return;
    };
    let hint = ReaderSource::from_book_id(record.book_id).sd_index();
    let Some(index) = crate::library_sd::find_index_by_identity(
        epd,
        sd_cs,
        record.source_hash,
        record.source_size,
        hint,
    ) else {
        esp_println::println!(
            "restore: no catalog match hash={:08x} size={}",
            record.source_hash,
            record.source_size
        );
        return;
    };
    // Stage the restored book's catalog entry so the position, colophon, and
    // page-count reads below resolve it, and so the first Home paint names it
    // before any open.
    crate::library_sd::load_active_entry(epd, sd_cs, library, usize::from(index));
    let (chapter, screen) = book_position(epd, sd_cs, library, index, record);
    esp_println::println!(
        "restore: index={} chapter={} screen={}",
        index,
        chapter,
        screen
    );
    // Resolve the chapter title now so wake-to-Home (rendered before the book
    // is opened) names the chapter; without this the colophon shows a numeral
    // until the book is first opened this session.
    reader_cache::load_chapter_title(epd, sd_cs, usize::from(index), chapter, library);
    // The book's total page count, so the Home progress bar has a denominator
    // on wake before the book is opened (read from the cache index header).
    let page_count = reader_cache::restore_book_page_count(epd, sd_cs, usize::from(index), library);
    send_required_library_event(&LibraryEvent::Restored {
        book_id: ReaderSource::sd(index).book_id(),
        chapter,
        page: screen,
        page_count,
        reading_orientation: record.reading_orientation,
        refresh_policy: record.refresh_policy,
        font_size: record.font_size,
        line_spacing: record.line_spacing,
        font_weight: record.font_weight,
        font_family: record.font_family,
        front_buttons: record.front_buttons,
    });
}

fn sleep_request_from_saved_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    pending_progress: &Option<AppStateRecord>,
) -> Option<RenderRequest> {
    // A coalesced record is state the card has not seen yet, so it outranks
    // both stored copies; only a record read back from the card has to defer
    // to the book's own position file.
    let (record, unflushed) = match *pending_progress {
        Some(record) => (record, true),
        None => (reader_cache::load_app_state(epd, sd_cs)?, false),
    };
    let hint = ReaderSource::from_book_id(record.book_id).sd_index();
    let index = crate::library_sd::find_index_by_identity(
        epd,
        sd_cs,
        record.source_hash,
        record.source_size,
        hint,
    )?;
    crate::library_sd::load_active_entry(epd, sd_cs, library, usize::from(index));
    let (chapter, screen) = if unflushed {
        (record.chapter, record.screen)
    } else {
        book_position(epd, sd_cs, library, index, record)
    };
    reader_cache::load_chapter_title(epd, sd_cs, usize::from(index), chapter, library);
    let page_count = reader_cache::restore_book_page_count(epd, sd_cs, usize::from(index), library);
    Some(RenderRequest {
        kind: RenderKind::Page,
        view: AppView::Home,
        page: screen,
        page_count,
        chapter,
        selection: 0,
        book_id: ReaderSource::sd(index).book_id(),
        orientation: display_orientation_from_u8(record.reading_orientation)
            .unwrap_or(DisplayOrientation::LandscapeButtonsBottom),
        front_buttons: app_core::front_buttons_from_u8(record.front_buttons)
            .unwrap_or(app_core::FrontButtons::PagesRight),
        reading_sheet: false,
        refresh_policy: refresh_policy_from_u8(record.refresh_policy)
            .unwrap_or(app_core::RefreshPolicy::FullOnWake),
        font_size: display::font::FontSize::from_u8(record.font_size)
            .unwrap_or(display::font::FontSize::Medium),
        line_spacing: display::font::LineSpacing::from_u8(record.line_spacing)
            .unwrap_or(display::font::LineSpacing::Normal),
        font_weight: display::font::FontWeight::from_u8(record.font_weight)
            .unwrap_or(display::font::FontWeight::Normal),
        font_family: display::font::FontFamily::from_u8(record.font_family)
            .unwrap_or(display::font::FontFamily::Literata),
        last_button: None,
        aux_raw: 0,
        nav_raw: 0,
        page_raw: 0,
        battery_mv: 0,
        battery_percent: 100,
        library_count: library.catalog_count_u16(),
        sync_status: SyncStatus::NotConfigured,
        wifi_ssid: [0; 32],
        wifi_ssid_len: 0,
        dirty: display::Rect::FULL,
    })
}

fn flush_pending_progress(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
) -> bool {
    if let Some(record) = *pending_progress {
        let start = Instant::now();
        let stored = reader_cache::store_app_state(epd, sd_cs, sd_library, record);
        if stored {
            *pending_progress = None;
            *last_progress_write = Some(Instant::now());
        }
        esp_println::println!(
            "bench: storage_progress action=flush ok={} book_id={} page={} elapsed_ms={} t_ms={}",
            stored,
            record.book_id,
            record.screen,
            start.elapsed().as_millis(),
            Instant::now().as_millis(),
        );
        stored
    } else {
        true
    }
}
