use crate::book_build::{self, ReaderCacheScratch};
use crate::display_flush::{self, Epd};
use crate::{
    DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, StorageCommand, DISPLAY_COMMANDS,
    DISPLAY_EVENTS, LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS,
};
use app_core::storage_loop::{
    loop_arm, owed_work, owed_work_delay_ms, storage_may_run, Drained, LoopArm, OpenAction,
    OpenSequence, OwedWork, SleepAction, SleepRefusal, SleepSequence,
};
use app_core::{
    display_orientation_from_u8, refresh_policy_from_u8, AppView, ChapterCursor,
    DisplayEventHolder, DisplayHoldOutcome, DisplayOrientation, EvictionStep, EvictionWalk,
    HoldOutcome, LibraryEventHolder, ReaderSource, RefreshPlanner, RenderKind, RenderRequest,
    SyncSession, SyncStatus,
};
use core::cell::Cell;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll, Waker};
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use embassy_futures::select::{select, select5, Either, Either5};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use proto::nvm::AppStateRecord;
use reader_cache::store::{ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS};
use reader_cache::{
    READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH, READER_HEADER_SCRATCH, READER_OPF_SCRATCH,
    READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use static_cell::ConstStaticCell;
use storage::task::OwedRescan;

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
// Const-constructed like every other scratch above, which is the whole point
// of `ZipInflateScratch`'s shape: it is ~32 KB against a 42 KB stack, so it
// must reach `.bss` without ever being built as a value. A `StaticCell` here
// meant writing one through a pointer, and that left a ~21 KB frame.
static EPUB_ZIP_INFLATE: ConstStaticCell<proto::epub::ZipInflateScratch> =
    ConstStaticCell::new(proto::epub::ZipInflateScratch::new());
// Separate from the scratch above, and deliberately: miniz builds this ~10 KB
// decoder only by value, so holding it *inside* the const scratch would make
// that const no longer provably all-zero and push the whole 43 KB out of
// `.bss` and into `.data` — 43 KB of flash, copied at every boot. An uninit
// `StaticCell` stays in `.bss` and pays for the value once, here.
static EPUB_DECOMPRESSOR: static_cell::StaticCell<proto::epub::DecompressorOxide> =
    static_cell::StaticCell::new();
static EPUB_SCRATCH: static_cell::StaticCell<ReaderCacheScratch<'static>> =
    static_cell::StaticCell::new();

/// Own panel and card I/O, servicing display commands before deferred rescans
/// and book-build slices. Sleep refuses any pick still waiting on a rescan.
///
/// `deep_sleep_wake` means the panel retained a settled sleep image;
/// `probe_diag` is the boot probe report to write to the card.
#[embassy_executor::task]
pub async fn run(
    mut epd: Epd,
    mut sd_cs: Output<'static>,
    deep_sleep_wake: bool,
    probe_diag: hal_ext::epd_probe::ProbeDiag,
) {
    esp_println::println!("display: started t_ms={}", Instant::now().as_millis());

    static FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB.init(Framebuffer::new());
    // The previous-frame buffer sits in dram2 so the radio's statics fit
    // in main DRAM; same exclusive &'static mut as the old local cell.
    let prev_fb = crate::sync_mem::take_prev_fb().expect("prev_fb claimed once");

    // Storage-command admission for the sync session lifecycle; the loan
    // transition and refusal rules live in app-core with the contracts.
    let mut sync_session = SyncSession::default();
    // Storage state: the background walk, the evidence job, the pending
    // progress record and the restore latch. See `storage::task`.
    let mut storage_task = storage::task::StorageTask::default();
    // A pick whose rescan the loop still owes: announced and painted, run by
    // the loop's own branch behind any frame queued meanwhile. One byte.
    let mut owed_rescan: Option<OwedRescan> = None;
    // On a deep-sleep (Power button) wake the panel still shows the sleep
    // screen: deep_sleep_wake is true only when the RTC wake cause is the
    // armed GPIO *and* the pre-sleep handshake recorded that the sleep frame
    // settled on the panel (see sleep_marker). The seeded planner then picks
    // the ~1.5 s one-flicker FastClean for the wake render instead of the
    // ~3.5 s multi-flash Full. Any other boot — battery pull, crash, software
    // reset, or a sleep whose final flush failed — leaves the seed false and
    // keeps the full waveform for unknown panel contents.
    let mut refresh_planner = RefreshPlanner::new().with_panel_shows_sleep_screen(deep_sleep_wake);
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
        Ok(outcome) if outcome.needs_reset() => {
            esp_println::println!("display: {:?}; resetting", outcome);
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
            esp_hal::system::software_reset();
        }
        Ok(_) => {}
        Err(e) => esp_println::println!("display: update check skipped: {:?}", e),
    }

    // Park the boot probe's verdict on the card. It goes here, on the storage
    // task, because SD access lives behind this task's shared SPI bus — and
    // after the update check, so a boot that is about to reflash and reset
    // does not spend a card write on a report the new image will rewrite. The
    // card is warm by now, so this is a file write, not another cold acquire.
    crate::probe_report::write(&mut epd, &mut sd_cs, &probe_diag);

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
        // Three ways to make progress, and the third is why the first two are
        // not enough on their own. A settling event with no room in the
        // channel is held here rather than dropped, and placing it means
        // waiting for the app task to drain — which the app task cannot do
        // while it is blocked handing this task a render. So the wait for
        // room runs *beside* the display queue, not in front of it: servicing
        // a render is what releases the consumer that frees the slot.
        //
        // Storage stands down while something is held. It is the producer of
        // settling events, the holder has one slot, and a second command
        // could fill it with nowhere for the first to go.
        //
        // The fourth is the same waiting-for-room branch for a render
        // acknowledgement, and it constrains nothing: rendering is what
        // produces acknowledgements, but it is also what releases the app to
        // drain them, so standing down would be waiting on itself.
        //
        // The fifth is the only one nobody else is waiting on: work this task
        // already owes itself, so it comes last on purpose and runs when the
        // four above have nothing. That is a pick's rescan once its note is
        // painted, or else a slice of a suspended book build. The rescan is
        // not run straight after its note: the note's flush yields, and a
        // Back or Power pressed in that window has its render or sleep queued
        // by the time the plate settles, so the 12 s scan must come after it.
        // Storage stands down while a rescan is owed, since it is one
        // command's second half. The branch waits before claiming the loop;
        // a bare yield hands the app task one poll, which does not cover
        // receiving a button, reducing it and sending the render. See
        // `owed_work_due`.
        let due = owed_work(
            owed_rescan.is_some(),
            storage_task.background_owed(sd_library) && !sd_library.text_holds_toc(),
        )
        .filter(|_| !sync_session.active() && holder().storage_may_run());
        match select5(
            DISPLAY_COMMANDS.receive(),
            storage_command_while_free(owed_rescan.is_some()),
            place_held_library_event(),
            place_held_display_event(),
            owed_work_due(
                due,
                // A place the card refused backs off on the same curve a
                // refused build does. Without it the retry runs at the settle
                // interval, which is 50 ms of cache work against a card that
                // is saying no.
                storage_task.background_attempts(),
            ),
        )
        .await
        {
            Either5::Fifth(()) => match due {
                Some(OwedWork::Rescan) => {
                    if let Some(owed) = owed_rescan.take() {
                        let portrait = last_portrait(&refresh_planner);
                        // The scan lends the bus back to the panel to show
                        // its progress, from inside its card session.
                        let mut painter = RescanPainter {
                            fb,
                            prev_fb,
                            refresh_planner: &mut refresh_planner,
                            prev_prestaged: &mut prev_prestaged,
                            waker: own_waker().await,
                        };
                        storage_task.rescan(
                            owed,
                            &mut crate::sd_session::card_reporting(
                                &mut epd,
                                &mut sd_cs,
                                &mut painter,
                            ),
                            &mut FwHost,
                            sd_library,
                            portrait,
                        );
                    }
                }
                Some(OwedWork::BuildSlice) => {
                    storage_task.background_step(
                        &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                        &mut FwHost,
                        sd_library,
                        font_metrics,
                        refresh_planner.last_request(),
                    );
                }
                // The branch is pending with nothing owed.
                None => {}
            },
            Either5::Third(()) | Either5::Fourth(()) => {}
            Either5::First(DisplayCommand::Render(mut request)) => {
                // The dequeue instant. This is NOT the pairing boundary --
                // `request.requested_at_ms` is, stamped by the app as it froze
                // the state. A render can sit in the channel behind a flush, a
                // prestage, a storage command or a background build step, so
                // the two differ by however long this task was busy, and a
                // press arriving in that gap belongs to the *next* frame.
                // Reported as `deq_ms` purely so that queue wait is visible:
                // `deq_ms - req_ms` is the delay, and it was invisible before.
                let dequeued_at_ms = Instant::now().as_millis();
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
                        // The list is the folder the reader is in, not a
                        // window over the flat catalog: slide the page over
                        // the rows this render will show.
                        crate::library_sd::ensure_folder_page(
                            &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                            sd_library,
                            request.selection,
                            app_core::is_portrait(request.orientation),
                        );
                    } else if ReaderSource::from_book_id(request.book_id).is_sd() {
                        if let Some(index) = ReaderStore::selected_book_index(request.book_id) {
                            if content_context_changed {
                                crate::library_sd::load_active_entry(
                                    &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                                    sd_library,
                                    index,
                                );
                            }
                            // Long TOCs are windowed like the catalog; slide
                            // the window over the rows this render will show.
                            if request.view == AppView::Chapters && sd_library.text_holds_toc() {
                                book_build::ensure_toc_window(
                                    &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                                    sd_library,
                                    index,
                                    request.selection as usize,
                                    app_core::is_portrait(request.orientation),
                                );
                            }
                        }
                    }
                }
                // The one place the walk's progress enters a frame. A plate
                // repainting this frame keeps what it drew; see
                // `RefreshPlanner::reading_plate_frame`. Set in place: a
                // second request here would be another 120 bytes held across
                // this task's awaits.
                if request.view == AppView::Reading {
                    request.footer_percent = storage_task
                        .build_progress(request.book_id)
                        .map(proto::progress::JobProgress::percent);
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

                // Panel init: true for a boot's first render (fresh planner)
                // and again after any display sleep — record_sleep clears
                // last_request, which also covers the aborted-sleep path where
                // a late button press interrupts the handshake after the panel
                // already powered down. The Sleep arm applies the same rule
                // before its own flush.
                if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                    esp_println::println!("display: wake init start");
                    if let Err(error) = display_flush::init_panel(&mut epd).await {
                        // The panel never came up; flushing into it would
                        // stream into a dead controller. Fail this render —
                        // the app clears its render lock and the next
                        // request retries init from scratch.
                        esp_println::println!("display: wake init failed: {:?}", error);
                        prev_prestaged = false;
                        let (display_event, power_event) =
                            app_core::display_refresh_outcome(false, None);
                        send_display_event(&display_event);
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
                // A frame the panel already shows costs 435 ms to send and
                // changes nothing. The compare early-exits on the first
                // differing byte, so the ordinary case is 14 to 16 us and
                // only a match pays the full scan, measured at 4.6 ms.
                //
                // Restricted to `Fast` so a deliberate ghost-clearing pass
                // still runs. `last_request` must be set, which a failed
                // flush and a sleep both clear, so the panel cannot disagree
                // with `prev_fb` here.
                let skipped = refresh_planner.screen_on()
                    && refresh_planner.last_request().is_some()
                    && mode == RefreshMode::Fast
                    && fb.bytes() == prev_fb.bytes();
                let flush_start = Instant::now();
                let flushed = if skipped {
                    Ok(display_flush::PanelSettle::from_ms(0))
                } else {
                    display_flush::flush(
                        &mut epd,
                        fb,
                        prev_fb,
                        refresh_planner.screen_on(),
                        mode,
                        prev_prestaged,
                    )
                    .await
                };
                if let Ok(settle) = flushed {
                    let flush_ms = flush_start.elapsed().as_millis();
                    if skipped {
                        refresh_planner.record_skipped_render(request);
                    } else {
                        refresh_planner.record_render(request, mode);
                        prev_fb.copy_from(fb);
                    }
                    // Keep the current chapter tracking the page just shown, past
                    // the reducer's 128-chapter cap. Cheap in-RAM check; only the
                    // loaded SD reader has an uncapped page map, so this no-ops on
                    // other views and reads SD only when the chapter changes. It
                    // rides out inside Settled: the app must apply it before it
                    // clears the render lock, and one message is the only way to
                    // promise that (see DisplayEvent::Settled).
                    let chapter_cursor = if request.view == AppView::Reading {
                        book_build::track_reading_chapter(
                            &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                            request.page,
                            sd_library,
                        )
                        .map(|current_chapter| ChapterCursor {
                            book_id: request.book_id,
                            page: request.page,
                            current_chapter,
                        })
                    } else {
                        None
                    };
                    // Settle before the ~23 ms RED prestage: the panel is visually
                    // done, so unblock the input/power pipeline. The prestage still
                    // runs on this task before the next command is dequeued, so
                    // `prev_prestaged` is always current by the next flush, and a
                    // Sleep queued by power_task after DisplaySettled waits behind it.
                    let (display_event, power_event) =
                        app_core::display_refresh_outcome(true, chapter_cursor);
                    let settled_at_ms = Instant::now().as_millis();
                    send_display_event(&display_event);
                    send_required_power_event(power_event).await;
                    // Emitted here, at the settle, and not after the prestage
                    // below. This timestamp is what the bench pairs each input
                    // against, so printing it later charged the reader for a
                    // write they never waited on: `Settled` has already gone
                    // out, and press-to-settled ends on this line.
                    bench_log!(
                        "bench: render view={:?} mode={:?} page={} chapter={} layout_ms={} flush_ms={} req_ms={} deq_ms={} t_ms={} skipped={}",
                        request.view,
                        mode,
                        request.page,
                        request.chapter,
                        layout_ms,
                        flush_ms,
                        request.requested_at_ms,
                        dequeued_at_ms,
                        settled_at_ms,
                        skipped,
                    );
                    // The clean plans' settle, deferred out of the flush so it
                    // falls after `Settled`. It guards the prestage below and
                    // nothing else, and the chapter read above now sits inside
                    // it rather than after it, so the gap cannot shrink while
                    // 200 ms comes off press-to-settled. Zero on other plans.
                    settle.wait().await;
                    // Skipped renders own none of this: nothing wrote panel
                    // RAM, so `prev_prestaged` is still true of the frame it
                    // was true of, and staging again would copy a frame the
                    // panel already holds.
                    if !skipped {
                        let prestage_start = Instant::now();
                        // Runs for every frame that reached the panel, and a
                        // queued next render is no reason to defer it. This
                        // write is off the critical path here, with Settled
                        // already out, while the write it would defer lands
                        // inside the next Fast flush where the reader waits.
                        // `fast_plan_only_writes_previous_plane_when_not_prestaged`
                        // (display/src/epd/uc8253.rs) pins the asymmetry: an
                        // unstaged Fast carries an extra WritePlane(Old,
                        // Previous) + DataStop, and the X4 writes RED from
                        // `prev_fb` for the same reason
                        // (fw/src/display_flush/ssd1677.rs). Deferring is also
                        // self-sustaining, since each deferral leaves the next
                        // turn unstaged, so a held button would pay the write
                        // on-path every turn instead of off-path once.
                        //
                        // The skip above is the one case that owes nothing,
                        // and for the opposite reason: it wrote no panel RAM,
                        // so the staging still standing is still correct.
                        prev_prestaged =
                            display_flush::prestage_previous(&mut epd, fb).await.is_ok();
                        // Its own event, after the render one above: prestage is
                        // real work on this task and still gates the next command,
                        // but it sits outside press-to-settled and is measured
                        // separately so neither number can absorb the other.
                        bench_log!(
                            "bench: prestage staged={} elapsed_ms={} t_ms={}",
                            prev_prestaged,
                            prestage_start.elapsed().as_millis(),
                            Instant::now().as_millis(),
                        );
                    }
                } else {
                    esp_println::println!("display: SPI transfer failed");
                    prev_prestaged = false;
                    // The flush may have run partially, so the panel's RAM
                    // and waveform state no longer match the planner's model;
                    // forget it so the next render re-inits the panel and
                    // takes the full waveform instead of fast-diffing
                    // against a frame that may never have landed.
                    refresh_planner.record_failure();
                    let (display_event, power_event) =
                        app_core::display_refresh_outcome(false, None);
                    send_display_event(&display_event);
                    send_required_power_event(power_event).await;
                }
            }
            Either5::First(DisplayCommand::Sleep { generation }) => {
                let sleep_start = Instant::now();
                bench_log!(
                    "bench: sleep phase=requested screen_on={} t_ms={}",
                    refresh_planner.screen_on(),
                    sleep_start.as_millis(),
                );
                // A background build is deliberately *not* dropped here. Sleep
                // is terminal — waking is a fresh boot — so a walk that goes
                // down needs no clearing, and the book it left behind is a
                // valid partial cache whose frontier a later open rebuilds
                // past. A sleep that is refused, or a handshake the user
                // abandons, returns to the loop with the walk still standing,
                // which is what should happen: nothing about it was finished.
                //
                // It also stays out of the pre-sleep drain by construction.
                // The drain works the storage queue, and a background step is
                // not a queued command — it is a branch of the loop's select —
                // so it can never spend the drain's budget or delay the panel.
                //
                // A pick still waiting on its rescan is refused rather than
                // scanned: sleep is terminal, and the scan would hold the
                // note on the panel for 12 s ahead of the sleep image. The
                // refusal is a required event, so it is sent before the
                // holder is checked below.
                if let Some(owed) = owed_rescan.take() {
                    storage_task.abandon_rescan(owed, &mut FwHost);
                }
                // Everything owed to the card, in order, before the panel goes
                // down. The ordering rules live in `SleepSequence` so they can
                // be driven from a host test; this arm only does what it is
                // told and reports back what the hardware said.
                let mut sleep = SleepSequence::new(STORAGE_COMMANDS.capacity());
                // The main loop keeps storage shut while an event is held; this
                // drain applies storage commands too, so it owes the same rule.
                // Checked before the first take and after every applied
                // command, because either end can be where the holder fills:
                // sleep can arrive with one already waiting, or the first
                // command drained can produce it. Carrying on past that point
                // is how a second completion would reach an occupied holder.
                let mut may_keep_draining = holder().sleep_may_proceed();
                let refusal = loop {
                    if !may_keep_draining {
                        break None;
                    }
                    match sleep.next() {
                        SleepAction::TakeQueued => match STORAGE_COMMANDS.try_receive() {
                            Err(_) => sleep.queue_empty(),
                            Ok(command) => match sleep.drained(&command) {
                                Drained::Apply => {
                                    esp_println::println!("storage: draining before sleep");
                                    let portrait = last_portrait(&refresh_planner);
                                    let owed = storage_task.handle(
                                        command,
                                        &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                                        &mut FwHost,
                                        sd_library,
                                        font_metrics,
                                        &mut sync_session,
                                        portrait,
                                    );
                                    // A pick that would rescan is refused, as
                                    // one already owed is above: the panel is
                                    // about to show the sleep image.
                                    if let Some(owed) = owed {
                                        storage_task.abandon_rescan(owed, &mut FwHost);
                                    }
                                    sleep.applied();
                                    may_keep_draining = holder().sleep_may_proceed();
                                }
                                Drained::RequeueAndRefuse => {
                                    // This send cannot fail today: nothing
                                    // between the receive above and here
                                    // awaits, and no task at interrupt priority
                                    // sends storage commands, so no producer
                                    // can take the slot the command just
                                    // vacated. Its answer is still taken rather
                                    // than assumed, because the only way it
                                    // could fail is a producer having refilled
                                    // the queue — which changes what the drain
                                    // must do next, and the sequence needs to
                                    // know.
                                    sleep.requeued(STORAGE_COMMANDS.try_send(command).is_ok());
                                }
                            },
                        },
                        SleepAction::FlushProgress => {
                            let stored = storage_task.flush_pending_progress(
                                &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                                sd_library,
                            );
                            sleep.flushed(stored);
                        }
                        SleepAction::Refuse(refusal) => break Some(refusal),
                        SleepAction::Proceed => break None,
                    }
                };
                // A drained `ClearBookCache` can leave its completion held, and
                // sleep is terminal — waking is a fresh boot — so going down
                // now would take the event with it and strand the app in
                // `Busy`. Stay awake instead; the loop's placing branch runs
                // the moment this returns, the rest of the queue is still
                // there to drain, and the power task re-requests sleep after.
                let sleep_holds_an_event = refusal.is_none() && !holder().sleep_may_proceed();
                if sleep_holds_an_event {
                    esp_println::println!("display: sleep deferred; library event still held");
                }
                if refusal.is_some() || sleep_holds_an_event {
                    // Stay awake. The power task's idle clock re-requests sleep
                    // once this failure releases its handshake wait, by which
                    // time the upload session has run or the pending record has
                    // been retried by the next flush.
                    if let Some(refusal) = refusal {
                        match refusal {
                            SleepRefusal::UploadQueued => {
                                esp_println::println!(
                                    "display: sleep deferred; upload session pending"
                                )
                            }
                            // The request itself is gone, so the browser is left
                            // waiting on a writer that will not start and
                            // UPLOAD_SESSION_ACTIVE stays set. Nothing here can
                            // recover that. What this refusal does protect is the
                            // rest of the queue, which is full: the ordinary loop
                            // applies it before the next sleep attempt.
                            SleepRefusal::UploadLost => esp_println::println!(
                                "display: sleep deferred; upload request lost, storage queue full"
                            ),
                            SleepRefusal::ProgressUnwritten => esp_println::println!(
                                "display: sleep deferred; progress persistence failed"
                            ),
                        }
                    }
                    send_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                    continue;
                }
                // The render arm's init rule, for a sleep that comes before
                // any render brought the panel up: Power pressed inside the
                // deferred first paint after a wake, or a sleep straight after
                // a failed render forgot the panel. Flushing into a controller
                // that was never initialised fails on the X3 (BUSY never
                // asserts) and on the X4 (BUSY wait fails), and the sleep
                // goes down without its image or not at all.
                let panel_ready =
                    if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                        esp_println::println!("display: sleep init start");
                        prev_prestaged = false;
                        match display_flush::init_panel(&mut epd).await {
                            Ok(()) => true,
                            Err(error) => {
                                esp_println::println!("display: sleep init failed: {:?}", error);
                                false
                            }
                        }
                    } else {
                        true
                    };
                let request = refresh_planner.last_request().or_else(|| {
                    sleep_request_from_saved_state(
                        &mut epd,
                        &mut sd_cs,
                        sd_library,
                        &storage_task.pending_progress,
                    )
                });
                if let Some(request) = request {
                    crate::views::render_sleep(fb, request, sd_library);
                } else {
                    crate::views::render_sleep_blank(fb);
                }
                let flushed = if panel_ready {
                    display_flush::flush(
                        &mut epd,
                        fb,
                        prev_fb,
                        refresh_planner.screen_on(),
                        RefreshMode::Full,
                        prev_prestaged,
                    )
                    .await
                    .ok()
                } else {
                    None
                };
                let sleep_frame_settled = if let Some(settle) = flushed {
                    // Nothing writes panel RAM before the power-down below.
                    // `Full` owes zero; holding it keeps that the plan's fact
                    // rather than this call site's assumption.
                    settle.wait().await;
                    prev_fb.copy_from(fb);
                    bench_log!(
                        "bench: sleep phase=refresh ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    true
                } else {
                    esp_println::println!("display: sleep framebuffer flush failed");
                    bench_log!(
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
                    // Emitted before the acknowledgement, not after the park:
                    // `DisplayAsleep` releases the power task to cut power, and
                    // deep sleep is terminal, so a line printed past that point
                    // only ever reaches the capture on an abandoned handshake.
                    // That left `phase=complete ok=true` absent from every
                    // successful sleep — the marker `sleep-sync` counts cycles
                    // by and the report reads as a terminal sleep.
                    bench_log!(
                        "bench: sleep phase=complete ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    send_display_event(&DisplayEvent::Asleep);
                    send_required_power_event(PowerEvent::DisplayAsleep(generation)).await;
                    park_until_resumed(generation).await;
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
                    send_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                    // The failing path stays awake, so this one can be stamped
                    // where the phase actually ends.
                    bench_log!(
                        "bench: sleep phase=complete ok=false elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                }
            }
            Either5::Second(command) => match loop_arm(&command, sync_session) {
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
                    //
                    // Only for an open the handler will actually act on. The
                    // same begin() gate it applies drops a stale request here
                    // too -- otherwise a superseded open would spend a
                    // multi-second full flush painting a plate for a target the
                    // reader has already navigated past, then be skipped.
                    // A fenced-out open begins straight into its refusal, so
                    // this asks whether the sequence will actually stage a
                    // book rather than merely whether it begins: a plate for
                    // an open that is about to be refused would spend a
                    // multi-second flush on a book nobody opens.
                    let will_stage = OpenSequence::begin(
                        &command,
                        LATEST_READER_REQUEST_ID.load(Ordering::Relaxed),
                        sd_library.catalog_epoch(),
                    )
                    .is_some_and(|open| !matches!(open.next(), OpenAction::Refuse { .. }));
                    if refresh_planner.screen_on() && will_stage {
                        if let Some(loading_request) =
                            open_loading_plate_request(&command, sd_library, &refresh_planner)
                        {
                            crate::views::render(fb, loading_request, sd_library);
                            flush_plate(
                                &mut epd,
                                fb,
                                prev_fb,
                                &mut refresh_planner,
                                &mut prev_prestaged,
                                loading_request,
                            )
                            .await;
                        }
                    }
                    let portrait = last_portrait(&refresh_planner);
                    let owed = storage_task.handle(
                        command,
                        &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                        &mut FwHost,
                        sd_library,
                        font_metrics,
                        &mut sync_session,
                        portrait,
                    );
                    if let Some(owed) = owed {
                        // A pick of a book the catalog does not know yet. Its
                        // rescan holds the card, and with it the bus, for
                        // about 12 s on a large library, so say so first, and
                        // then leave the scan to the loop's own branch: the
                        // plate's flush yields, and a render or sleep queued
                        // meanwhile goes first. Storage stood down the moment
                        // the last one was owed, so none is pending here.
                        if let Some(note) = rescan_plate_request(&refresh_planner) {
                            crate::library_sd::ensure_folder_page(
                                &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                                sd_library,
                                note.selection,
                                portrait,
                            );
                            crate::views::render(fb, note, sd_library);
                            flush_plate(
                                &mut epd,
                                fb,
                                prev_fb,
                                &mut refresh_planner,
                                &mut prev_prestaged,
                                note,
                            )
                            .await;
                        }
                        owed_rescan = Some(owed);
                    }
                }
            },
        }
    }
}

/// Flush a plate already drawn into `fb`: a frame painted ahead of work that
/// blocks this task, which nobody waits on, so it owes no event.
/// An identical fast frame is skipped. A successful flush waits for panel
/// settling and updates the saved frame; a failed flush invalidates the
/// planner and prestage state without returning an error.
async fn flush_plate(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &mut Framebuffer,
    refresh_planner: &mut RefreshPlanner,
    prev_prestaged: &mut bool,
    request: RenderRequest,
) {
    let mode = refresh_planner.mode_for(request);
    // A plate is often the frame already on the glass: an extend of the
    // section in front of the reader repaints what they are looking at.
    // Measured 32 of 32 identical over two device runs, 435 ms each. Nobody
    // waits on the plate, so a skip owes no planner update either.
    if refresh_planner.screen_on()
        && refresh_planner.last_request().is_some()
        && mode == RefreshMode::Fast
        && fb.bytes() == prev_fb.bytes()
    {
        bench_log!(
            "bench: plate skipped=true t_ms={}",
            Instant::now().as_millis()
        );
    } else if let Ok(settle) = display_flush::flush(
        epd,
        fb,
        prev_fb,
        refresh_planner.screen_on(),
        mode,
        *prev_prestaged,
    )
    .await
    {
        // Held, not deferred like the render path's: no prestage follows a
        // plate, so there is no nearer owner for the interval than here.
        settle.wait().await;
        refresh_planner.record_render(request, mode);
        prev_fb.copy_from(fb);
        *prev_prestaged = false;
    } else {
        // The panel state is as unknown as after any failed flush: drop the
        // prestage claim and the planner's screen model.
        esp_println::println!("display: plate flush failed");
        *prev_prestaged = false;
        refresh_planner.record_failure();
    }
}

/// Repaints the rescan note's progress rule while the scan holds the card.
///
/// Called from inside the scan's card session, between card operations,
/// with the bus clocked for the panel and the card deselected. The rows
/// cannot be redrawn there (the scan has the catalog and the arena they
/// come from), so only the note and its rule are redrawn, over the frame on the
/// glass, and flushed on a fast refresh.
struct RescanPainter<'a> {
    fb: &'a mut Framebuffer,
    prev_fb: &'a mut Framebuffer,
    refresh_planner: &'a mut RefreshPlanner,
    prev_prestaged: &'a mut bool,
    /// This task's own waker, for polling the flush to completion.
    waker: Waker,
}

impl crate::sd_session::SessionPainter for RescanPainter<'_> {
    fn paint(&mut self, epd: &mut Epd, percent: u8) {
        let Some(request) = self.refresh_planner.rescan_progress_frame(percent) else {
            return;
        };
        let start = Instant::now();
        // The glass, which a fast refresh diffs against.
        self.fb.copy_from(self.prev_fb);
        ui::app_render::render_library_rescan_progress(self.fb, request);
        poll_to_completion(
            flush_plate(
                epd,
                self.fb,
                self.prev_fb,
                self.refresh_planner,
                self.prev_prestaged,
                request,
            ),
            &self.waker,
        );
        bench_log!(
            "bench: rescan_progress percent={} elapsed_ms={} t_ms={}",
            percent,
            start.elapsed().as_millis(),
            Instant::now().as_millis()
        );
    }
}

/// The waker of the task awaiting this, without yielding.
async fn own_waker() -> Waker {
    core::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await
}

/// Drive `future` to completion from synchronous code that holds the
/// executor anyway: the scan, which runs for seconds without yielding.
///
/// Polled with this task's own waker, since embassy-time files its timers
/// under the task a waker names and refuses any other. The spin lasts one
/// flush and replaces no work: nothing else can run until the scan returns.
/// The wakes it collects only poll the task once more after the scan.
fn poll_to_completion<F: core::future::Future>(future: F, waker: &Waker) -> F::Output {
    let mut future = core::pin::pin!(future);
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        core::hint::spin_loop();
    }
}

/// The Library frame to paint before a pick's rescan: the one on the glass,
/// with the note. None when the screen is off or shows something else.
fn rescan_plate_request(refresh_planner: &RefreshPlanner) -> Option<RenderRequest> {
    if !refresh_planner.screen_on() {
        return None;
    }
    let mut request = refresh_planner.last_request()?;
    if request.view != AppView::Library {
        return None;
    }
    request.library_move_pending = true;
    request.library_rescanning = true;
    Some(request)
}

/// Places the settling event [`send_required_library_event`] could not, once
/// the channel has room. Pending forever when nothing is held, so it can sit
/// in the main loop's select as a branch that only fires when it has work.
///
/// The sender itself cannot wait: it runs inside `handle_storage_command`,
/// which is synchronous and has to stay that way — it owns the SD session and
/// multi-KB scratch near the stack floor. Nor may the *loop* simply wait here
/// and nowhere else. The app task blocks on `DISPLAY_COMMANDS.send` to hand
/// over a render, so a display task doing nothing but waiting for library-
/// event room would be waiting on a consumer waiting on it. Selecting this
/// against the display queue is what breaks that: servicing the render
/// releases the app task, which returns to its own select and drains.
async fn place_held_library_event() {
    let Some(event) = holder().pending() else {
        return core::future::pending::<()>().await;
    };
    LIBRARY_EVENTS.send(event).await;
    // Nothing awaits between the send completing and this, so the holder
    // cannot be observed empty with the event still unsent, or cleared for a
    // send that a cancellation abandoned.
    let _ = with_holder(LibraryEventHolder::placed);
}

/// The next storage command, but only while nothing is held and no rescan is
/// owed.
///
/// Storage is where settling events come from and the holder has one slot, so
/// applying another command while one waits could produce a second with
/// nowhere to go. And an owed rescan is one command's second half; a second
/// pick applied under it would replace the pick it stands for. Pending forever
/// until both clear, which the loop's other branches are free to do meanwhile.
async fn storage_command_while_free(rescan_owed: bool) -> StorageCommand {
    if !storage_may_run(holder().storage_may_run(), rescan_owed) {
        return core::future::pending::<StorageCommand>().await;
    }
    STORAGE_COMMANDS.receive().await
}

/// Ready when the loop should run the work it owes itself: a pick's rescan, or
/// a slice of a suspended book build.
///
/// `None` stays pending forever. The caller excludes work while a settling
/// event is held or a sync session is active, and excludes build slices while
/// the Chapters overview holds the text arena.
///
/// The wait is what makes an otherwise always-ready branch safe to sit in a
/// `select`. Returning immediately would let this task run slice after slice
/// without handing the executor back — every other task starved for the length
/// of the whole build. A single yield was not enough either: measured on
/// device, one step ended and the next began 0.2 ms later, with a page turn
/// pressed 86 ms earlier still not reduced into a render, so the reader waited
/// out another 2912 ms step for a page that already existed. Waiting
/// `BACKGROUND_SETTLE_MS` gives the app room to produce that render, which the
/// first branch then services ahead of the next slice. The same window covers a
/// rescan: a Back pressed as its note finished painting becomes the Home frame
/// the loop paints before the scan.
///
/// A walk that is only retrying waits on its backoff instead, which is what
/// lets a step that never began be kept indefinitely rather than given up on.
async fn owed_work_due(work: Option<OwedWork>, attempts: u8) {
    let Some(work) = work else {
        return core::future::pending::<()>().await;
    };
    // Always a wait, never a bare yield. The two reasons differ but the failure
    // of yielding is the same in both: this branch is ready again the instant a
    // step ends, so a single poll is not enough for the app task to get its
    // work in, and the loop commits to another multi-second slice ahead of it.
    Timer::after(Duration::from_millis(owed_work_delay_ms(work, attempts))).await;
}

/// Holds the display task still from the moment the panel goes down until the
/// power task says the sleep was abandoned.
///
/// This is the whole guarantee that a slept panel keeps showing its sleep
/// image. A render is routinely queued behind the `Sleep` — the pre-sleep
/// storage drain provokes one itself, since applying a book open emits
/// `Loaded` and the app repaints on it, and the sleep frame's full-waveform
/// flush gives it seconds to arrive. Returning to the command loop with that
/// render waiting re-initialises the panel and paints a page over the sleep
/// image, racing the power cut. Nothing the loop could do with that render is
/// right: painting it is the bug, and answering it discards the repaint the
/// abandoning press is owed. So the task does not go back to the loop at all.
///
/// On the ordinary path this never returns — `enter_deep_sleep_button` is `!`
/// and the chip reboots on wake. It returns only when the press that abandoned
/// the handshake releases it, and then the queued render is still queued and
/// repaints, which is exactly what that press asked for.
///
/// The wait is bounded so a lost `DisplayAsleep` cannot freeze the device.
/// That acknowledgement is a 20 ms bounded send into a queue the power task is
/// already draining, so losing it should not happen; if it ever does, the power
/// task waits for an ack that will not come and an unbounded park here would
/// wait on a resume that will not come either, leaving both tasks stopped
/// behind a dark panel. Waking early risks repainting over the sleep image —
/// far better than a device that has to be reset.
async fn park_until_resumed(generation: u32) {
    // Deep sleep follows its acknowledgement within about one input poll tick
    // (the power task's wake-button handoff), so seconds here are already many
    // orders of margin.
    const ABANDONED_HANDSHAKE_CEILING_SECS: u64 = 5;
    // `Timer::at` rather than a remaining-time subtraction: discarding a stale
    // resume can put the deadline in the past, and `Instant - Instant` panics
    // there. A deadline already passed simply fires at once.
    let deadline =
        Instant::now() + embassy_time::Duration::from_secs(ABANDONED_HANDSHAKE_CEILING_SECS);
    loop {
        match select(
            crate::DISPLAY_RESUME.wait(),
            embassy_time::Timer::at(deadline),
        )
        .await
        {
            Either::First(resumed) if resumed == generation => {
                esp_println::println!("display: sleep abandoned; resuming");
                return;
            }
            // A resume left over from a sleep this task never parked for, e.g.
            // one whose panel handshake failed. Not ours; keep waiting.
            Either::First(stale) => {
                esp_println::println!("display: ignoring stale resume generation={}", stale)
            }
            Either::Second(_) => {
                esp_println::println!(
                    "display: no deep sleep or resume within {} s; releasing the panel",
                    ABANDONED_HANDSHAKE_CEILING_SECS
                );
                return;
            }
        }
    }
}

pub(crate) fn send_library_event(event: &LibraryEvent) {
    // An event that settles something the app is holding cannot go out the
    // lossy way: the work is done and will not be redone, so a dropped one
    // strands the wait. The event says which it is, so no call site has to.
    if event.must_be_delivered() {
        send_required_library_event(event);
        return;
    }
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
    refresh_planner: &RefreshPlanner,
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
    refresh_planner.reading_plate_frame(book_id, target_pages as u32, type_settings)
}

/// Queue an event the app is waiting on, making room if the channel is full.
///
/// It used to make room by taking whatever was at the front and throwing it
/// away, without looking at it — so delivering one settling event could
/// silently destroy another, which is the whole thing this function exists to
/// prevent. It walks the ring now; [`EvictionWalk`] owns which events may be
/// spent for the newcomer and is host-tested against a modelled channel.
fn send_required_library_event(event: &LibraryEvent) {
    if LIBRARY_EVENTS.try_send(*event).is_ok() {
        return;
    }
    let mut walk = EvictionWalk::new(app_core::LIBRARY_EVENT_SLOTS);
    while !walk.exhausted() {
        let Ok(head) = LIBRARY_EVENTS.try_receive() else {
            break;
        };
        match walk.inspect(&head) {
            EvictionStep::Discard => {
                // The refresh is spent; its slot is this event's. Nothing
                // else writes this channel, so the send cannot lose the race.
                if LIBRARY_EVENTS.try_send(*event).is_ok() {
                    return;
                }
                break;
            }
            EvictionStep::Requeue => {
                // One slot is free (the head came off), so this always lands.
                let _ = LIBRARY_EVENTS.try_send(head);
            }
        }
    }
    // Every slot holds something the app is waiting on, so nothing here may
    // be spent — but this event is awaited too, and dropping it would leave
    // its wait unanswerable. Hold it for `deliver_held_library_event`, which
    // retries once the app task has had a turn to drain.
    hold_library_event(event);
}

/// The settling event with nowhere to go, and the gates it closes while it
/// waits. [`LibraryEventHolder`] owns both, and is host-tested; this task
/// asks it rather than re-deciding at each of the four sites that must agree.
static HELD_LIBRARY_EVENT: Mutex<CriticalSectionRawMutex, Cell<LibraryEventHolder>> =
    Mutex::new(Cell::new(LibraryEventHolder::new()));

/// Reads the holder. `Copy`, so this is a snapshot — fine for the gates,
/// which only ever narrow: nothing but this task fills the holder, and the
/// one thing that empties it is this task's own placing branch.
fn holder() -> LibraryEventHolder {
    HELD_LIBRARY_EVENT.lock(Cell::get)
}

fn with_holder<T>(update: impl FnOnce(&mut LibraryEventHolder) -> T) -> T {
    HELD_LIBRARY_EVENT.lock(|cell| {
        let mut holder = cell.get();
        let outcome = update(&mut holder);
        cell.set(holder);
        outcome
    })
}

/// Keeps `event` until the channel has room, or says why it could not.
///
/// Both refusals end the same way — one last try for a slot that may have
/// freed since, and the drop reported if it has not. They are reported apart
/// because they mean different things about the code above: `NotSettling` is
/// a caller that routed a refresh here, `Occupied` is two settling events out
/// of one storage command.
fn hold_library_event(event: &LibraryEvent) {
    let outcome = with_holder(|holder| holder.hold(event));
    let refusal = match outcome {
        HoldOutcome::Held => return,
        HoldOutcome::NotSettling => "refresh routed to the holder",
        HoldOutcome::Occupied => "holder occupied",
    };
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: {}, dropped {:?}", refusal, event);
    }
}

/// Sends a library event down the display channel, falling back to its own
/// when that one is full.
///
/// The two channels reach the app independently, so this is a choice of route
/// and not of order — anything that must be ordered against a render
/// acknowledgement travels *inside* it (see `DisplayEvent::Settled`) rather
/// than relying on which queue it landed in. Only `Loaded` comes through here
/// now, and it is order-free: whichever way round it and the acknowledgement
/// arrive, the app folds it and renders.
///
/// The fallback routes by `must_be_delivered` like every other send. It used
/// to go straight to the required path, which let a refresh-only event take
/// the holder — and the holder being occupied is what stops the very next
/// `Settled` from making room for itself, so a droppable event could strand
/// the app's render lock.
fn send_loaded_library_event(event: &LibraryEvent) {
    if DISPLAY_EVENTS
        .try_send(DisplayEvent::Library(*event))
        .is_ok()
    {
        return;
    }
    send_library_event(event);
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

/// Sends a display event, by the rule the event itself carries.
///
/// The two sleep notifications take the lossy path. The handshake the power
/// task waits on goes over `POWER_EVENTS` beside each of them and the app only
/// logs these, so a dropped one costs the log line — and letting them compete
/// for room with an acknowledgement got the priority exactly backwards, since
/// an acknowledgement is what ends the app's render cycle.
fn send_display_event(event: &DisplayEvent) {
    if event.must_be_delivered() {
        send_required_display_event(event);
        return;
    }
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: display event queue full, dropped {:?}", event);
    }
}

/// Queues an acknowledgement the app is waiting on, holding it if the channel
/// is full.
///
/// This used to make room by walking the queue. That was wrong twice over: the
/// first version's `try_receive` dropped whatever its pattern did not match,
/// and the walk that replaced it had to requeue at the tail, reordering the
/// queue the app reads its acknowledgements from. The queue is left alone now
/// — [`DisplayEventHolder`] explains why nothing in it is worth spending — and
/// the acknowledgement waits for room instead.
fn send_required_display_event(event: &DisplayEvent) {
    if DISPLAY_EVENTS.try_send(*event).is_ok() {
        return;
    }
    let outcome = with_display_holder(|holder| holder.hold(event));
    let refusal = match outcome {
        DisplayHoldOutcome::Held => return,
        DisplayHoldOutcome::NotRequired => "refresh routed to the acknowledgement holder",
        // Both end the render cycle and the app clears its lock on either, so
        // the one already waiting answers for this one too.
        DisplayHoldOutcome::Occupied => "acknowledgement holder occupied",
    };
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: {}, dropped {:?}", refusal, event);
    }
}

/// The acknowledgement that had nowhere to go, waiting for room. Gates
/// nothing: see [`DisplayEventHolder`] for why refusing renders while one is
/// held would deadlock the very task that empties it.
static HELD_DISPLAY_EVENT: Mutex<CriticalSectionRawMutex, Cell<DisplayEventHolder>> =
    Mutex::new(Cell::new(DisplayEventHolder::new()));

fn display_holder() -> DisplayEventHolder {
    HELD_DISPLAY_EVENT.lock(Cell::get)
}

fn with_display_holder<T>(update: impl FnOnce(&mut DisplayEventHolder) -> T) -> T {
    HELD_DISPLAY_EVENT.lock(|cell| {
        let mut holder = cell.get();
        let outcome = update(&mut holder);
        cell.set(holder);
        outcome
    })
}

/// Places the acknowledgement [`send_required_display_event`] could not, once
/// the channel has room. Pending forever when nothing is held, so it can sit
/// in the main loop's select as a branch that only fires when it has work.
///
/// Selecting this against the display queue is what keeps it from deadlocking:
/// the app blocks on `DISPLAY_COMMANDS.send` to hand over a render, and
/// servicing that render is what releases it to drain this event's channel.
async fn place_held_display_event() {
    let Some(event) = display_holder().pending() else {
        return core::future::pending::<()>().await;
    };
    DISPLAY_EVENTS.send(event).await;
    // Nothing awaits between the send completing and this, so the holder
    // cannot be observed empty with the event still unsent, or cleared for a
    // send that a cancellation abandoned.
    let _ = with_display_holder(DisplayEventHolder::placed);
}

/// Kept out of line: first-call initialization constructs `DecompressorOxide`
/// by value into a static; that temporary stack frame must not sit at the base
/// of the EPUB open call chain.
///
/// While `EPUB_ZIP_INFLATE`'s 32 KiB window buffer is const-initialized in `.bss`,
/// `DecompressorOxide::new()` still produces a measured 10,512-byte temporary
/// frame when initializing the decoder. `#[inline(never)]` keeps this ~10.5 KiB
/// allocation transient on a shallow frame rather than resident under the deeper
/// EPUB open call stack, leaving the 13,840-byte EPUB cache builder as the largest
/// The firmware's side of the storage task: its event channels, the scratch
/// it keeps in statics, and the sync loan only it may build.
struct FwHost;

impl storage::task::Host for FwHost {
    fn send(&mut self, event: &LibraryEvent) {
        send_library_event(event);
    }

    fn send_required(&mut self, event: &LibraryEvent) {
        send_required_library_event(event);
    }

    fn send_loaded(&mut self, event: &LibraryEvent) {
        send_loaded_library_event(event);
    }

    fn latest_reader_request_id(&self) -> u32 {
        LATEST_READER_REQUEST_ID.load(Ordering::Relaxed)
    }

    fn waiting_on_pick(&self, request_id: u32) -> bool {
        crate::LIBRARY_BROWSE_REQUEST_ID.load(Ordering::Relaxed) == request_id
    }

    fn requeue(&mut self, command: StorageCommand) {
        let _ = STORAGE_COMMANDS.try_send(command);
    }

    fn ensure_scratch<'s>(
        &mut self,
        slot: &'s mut Option<&'static mut ReaderCacheScratch<'static>>,
    ) -> &'s mut ReaderCacheScratch<'static> {
        ensure_epub_scratch(slot)
    }

    fn network_saved(&mut self, ssid: app_core::WifiSsid) {
        let _ = crate::SYNC_EVENTS.try_send(crate::SyncEvent::NetworkSaved(ssid));
    }

    fn wifi_storage_result(&mut self, confirmed: bool) {
        let _ = crate::WIFI_STORAGE_RESULTS.try_send(confirmed);
    }

    fn grant_sync_loan(
        &mut self,
        card: &mut impl storage::card::Card,
        scratch: &'static mut ReaderCacheScratch<'static>,
    ) {
        let mut loan = crate::sync_mem::dismantle_scratch(scratch);
        let stored_wifi = book_build::load_wifi_credentials(card);
        // The hint is matched against the credentials here rather than in
        // the wifi task, because this is the one place holding both
        // records — and a hint for another network must never steer this
        // join. A mismatch is not an error; it just means scan.
        loan.wifi_hint = stored_wifi.as_ref().and_then(|creds| {
            let ssid = &creds.ssid[..creds.ssid_len.min(32) as usize];
            book_build::load_wifi_ap_hint(card)
                .filter(|hint| hint.matches_ssid(ssid))
                .map(|hint| app_core::WifiApHint {
                    bssid: hint.bssid,
                    channel: hint.channel,
                })
        });
        loan.wifi = stored_wifi.map(|record| app_core::WifiCredentials {
            ssid: record.ssid,
            ssid_len: record.ssid_len,
            password: record.password,
            password_len: record.password_len,
        });
        loan.catalog_len = crate::library_sd::write_catalog_listing(card, loan.http_b);
        if crate::SYNC_LOANS.try_send(Ok(loan)).is_err() {
            // Unreachable in practice: the wifi task blocks on each
            // answer before it can request again. The memory is gone
            // either way.
            esp_println::println!("storage: sync loan channel full");
        }
    }

    fn refuse_sync_loan(&mut self) {
        let _ = crate::SYNC_LOANS.try_send(Err(app_core::SyncError::Storage));
    }
}

/// frame in the binary. `tools/check.sh stack-frames` is the guard on it.
#[inline(never)]
fn ensure_epub_scratch<'a>(
    epub_scratch: &'a mut Option<&'static mut ReaderCacheScratch<'static>>,
) -> &'a mut ReaderCacheScratch<'static> {
    if epub_scratch.is_none() {
        esp_println::println!("storage: init epub scratch");
        let zip_ref = EPUB_ZIP_INFLATE.take();
        // Build the decoder here rather than letting the first decode do it:
        // this frame is shallow, the EPUB open chain's is not.
        zip_ref.prepare(EPUB_DECOMPRESSOR.init(proto::epub::DecompressorOxide::new()));
        *epub_scratch = Some(EPUB_SCRATCH.init(ReaderCacheScratch::new(
            EPUB_TAIL.take(),
            EPUB_HEADER.take(),
            EPUB_NAME.take(),
            EPUB_COMPRESSED.take(),
            EPUB_CONTAINER.take(),
            EPUB_OPF.take(),
            EPUB_XHTML.take(),
            EPUB_BOOK_SECTIONS.take(),
            zip_ref,
        )));
    }
    epub_scratch.as_deref_mut().unwrap()
}

/// The on-card record for a state the app persisted, with the fields only the
/// firmware knows filled in.
///
/// The reducer derives chapter from the 128-capped `sd_chapter_for_page`, so a
/// deep position would save a stuck chapter that the sleep/boot colophon then
/// shows wrong until the book reopens. The firmware tracks the true chapter
/// over the whole book; adopt it for the loaded SD book so saved and restored
/// state name the chapter right.
/// The orientation the last render was asked for, which is the one a
/// storage command should be answered in. True before any request, matching
/// the boot default.
fn last_portrait(planner: &RefreshPlanner) -> bool {
    planner
        .last_request()
        .map(|last| app_core::is_portrait(last.orientation))
        .unwrap_or(true)
}

/// Build a sleep-screen request and load its book metadata from the card.
/// A pending position outranks saved state; otherwise the book's own position
/// takes precedence over the global record. Returns `None` when no usable
/// record or unambiguous catalog match can be read. Invalid settings use defaults.
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
        None => (
            book_build::load_app_state(&mut crate::sd_session::card(epd, sd_cs))?,
            false,
        ),
    };
    let index = crate::library_sd::find_index_by_identity(
        &mut crate::sd_session::card(epd, sd_cs),
        record.source_hash,
        record.source_size,
        record.legacy_source_identity,
    )?;
    crate::library_sd::load_active_entry(
        &mut crate::sd_session::card(epd, sd_cs),
        library,
        usize::from(index),
    );
    let (chapter, screen) = if unflushed {
        (record.chapter, record.screen)
    } else {
        storage::task::book_position(
            &mut crate::sd_session::card(epd, sd_cs),
            library,
            index,
            record,
        )
    };
    book_build::load_chapter_title(
        &mut crate::sd_session::card(epd, sd_cs),
        usize::from(index),
        chapter,
        library,
    );
    let page_count = book_build::restore_book_page_count(
        &mut crate::sd_session::card(epd, sd_cs),
        usize::from(index),
        library,
    );
    Some(RenderRequest {
        kind: RenderKind::Page,
        // The sleep frame is not queued and answers no press.
        requested_at_ms: 0,
        view: AppView::Home,
        page: screen,
        page_count,
        chapter,
        selection: 0,
        book_id: ReaderSource::sd(index).book_id(),
        orientation: display_orientation_from_u8(record.reading_orientation)
            .unwrap_or(DisplayOrientation::PortraitButtonsLeft),
        front_buttons: app_core::front_buttons_from_u8(record.front_buttons)
            .unwrap_or(app_core::FrontButtons::PagesRight),
        reading_sheet: false,
        library_menu: app_core::LibraryMenu::None,
        library_move_pending: false,
        library_rescanning: false,
        footer_percent: None,
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
