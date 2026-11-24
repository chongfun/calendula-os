use crate::book_build::{self, ReaderCacheScratch};
use crate::display_flush::{self, Epd};
use crate::next_page::{NextPage, StageUse};
use crate::{
    library_event_holder, own_waker, place_held_display_event, place_held_library_event,
    poll_to_completion, send_display_event, send_library_event, send_required_library_event,
    storage_command_while_free, DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent,
    StorageCommand, DISPLAY_COMMANDS, LATEST_READER_REQUEST_ID, POWER_EVENTS, STORAGE_COMMANDS,
};
use app_core::storage_loop::{
    loop_arm, owed_work, owed_work_delay_ms, Drained, LoopArm, OpenAction, OpenSequence, OwedWork,
    SleepAction, SleepRefusal, SleepSequence,
};
use app_core::{
    AppView, ChapterCursor, ReaderSource, RefreshPlanner, RenderRequest, SleepResume, SyncSession,
};
use core::sync::atomic::Ordering;
use core::task::Waker;
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use embassy_futures::select::{select, select5, Either, Either5};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use reader_cache::store::ReaderStore;
use reader_cache::{
    READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH, READER_HEADER_SCRATCH, READER_OPF_SCRATCH,
    READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use static_cell::ConstStaticCell;
use storage::task::OwedRescan;
use storage::views::{
    open_loading_plate_request, rescan_plate_request, sleep_request_from_saved_state,
};

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

/// Build the EPUB scratch before the executor starts, while the boot stack is
/// otherwise empty. `DecompressorOxide::new()` constructs a ~10.5 KB value on
/// the stack before moving it into `.bss`; running that inside a task body
/// stacks it on top of the executor and poll frames.
#[inline(never)]
pub(crate) fn init_epub_scratch() -> &'static mut ReaderCacheScratch<'static> {
    let zip_ref = EPUB_ZIP_INFLATE.take();
    zip_ref.prepare(EPUB_DECOMPRESSOR.init(proto::epub::DecompressorOxide::new()));
    EPUB_SCRATCH.init(ReaderCacheScratch::new(
        EPUB_TAIL.take(),
        EPUB_HEADER.take(),
        EPUB_NAME.take(),
        EPUB_COMPRESSED.take(),
        EPUB_CONTAINER.take(),
        EPUB_OPF.take(),
        EPUB_XHTML.take(),
        zip_ref,
    ))
}

/// Own panel and card I/O, servicing display commands before deferred rescans
/// and book-build slices. Sleep refuses any pick still waiting on a rescan.
///
/// `woke_on_sleep_screen` carries the retained sleep screen from a deep-sleep
/// wake; `probe_diag` is the boot probe report to write to the card.
#[embassy_executor::task]
pub async fn run(
    mut epd: Epd,
    mut sd_cs: Output<'static>,
    mut woke_on_sleep_screen: Option<crate::sleep_marker::SleepScreen>,
    probe_diag: crate::epd_probe::ProbeDiag,
    scratch: &'static mut ReaderCacheScratch<'static>,
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
    storage_task.epub_scratch = Some(scratch);
    // A pick whose rescan the loop still owes: announced and painted, run by
    // the loop's own branch behind any frame queued meanwhile. One byte.
    let mut owed_rescan: Option<OwedRescan> = None;
    // On a deep-sleep (Power button) wake the panel still shows the sleep
    // screen: `woke_on_sleep_screen` is `Some` only when the RTC wake cause
    // is the armed GPIO (or bench timer) *and* the pre-sleep handshake
    // recorded that the sleep frame settled on the panel (see sleep_marker).
    // The seeded planner then picks a fast page turn (when the redrawn sleep
    // frame digests to the recorded one) or the ~1.5 s one-flicker FastClean
    // instead of the ~3.5 s multi-flash Full. Any other boot — battery pull,
    // crash, software reset, or a sleep whose final flush failed — leaves the
    // seed false and keeps the full waveform for unknown panel contents.
    let mut refresh_planner =
        RefreshPlanner::new().with_panel_shows_sleep_screen(woke_on_sleep_screen.is_some());
    bench_log!(
        "bench: wake sleep_screen={} reading={}",
        woke_on_sleep_screen.is_some(),
        woke_on_sleep_screen.is_some_and(|screen| screen.reading),
    );
    // True while RED RAM is known to hold exactly prev_fb's content, letting
    // a fast refresh skip its previous-frame stream. Reset on any failure,
    // sleep, or panel re-init; false just means the next flush writes RED.
    let mut prev_prestaged = false;
    static SD_LIBRARY: ConstStaticCell<ReaderStore> = ConstStaticCell::new(ReaderStore::new());
    let sd_library = SD_LIBRARY.take();
    // ReaderStore::new() is all-zero bytes so the ~52 KB static lives in
    // .bss (not a flashed .data image); fill in the non-zero defaults once,
    // in place, before anything reads the store.
    sd_library.init_runtime_defaults();
    // ASCII glyph metrics for the custom font pack; shared by the build's
    // line measurement and the reading-page draw so both stay off the card.
    static FONT_METRICS: ConstStaticCell<crate::custom_font::MetricCache> =
        ConstStaticCell::new(crate::custom_font::MetricCache::new());
    let font_metrics = FONT_METRICS.take();

    // No panel init here: the init guard in the loop's render and Sleep arms
    // (fresh planner: screen off, no last request) owns the boot init, as it
    // owns re-init after a display sleep. Initializing at task start too made
    // every boot's first render pay reset + init twice (on the X3 that second
    // pass re-whitens both ~52 KB DTM planes).

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

    let mut held_restore = fw_common::HeldSleepRestore::new();
    // Whether the page on the panel holds images: a quick page turn out of
    // it would leave their dots' residue behind.
    let mut shown_has_images = false;
    let mut next_page = NextPage::new();

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
            storage_task.background_owed(sd_library, refresh_planner.last_request())
                && !sd_library.text_holds_toc(),
        )
        .filter(|_| !sync_session.active() && library_event_holder().storage_may_run());
        // An unrelated queued command, such as StageNext or SetFrontlight,
        // cannot replace the sleep frame. Drain DISPLAY_COMMANDS first so a
        // queued Render/Sleep supersedes any held restore before it paints.
        let (incoming, is_stage, is_restore) = if let Some(req) = DISPLAY_COMMANDS
            .is_empty()
            .then(|| held_restore.take_for_render())
            .flatten()
        {
            esp_println::println!("display: rendering held restore");
            (
                Some(Either5::First(DisplayCommand::Render(req))),
                false,
                true,
            )
        } else {
            let stage_due = next_page.due(due.is_some());
            // Release the display boot hold only during the command/background
            // scheduler wait. Reacquire before any SPI, panel or SD work.
            crate::light_sleep::release(crate::light_sleep::Hold::Display);
            let res = if stage_due {
                match select(
                    select5(
                        DISPLAY_COMMANDS.receive(),
                        storage_command_while_free(owed_rescan.is_some()),
                        place_held_library_event(),
                        place_held_display_event(),
                        owed_work_due(
                            due,
                            storage_task
                                .background_attempts(sd_library, refresh_planner.last_request()),
                        ),
                    ),
                    embassy_time::Timer::after_millis(1),
                )
                .await
                {
                    Either::First(event) => (Some(event), false, false),
                    Either::Second(()) => (None, true, false),
                }
            } else {
                let event = select5(
                    DISPLAY_COMMANDS.receive(),
                    storage_command_while_free(owed_rescan.is_some()),
                    place_held_library_event(),
                    place_held_display_event(),
                    owed_work_due(
                        due,
                        storage_task
                            .background_attempts(sd_library, refresh_planner.last_request()),
                    ),
                )
                .await;
                (Some(event), false, false)
            };
            crate::light_sleep::hold(crate::light_sleep::Hold::Display);
            res
        };
        if is_stage {
            if !next_page
                .stage(
                    &mut epd,
                    fb,
                    sd_library,
                    refresh_planner.screen_on() && prev_prestaged && !sync_session.active(),
                )
                .await
            {
                prev_prestaged = false;
            }
            continue;
        }
        let incoming = incoming.unwrap();
        match incoming {
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
                    next_page.idle_ran(sd_library);
                }
                // The branch is pending with nothing owed.
                None => {}
            },
            Either5::Third(()) | Either5::Fourth(()) => {}
            Either5::First(DisplayCommand::StageNext { request, presses }) => {
                next_page.offer(request, presses);
            }
            Either5::First(DisplayCommand::SetFrontlight { brightness, warmth }) => {
                held_restore.update_frontlight(brightness, warmth);
            }
            Either5::First(DisplayCommand::Render(mut request)) => {
                if !is_restore && held_restore.supersede() {
                    esp_println::println!("display: held restore superseded by render");
                }
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
                                    request.chapters_expanded,
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
                // A page staged while the reader read the one before: fb and
                // BW RAM already hold this render's frame.
                let stage = next_page.take(request, sd_library, refresh_planner.screen_on());
                let staged = stage == StageUse::Hit;
                // Whether the frame shows images, which draw from the card,
                // and whether one is a picture, which takes the full refresh
                // and gives the frame after it the clean.
                let shows_images = crate::views::reader_page_has_images(request, sd_library);
                let shows_pictures = crate::views::reader_page_has_pictures(request, sd_library);
                let shows_plate = crate::views::reader_shows_plate(request, sd_library);
                // A page whose section the book index names loads in one file
                // read: the frame on the glass stays rather than a plate
                // flashing for tens of milliseconds, and the load's own render
                // draws the page. The glass must be known for that.
                let deferred = shows_plate
                    && crate::views::reader_section_loads_quickly(request, sd_library)
                    && refresh_planner.screen_on()
                    && refresh_planner.last_request().is_some();
                let layout_start = Instant::now();
                if !staged
                    && !render_reader_from_card(
                        &mut epd,
                        &mut sd_cs,
                        fb,
                        request,
                        sd_library,
                        font_metrics,
                        &mut storage_task.epub_scratch,
                        shows_images,
                    )
                {
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
                        refresh_planner.record_failure();
                        if is_restore {
                            recover_failed_restore(request, &mut held_restore).await;
                        } else {
                            let (display_event, power_event) =
                                app_core::display_refresh_outcome(false, None);
                            send_display_event(&display_event);
                            send_required_power_event(power_event).await;
                        }
                        continue;
                    }
                    esp_println::println!("display: wake init complete");
                    prev_prestaged = false;
                }

                // The first paint over a sleep screen takes the panel's own
                // page turn: the screen may have stood for days.
                let waking = refresh_planner.panel_shows_sleep_screen();
                if let Some(screen) = woke_on_sleep_screen.take() {
                    if waking {
                        hold_woken_sleep_frame(
                            screen,
                            prev_fb,
                            request,
                            sd_library,
                            &mut refresh_planner,
                        );
                    }
                }
                let mode = refresh_planner.mode_for_frame(request, shows_pictures);
                // On X3 the UC8253 cannot run a differential Fast waveform
                // from the powered-down state. Its flush driver promotes it
                // to FastClean; use that effective mode for the planner too,
                // or the clean would count as one of the next eight fast turns.
                #[cfg(feature = "device-x3")]
                let mode = if waking {
                    display::epd::uc8253::effective_refresh_mode(mode, false)
                } else {
                    mode
                };
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
                if deferred {
                    // The plate was drawn into fb; what follows (the prestage,
                    // the next fast diff) must see what the glass shows.
                    fb.copy_from(prev_fb);
                }
                let skipped = deferred
                    || refresh_planner.screen_on()
                        && refresh_planner.last_request().is_some()
                        && mode == RefreshMode::Fast
                        && fb.bytes() == prev_fb.bytes();
                let quick_allowed = !(shows_images || shown_has_images || waking);
                let flush_start = Instant::now();
                let flushed = if skipped {
                    Ok(display_flush::PanelSettle::from_ms(0))
                } else if staged {
                    display_flush::flush_staged(
                        &mut epd,
                        fb,
                        prev_fb,
                        refresh_planner.screen_on(),
                        mode,
                        prev_prestaged,
                        quick_allowed,
                    )
                    .await
                } else {
                    display_flush::flush(
                        &mut epd,
                        fb,
                        prev_fb,
                        refresh_planner.screen_on(),
                        mode,
                        prev_prestaged,
                        quick_allowed,
                    )
                    .await
                };
                if let Ok(settle) = flushed {
                    if is_restore {
                        held_restore.completed();
                    }
                    if !deferred {
                        shown_has_images = shows_images;
                    }
                    let flush_ms = flush_start.elapsed().as_millis();
                    if deferred {
                        // The glass still shows the frame before, so the
                        // planner keeps it: the page's own render is judged
                        // against that frame, a chapter list's included.
                    } else if skipped {
                        refresh_planner.record_skipped_render(request);
                    } else {
                        // A normal Reading render can draw the loading plate
                        // too, not just the storage-driven flush_plate path.
                        // Keep that fact until the real page replaces it.
                        if shows_plate {
                            refresh_planner.record_loading_plate(request, mode);
                        } else {
                            refresh_planner.record_render_showing(request, mode, shows_pictures);
                        }
                        prev_fb.copy_from(fb);
                    }
                    // Settle before the ~23 ms RED prestage: the panel is visually
                    // done, so unblock the input/power pipeline. The prestage still
                    // runs on this task before the next command is dequeued, so
                    // `prev_prestaged` is always current by the next flush, and a
                    // Sleep queued by power_task after DisplaySettled waits behind it.
                    let settled_at_ms = Instant::now().as_millis();
                    if !is_restore {
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
                        let (display_event, power_event) =
                            app_core::display_refresh_outcome(true, chapter_cursor);
                        send_display_event(&display_event);
                        send_required_power_event(power_event).await;
                    }
                    // Draw the staged page again from scratch and compare.
                    #[cfg(feature = "bench-selftest")]
                    if staged {
                        crate::next_page::check_staged(fb, prev_fb, request, sd_library);
                    }
                    // Emitted here, at the settle, and not after the prestage
                    // below. This timestamp is what the bench pairs each input
                    // against, so printing it later charged the reader for a
                    // write they never waited on: `Settled` has already gone
                    // out, and press-to-settled ends on this line.
                    let stage_note = stage.label();
                    bench_log!(
                        "bench: render view={:?} mode={:?} page={} chapter={} staged={} stage={} layout_ms={} flush_ms={} req_ms={} deq_ms={} t_ms={} skipped={} pictures={} plate={}",
                        request.view,
                        mode,
                        request.page,
                        request.chapter,
                        staged,
                        stage_note,
                        layout_ms,
                        flush_ms,
                        request.requested_at_ms,
                        dequeued_at_ms,
                        settled_at_ms,
                        skipped,
                        shows_pictures,
                        shows_plate,
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
                        if settle.old_plane_synced() {
                            // Full/PowerDown explicitly sync DTM1; UC8253
                            // Fast/FastClean refreshes also appear to copy
                            // DTM2 -> DTM1 in hardware (Free-Ink #220).
                            prev_prestaged = true;
                        } else {
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
                            // (fw-c3/src/display_flush/ssd1677.rs). Deferring is also
                            // self-sustaining, since each deferral leaves the next
                            // turn unstaged, so a held button would pay the write
                            // on-path every turn instead of off-path once.
                            //
                            // The skip above is the one case that owes nothing,
                            // and for the opposite reason: it wrote no panel RAM,
                            // so the staging still standing is still correct.
                            prev_prestaged = display_flush::prestage_previous(&mut epd, prev_fb)
                                .await
                                .is_ok();
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
                    if is_restore {
                        recover_failed_restore(request, &mut held_restore).await;
                    } else {
                        let (display_event, power_event) =
                            app_core::display_refresh_outcome(false, None);
                        send_display_event(&display_event);
                        send_required_power_event(power_event).await;
                    }
                }
            }
            Either5::First(DisplayCommand::Sleep { generation, .. }) => {
                next_page.forget();
                if let Some(reason) = crate::sleep_is_cancelled(generation) {
                    esp_println::println!(
                        "display: sleep generation={} cancelled ({:?}); skipping sleep",
                        generation,
                        reason
                    );
                    continue;
                }
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
                let mut may_keep_draining = library_event_holder().sleep_may_proceed();
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
                                    may_keep_draining = library_event_holder().sleep_may_proceed();
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
                let sleep_holds_an_event =
                    refusal.is_none() && !library_event_holder().sleep_may_proceed();
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
                let request = refresh_planner
                    .last_request()
                    .or(held_restore.pending())
                    .or_else(|| {
                        sleep_request_from_saved_state(
                            &mut crate::sd_session::card(&mut epd, &mut sd_cs),
                            sd_library,
                            storage_task.pending_progress.as_ref(),
                            false,
                        )
                    });
                if let Some(reason) = crate::sleep_is_cancelled(generation) {
                    esp_println::println!(
                        "display: sleep generation={} cancelled before flush ({:?}); skipping sleep",
                        generation,
                        reason
                    );
                    continue;
                }
                // Only a sleep committed to painting can supersede the
                // previous restoration. Cancelled/refused sleeps above retain it.
                if held_restore.supersede() {
                    esp_println::println!("display: held restore superseded by sleep");
                }
                // A sleep committed to painting supersedes the boot-time wake
                // marker so an abandoned deep sleep cannot later reconstruct an
                // older sleep screen over `prev_fb`.
                woke_on_sleep_screen = None;
                if let Some(request) = request {
                    crate::views::render_sleep(fb, request, sd_library);
                    bench_log!(
                        "bench: sleep_frame view={:?} page={} page_count={} chapter={}",
                        request.view,
                        request.page,
                        request.page_count,
                        request.chapter,
                    );
                } else {
                    crate::views::render_sleep_blank(fb);
                }
                let sleep_screen = crate::sleep_marker::SleepScreen {
                    digest: crate::sleep_marker::frame_digest(fb),
                    reading: request.is_some_and(|req| req.view == AppView::Reading),
                };
                #[cfg(feature = "device-x3")]
                let sleep_mode = RefreshMode::PowerDown;
                #[cfg(not(feature = "device-x3"))]
                let sleep_mode = RefreshMode::Full;
                let flushed = if panel_ready {
                    display_flush::flush(
                        &mut epd,
                        fb,
                        prev_fb,
                        refresh_planner.screen_on(),
                        sleep_mode,
                        prev_prestaged,
                        false,
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
                    if sleep_frame_settled {
                        // prev_fb holds the sleep screen for as long as the chip
                        // stays up, so a wake before deep sleep is a page turn.
                        refresh_planner.hold_sleep_frame();
                    }
                }
                // Persist whether the panel really holds the sleep frame
                // before DisplayAsleep releases the power task to cut power:
                // the next boot's GPIO wake seeds its fast-wake planner from
                // this marker, and a flush or panel-sleep failure must leave
                // it cleared so that boot falls back to the full waveform.
                crate::sleep_marker::record_sleep_image(
                    (panel_slept && sleep_frame_settled).then_some(sleep_screen),
                );
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
                    let resume_reason = park_until_resumed(generation).await;
                    // Do not use !DISPLAY_COMMANDS.is_empty() as evidence of a
                    // replacement frame: non-paint commands (StageNext /
                    // SetFrontlight) can queue during cancellation. Hold the
                    // restore until a real Render/Sleep supersedes it, or until
                    // the command queue drains.
                    if let Some(reason) = resume_reason {
                        if let Some(req) =
                            app_core::sleep_resume_restore_render(reason, request, false)
                        {
                            esp_println::println!(
                                "display: sleep abandoned ({:?}); holding restore",
                                reason
                            );
                            held_restore.hold(req);
                        }
                    }
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
                    if let Some(req) = request {
                        // Even a failed sleep-frame flush leaves the panel
                        // invalidated (and possibly partially painted).
                        // Reinitialize and repaint.
                        held_restore.hold(req);
                    }
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
                            next_page.forget();
                            crate::views::render(fb, loading_request, sd_library);
                            let quick_allowed = !shown_has_images;
                            shown_has_images = false;
                            flush_plate(
                                &mut epd,
                                fb,
                                prev_fb,
                                &mut refresh_planner,
                                &mut prev_prestaged,
                                loading_request,
                                quick_allowed,
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
                                false,
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
    quick_allowed: bool,
) {
    // A plate shows no picture, but it may replace one.
    let mode = refresh_planner.mode_for_frame(request, false);
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
        quick_allowed,
    )
    .await
    {
        // Held, not deferred like the render path's: no prestage follows a
        // plate, so there is no nearer owner for the interval than here.
        settle.wait().await;
        if request.view == AppView::Reading {
            refresh_planner.record_loading_plate(request, mode);
        } else {
            refresh_planner.record_render_showing(request, mode, false);
        }
        prev_fb.copy_from(fb);
        *prev_prestaged = settle.old_plane_synced();
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
                false,
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

async fn recover_failed_restore(request: RenderRequest, held: &mut fw_common::HeldSleepRestore) {
    if held.retry_failed(request) {
        // Retry a transient controller BUSY/reset failure, but do not loop
        // indefinitely on a broken panel.
        Timer::after(Duration::from_millis(200)).await;
    } else {
        // Synthetic restores have no app render lock. After two retries,
        // release recovery to the app's bounded repaint path.
        esp_println::println!("display: synthetic restore retries exhausted");
        let (display_event, power_event) = app_core::display_refresh_outcome(false, None);
        send_display_event(&display_event);
        send_required_power_event(power_event).await;
    }
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
async fn park_until_resumed(generation: u32) -> Option<SleepResume> {
    if let Some(reason) = crate::sleep_is_cancelled(generation) {
        esp_println::println!("display: sleep abandoned ({:?}); resuming", reason);
        return Some(reason);
    }
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
            Either::First(()) => {
                if let Some(reason) = crate::sleep_is_cancelled(generation) {
                    esp_println::println!("display: sleep abandoned ({:?}); resuming", reason);
                    return Some(reason);
                }
            }
            Either::Second(_) => {
                esp_println::println!(
                    "display: no deep sleep or resume within {} s; releasing the panel",
                    ABANDONED_HANDSHAKE_CEILING_SECS
                );
                return None;
            }
        }
    }
}

/// Renders a reading page that needs the card: one in the custom face, or
/// one that shows a picture, whose rows stream from the book's pack through
/// the EPUB scratch's XHTML window (idle while a page draws). False when
/// the page needs nothing from the card, or the session would not open, so
/// the caller draws it from RAM (a picture as its placeholder). With the
/// scratch on loan to a sync session, a page draws its placeholders too.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn render_reader_from_card(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    shows_images: bool,
) -> bool {
    if request.view != AppView::Reading || !ReaderSource::from_book_id(request.book_id).is_sd() {
        return false;
    }
    let custom = request.font_family == display::font::FontFamily::Custom
        && !display::font::builtin_custom_available()
        && sd_library.custom_font_available();
    let Some(scratch) = epub_scratch.as_deref_mut().filter(|_| shows_images) else {
        if !custom {
            return false;
        }
        return crate::sd_session::with_root(epd, sd_cs, |root| {
            crate::views::render_reader_from_root(
                fb,
                request,
                sd_library,
                font_metrics,
                root,
                &mut [],
                true,
            );
        })
        .is_ok();
    };
    let band = scratch.image_band();
    crate::sd_session::with_root(epd, sd_cs, |root| {
        crate::views::render_reader_from_root(
            fb,
            request,
            sd_library,
            font_metrics,
            root,
            band,
            custom,
        );
    })
    .is_ok()
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
        send_library_event(event);
    }

    fn latest_reader_request_id(&self) -> u32 {
        LATEST_READER_REQUEST_ID.load(Ordering::Relaxed)
    }

    fn button_presses(&self) -> u32 {
        crate::BUTTON_PRESSES.load(Ordering::Relaxed)
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

/// The scratch is built at boot ([`init_epub_scratch`]) and only ever absent
/// while lent to a sync session, which refuses reader work and ends in a
/// reset; before this change the lazy build here panicked on a second
/// `take()` in the same case.
fn ensure_epub_scratch<'a>(
    epub_scratch: &'a mut Option<&'static mut ReaderCacheScratch<'static>>,
) -> &'a mut ReaderCacheScratch<'static> {
    epub_scratch
        .as_deref_mut()
        .expect("EPUB scratch is lent only to the sync session")
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
/// After a deep-sleep wake, draw the sleep screen into `prev_fb` as the
/// sleep path drew it, from the first render's request and the restored
/// store. If it digests to what the sleep path recorded, `prev_fb` is the
/// frame on the panel and the planner may turn the page from it. If the
/// state it was drawn from has moved (a chapter cursor that settled after
/// the last paint, a page count that grew), the digest differs and the
/// paint takes the clean, which reads no previous frame.
fn hold_woken_sleep_frame(
    screen: crate::sleep_marker::SleepScreen,
    prev_fb: &mut Framebuffer,
    request: RenderRequest,
    library: &ReaderStore,
    refresh_planner: &mut RefreshPlanner,
) {
    let start = Instant::now();
    crate::views::render_sleep(prev_fb, request, library);
    let held = crate::sleep_marker::matches(screen, prev_fb);
    if held {
        refresh_planner.hold_sleep_frame();
    }
    bench_log!(
        "bench: wake_frame held={} view={:?} page={} page_count={} chapter={} draw_us={}",
        held,
        request.view,
        request.page,
        request.page_count,
        request.chapter,
        start.elapsed().as_micros(),
    );
}

fn last_portrait(planner: &RefreshPlanner) -> bool {
    planner
        .last_request()
        .map(|last| app_core::is_portrait(last.orientation))
        .unwrap_or(true)
}
