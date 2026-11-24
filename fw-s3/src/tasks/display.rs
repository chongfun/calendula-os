use core::task::Waker;

use crate::display_flush::{self, Epd};
use crate::frontlight::Frontlight;
use crate::sd_session::SdmmcBus;
use app_core::storage_loop::{
    loop_arm, owed_work, owed_work_delay_ms, Drained, LoopArm, OpenAction, OpenSequence, OwedWork,
    SleepAction, SleepRefusal, SleepSequence,
};
use app_core::{
    AppView, ChapterCursor, DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, RefreshPlanner,
    RenderRequest, SleepResume, SyncSession,
};
use core::sync::atomic::Ordering;
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use embassy_futures::select::{select5, Either5};
use embassy_time::{Duration, Instant};
pub(crate) use fw_common::send_display_event;
use fw_common::{
    library_event_holder, own_waker, place_held_display_event, place_held_library_event,
    poll_to_completion, send_library_event, send_required_library_event,
    storage_command_while_free, HeldSleepRestore, ReaderSource, StorageCommand, DISPLAY_COMMANDS,
    LATEST_READER_REQUEST_ID, LIBRARY_BROWSE_REQUEST_ID, POWER_EVENTS, STORAGE_COMMANDS,
};
use reader_cache::store::ReaderStore;
use reader_cache::{
    READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH, READER_HEADER_SCRATCH, READER_OPF_SCRATCH,
    READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use static_cell::ConstStaticCell;
use storage::book_build::{self, ReaderCacheScratch};
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
static EPUB_ZIP_INFLATE: ConstStaticCell<proto::epub::ZipInflateScratch> =
    ConstStaticCell::new(proto::epub::ZipInflateScratch::new());
static EPUB_DECOMPRESSOR: static_cell::StaticCell<proto::epub::DecompressorOxide> =
    static_cell::StaticCell::new();
static EPUB_SCRATCH: static_cell::StaticCell<ReaderCacheScratch<'static>> =
    static_cell::StaticCell::new();

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

#[embassy_executor::task]
pub async fn run(
    mut epd: Epd,
    mut sdmmc_bus: SdmmcBus,
    mut frontlight: Frontlight,
    mut woke_on_sleep_screen: Option<crate::sleep_marker::SleepScreen>,
    probe_diag: crate::epd_probe::ProbeDiag,
    scratch: &'static mut ReaderCacheScratch<'static>,
) {
    esp_println::println!("s3-display: started t_ms={}", Instant::now().as_millis());

    static FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB.init(Framebuffer::new());
    let prev_fb = crate::sync_mem::take_prev_fb().expect("prev_fb claimed once");

    let mut sync_session = SyncSession::default();
    let mut storage_task = storage::task::StorageTask::default();
    storage_task.epub_scratch = Some(scratch);
    let mut owed_rescan: Option<OwedRescan> = None;
    let mut refresh_planner =
        RefreshPlanner::new().with_panel_shows_sleep_screen(woke_on_sleep_screen.is_some());
    bench_log!(
        "bench: wake sleep_screen={} reading={}",
        woke_on_sleep_screen.is_some(),
        woke_on_sleep_screen.is_some_and(|screen| screen.reading),
    );

    static SD_LIBRARY: ConstStaticCell<ReaderStore> = ConstStaticCell::new(ReaderStore::new());
    let sd_library = SD_LIBRARY.take();
    sd_library.init_runtime_defaults();

    static FONT_METRICS: ConstStaticCell<storage::custom_font::MetricCache> =
        ConstStaticCell::new(storage::custom_font::MetricCache::new());
    let font_metrics = FONT_METRICS.take();

    match crate::sd_session::with_root(&mut sdmmc_bus, crate::ota_update::apply_pending_update) {
        Ok(outcome) if outcome.needs_reset() => {
            esp_println::println!("s3-display: {:?}; resetting", outcome);
            embassy_time::Timer::after(Duration::from_millis(50)).await;
            esp_hal::system::software_reset();
        }
        Ok(_) => {}
        Err(err) => esp_println::println!("s3-display: ota check skipped: {:?}", err),
    }

    crate::probe_report::write(&mut sdmmc_bus, &probe_diag);

    #[cfg(feature = "ota-selftest")]
    if crate::ota_update::run_selftest() {
        esp_println::println!("s3-selftest: staged; resetting");
        embassy_time::Timer::after(Duration::from_millis(50)).await;
        esp_hal::system::software_reset();
    }

    let mut held_restore = HeldSleepRestore::new();
    let mut current_frontlight: Option<(u8, u8)> = None;
    // Sleep interrupted an upload session while this task was its sole
    // DISPLAY_COMMANDS receiver. Preserve the sleep until the queued
    // commands are drained rather than evicting any render acknowledgement.
    let mut pending_upload_sleep: Option<DisplayCommand> = None;
    let mut shown_has_images = false;

    loop {
        let (incoming, is_restore) = if DISPLAY_COMMANDS.is_empty() {
            if let Some(sleep) =
                fw_common::take_deferred_display_sleep(true, &mut pending_upload_sleep)
            {
                (Either5::First(sleep), false)
            } else if let Some(req) = held_restore.take_for_render() {
                esp_println::println!("s3-display: rendering held restore");
                (Either5::First(DisplayCommand::Render(req)), true)
            } else {
                let due = owed_work(
                    owed_rescan.is_some(),
                    storage_task.background_owed(sd_library, refresh_planner.last_request())
                        && !sd_library.text_holds_toc(),
                )
                .filter(|_| !sync_session.active() && library_event_holder().storage_may_run());

                (
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
                    )
                    .await,
                    false,
                )
            }
        } else {
            // An unrelated queued command, such as SetFrontlight, cannot
            // replace the sleep frame. Drain DISPLAY_COMMANDS first so a queued
            // Render/Sleep supersedes any held restore or SetFrontlight updates
            // it before it paints.
            let due = owed_work(
                owed_rescan.is_some(),
                storage_task.background_owed(sd_library, refresh_planner.last_request())
                    && !sd_library.text_holds_toc(),
            )
            .filter(|_| !sync_session.active() && library_event_holder().storage_may_run());

            (
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
                )
                .await,
                false,
            )
        };

        let owed_due_for_fifth = owed_work(
            owed_rescan.is_some(),
            storage_task.background_owed(sd_library, refresh_planner.last_request())
                && !sd_library.text_holds_toc(),
        )
        .filter(|_| !sync_session.active() && library_event_holder().storage_may_run());

        match incoming {
            Either5::Fifth(()) => match owed_due_for_fifth {
                Some(OwedWork::Rescan) => {
                    if let Some(owed) = owed_rescan.take() {
                        let portrait = last_portrait(&refresh_planner);
                        let mut painter = RescanPainter {
                            epd: &mut epd,
                            fb,
                            prev_fb,
                            refresh_planner: &mut refresh_planner,
                            waker: own_waker().await,
                        };
                        storage_task.rescan(
                            owed,
                            &mut crate::sd_session::card_reporting(&mut sdmmc_bus, &mut painter),
                            &mut FwHost,
                            sd_library,
                            portrait,
                        );
                    }
                }
                Some(OwedWork::BuildSlice) => {
                    storage_task.background_step(
                        &mut crate::sd_session::card(&mut sdmmc_bus),
                        &mut FwHost,
                        sd_library,
                        font_metrics,
                        refresh_planner.last_request(),
                    );
                }
                None => {}
            },
            Either5::Third(()) | Either5::Fourth(()) => {}
            Either5::First(DisplayCommand::StageNext { .. }) => {}
            Either5::First(DisplayCommand::SetFrontlight { brightness, warmth }) => {
                frontlight.set(brightness, warmth);
                current_frontlight = Some((brightness, warmth));
                // A held sleep-restore frame was frozen before this toggle.
                // Keep its PWM fields current so restoring the pixels does
                // not undo the frontlight change that cancelled sleep.
                held_restore.update_frontlight(brightness, warmth);
            }
            Either5::First(DisplayCommand::Render(mut request)) => {
                let dequeued_at_ms = Instant::now().as_millis();
                if !is_restore && held_restore.supersede() {
                    esp_println::println!("s3-display: held restore superseded by render");
                }
                // Touches while a panel update is in progress must not
                // select rows from either the stale or the incoming list.
                fw_common::DISPLAY_FRAME_ACTIVE.store(false, Ordering::Release);
                crate::input::set_touch_context(&request);
                frontlight.set(request.frontlight_brightness, request.frontlight_warmth);
                current_frontlight =
                    Some((request.frontlight_brightness, request.frontlight_warmth));

                let content_context_changed = refresh_planner
                    .last_request()
                    .map(|last| (last.view, last.book_id))
                    != Some((request.view, request.book_id));

                if !sync_session.active() {
                    if request.view == AppView::Library {
                        storage::library_sd::ensure_folder_page(
                            &mut crate::sd_session::card(&mut sdmmc_bus),
                            sd_library,
                            request.selection,
                            app_core::is_portrait(request.orientation),
                        );
                    } else if ReaderSource::from_book_id(request.book_id).is_sd() {
                        if let Some(index) = ReaderStore::selected_book_index(request.book_id) {
                            if content_context_changed {
                                storage::library_sd::load_active_entry(
                                    &mut crate::sd_session::card(&mut sdmmc_bus),
                                    sd_library,
                                    index,
                                );
                            }
                            if request.view == AppView::Chapters && sd_library.text_holds_toc() {
                                book_build::ensure_toc_window(
                                    &mut crate::sd_session::card(&mut sdmmc_bus),
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

                if request.view == AppView::Reading {
                    request.footer_percent = storage_task
                        .build_progress(request.book_id)
                        .map(proto::progress::JobProgress::percent);
                }

                let shows_images = crate::views::reader_page_has_images(request, sd_library);
                let shows_pictures = crate::views::reader_page_has_pictures(request, sd_library);
                let shows_plate = crate::views::reader_shows_plate(request, sd_library);
                let deferred = shows_plate
                    && crate::views::reader_section_loads_quickly(request, sd_library)
                    && refresh_planner.screen_on()
                    && refresh_planner.last_request().is_some();

                let layout_start = Instant::now();
                if !render_reader_from_card(
                    &mut sdmmc_bus,
                    fb,
                    request,
                    sd_library,
                    font_metrics,
                    &mut storage_task.epub_scratch,
                    shows_images,
                ) {
                    crate::views::render(fb, request, sd_library);
                }
                let layout_ms = layout_start.elapsed().as_millis();

                if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                    esp_println::println!("s3-display: wake init start");
                    if let Err(err) = display_flush::init_panel(&mut epd).await {
                        esp_println::println!("s3-display: wake init failed={:?}", err);
                        refresh_planner.record_failure();
                        if is_restore {
                            recover_failed_restore(request, &mut held_restore).await;
                        } else {
                            let (display_event, power_event) =
                                app_core::display_refresh_outcome(false, None);
                            send_display_event(&display_event);
                            let _ = POWER_EVENTS.try_send(power_event);
                        }
                        continue;
                    }
                }

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
                // Wake now takes FastClean or Full, neither of which needs
                // the old frame prestaged in controller RAM.
                let mode = refresh_planner.mode_for_frame(request, shows_pictures);
                if deferred {
                    fb.copy_from(prev_fb);
                }
                let skipped = deferred
                    || (refresh_planner.screen_on()
                        && refresh_planner.last_request().is_some()
                        && mode == RefreshMode::Fast
                        && fb.bytes() == prev_fb.bytes());

                let flush_start = Instant::now();
                let flushed = if skipped {
                    Ok(())
                } else {
                    display_flush::flush(&mut epd, fb, prev_fb, mode, false, false).await
                };

                if flushed.is_ok() {
                    if is_restore {
                        held_restore.completed();
                    }
                    if !deferred {
                        fw_common::DISPLAY_FRAME_EPOCH.fetch_add(1, Ordering::Release);
                        fw_common::DISPLAY_FRAME_ACTIVE.store(true, Ordering::Release);
                    }
                    let flush_ms = flush_start.elapsed().as_millis();
                    if !deferred {
                        shown_has_images = shows_images;
                    }
                    if deferred {
                    } else if skipped {
                        refresh_planner.record_skipped_render(request);
                    } else {
                        refresh_planner.record_render_showing(request, mode, shows_pictures);
                        prev_fb.copy_from(fb);
                    }
                    let chapter_cursor = if request.view == AppView::Reading {
                        book_build::track_reading_chapter(
                            &mut crate::sd_session::card(&mut sdmmc_bus),
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
                    let settled_at_ms = Instant::now().as_millis();
                    if !is_restore {
                        let (display_event, power_event) =
                            app_core::display_refresh_outcome(true, chapter_cursor);
                        send_display_event(&display_event);
                        let _ = POWER_EVENTS.try_send(power_event);
                    }
                    bench_log!(
                        "bench: render view={:?} mode={:?} page={} chapter={} staged={} stage={} layout_ms={} flush_ms={} req_ms={} deq_ms={} t_ms={} skipped={} pictures={} plate={}",
                        request.view,
                        mode,
                        request.page,
                        request.chapter,
                        false,
                        "off",
                        layout_ms,
                        flush_ms,
                        request.requested_at_ms,
                        dequeued_at_ms,
                        settled_at_ms,
                        skipped,
                        shows_pictures,
                        shows_plate,
                    );
                    if !skipped {
                        esp_println::println!(
                            "s3-display: flush ok view={:?} mode={:?} t_ms={}",
                            request.view,
                            mode,
                            flush_ms,
                        );
                        let _ = display_flush::prestage_frame(&mut epd, prev_fb).await;
                    }
                } else {
                    refresh_planner.record_failure();
                    esp_println::println!(
                        "s3-display: flush err view={:?} mode={:?}",
                        request.view,
                        mode,
                    );
                    if is_restore {
                        recover_failed_restore(request, &mut held_restore).await;
                    } else {
                        let (display_event, power_event) =
                            app_core::display_refresh_outcome(false, None);
                        send_display_event(&display_event);
                        let _ = POWER_EVENTS.try_send(power_event);
                    }
                }
            }
            Either5::First(DisplayCommand::Sleep { generation, .. }) => {
                if let Some(reason) = fw_common::sleep_is_cancelled(generation) {
                    esp_println::println!(
                        "s3-display: sleep gen={} cancelled ({:?})",
                        generation,
                        reason
                    );
                    continue;
                }

                if let Some(owed) = owed_rescan.take() {
                    storage_task.abandon_rescan(owed, &mut FwHost);
                }

                let mut sleep = SleepSequence::new(STORAGE_COMMANDS.capacity());
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
                                    let portrait = last_portrait(&refresh_planner);
                                    let command = fw_common::current_frontlight_command(
                                        command,
                                        fw_common::latest_frontlight_change(),
                                    );
                                    let owed = storage_task.handle(
                                        command,
                                        &mut crate::sd_session::card(&mut sdmmc_bus),
                                        &mut FwHost,
                                        sd_library,
                                        font_metrics,
                                        &mut sync_session,
                                        portrait,
                                    );
                                    if let Some(owed) = owed {
                                        storage_task.abandon_rescan(owed, &mut FwHost);
                                    }
                                    sleep.applied();
                                    may_keep_draining = library_event_holder().sleep_may_proceed();
                                }
                                Drained::RequeueAndRefuse => {
                                    sleep.requeued(STORAGE_COMMANDS.try_send(command).is_ok());
                                }
                            },
                        },
                        SleepAction::FlushProgress => {
                            let stored = storage_task.flush_pending_progress(
                                &mut crate::sd_session::card(&mut sdmmc_bus),
                                sd_library,
                            );
                            sleep.flushed(stored);
                        }
                        SleepAction::Refuse(refusal) => break Some(refusal),
                        SleepAction::Proceed => break None,
                    }
                };

                let sleep_holds_an_event =
                    refusal.is_none() && !library_event_holder().sleep_may_proceed();
                if refusal.is_some() || sleep_holds_an_event {
                    if let Some(r) = refusal {
                        match r {
                            SleepRefusal::UploadQueued => {
                                esp_println::println!("s3-display: sleep deferred; upload queued");
                            }
                            SleepRefusal::UploadLost => {
                                esp_println::println!("s3-display: sleep deferred; upload lost");
                            }
                            SleepRefusal::ProgressUnwritten => {
                                esp_println::println!(
                                    "s3-display: sleep deferred; progress unwritten"
                                );
                            }
                        }
                    }
                    send_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                    continue;
                }

                let panel_ready =
                    if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                        match display_flush::init_panel(&mut epd).await {
                            Ok(()) => true,
                            Err(err) => {
                                esp_println::println!("s3-display: sleep init err={:?}", err);
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
                            &mut crate::sd_session::card(&mut sdmmc_bus),
                            sd_library,
                            storage_task.pending_progress.as_ref(),
                            true,
                        )
                    })
                    .map(|mut req| {
                        // Stamp the latest applied frontlight setting (which may
                        // have arrived via SetFrontlight without a Render) so a
                        // cancelled or failed sleep restores the current level.
                        if let Some((brightness, warmth)) = current_frontlight {
                            req.frontlight_brightness = brightness;
                            req.frontlight_warmth = warmth;
                        }
                        req
                    });
                if let Some(reason) = fw_common::sleep_is_cancelled(generation) {
                    esp_println::println!(
                        "s3-display: sleep gen={} cancelled before flush ({:?})",
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
                // Only a sleep committed to painting can supersede the
                // previous restoration. Cancelled/refused sleeps above retain it.
                if held_restore.supersede() {
                    esp_println::println!("s3-display: held restore superseded by sleep");
                }
                // A sleep committed to painting supersedes the boot-time wake
                // marker so an abandoned deep sleep cannot later reconstruct an
                // older sleep screen over `prev_fb`.
                woke_on_sleep_screen = None;
                // Invalidate row taps only when the sleep image is really
                // about to replace the list. A cancelled/refused sleep leaves
                // the original screen available and must not disable taps.
                fw_common::DISPLAY_FRAME_ACTIVE.store(false, Ordering::Release);
                fw_common::DISPLAY_FRAME_EPOCH.fetch_add(1, Ordering::Release);
                frontlight.off();

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
                let sleep_flush_start = Instant::now();
                let sleep_frame_settled = if panel_ready {
                    match display_flush::flush(
                        &mut epd,
                        fb,
                        prev_fb,
                        RefreshMode::Full,
                        false,
                        true,
                    )
                    .await
                    {
                        Ok(()) => {
                            prev_fb.copy_from(fb);
                            true
                        }
                        Err(err) => {
                            esp_println::println!("s3-display: sleep flush err={:?}", err);
                            false
                        }
                    }
                } else {
                    false
                };
                bench_log!(
                    "bench: sleep phase=refresh ok={} flush_ms={} t_ms={}",
                    sleep_frame_settled,
                    sleep_flush_start.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
                let panel_slept = display_flush::sleep_panel(&mut epd).await.is_ok();
                bench_log!(
                    "bench: sleep phase=deep ok={} t_ms={}",
                    panel_slept,
                    Instant::now().as_millis(),
                );
                if panel_slept {
                    refresh_planner.record_sleep(sleep_frame_settled);
                    if sleep_frame_settled {
                        refresh_planner.hold_sleep_frame();
                    }
                }
                crate::sleep_marker::record_sleep_image(
                    (panel_slept && sleep_frame_settled).then_some(sleep_screen),
                );

                if panel_slept {
                    bench_log!(
                        "bench: sleep phase=complete ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    epd.rst_mut().set_high();
                    epd.rst_mut().set_pad_hold(true);
                    let sd_pwr = sdmmc_bus.power_pin_mut();
                    sd_pwr.set_high();
                    sd_pwr.set_pad_hold(true);

                    send_display_event(&DisplayEvent::Asleep);
                    send_required_power_event(PowerEvent::DisplayAsleep(generation)).await;
                    // The power task may wait arbitrarily long for Power
                    // release. Retain both GPIO holds throughout that wait.
                    let resume_reason = park_until_resumed(generation).await;

                    epd.rst_mut().set_pad_hold(false);
                    sdmmc_bus.power_pin_mut().set_pad_hold(false);

                    // Do not use !DISPLAY_COMMANDS.is_empty() as evidence of
                    // a replacement frame: frontlight-only commands queue
                    // during a sleep cancellation but paint no pixels.
                    // Hold the restore until an actual queued Render (or
                    // another Sleep) supersedes it, or until the queue drains.
                    if let Some(req) =
                        app_core::sleep_resume_restore_render(resume_reason, request, false)
                    {
                        held_restore.hold(req);
                    }
                } else {
                    bench_log!(
                        "bench: sleep phase=complete ok=false elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    refresh_planner.record_failure();
                    if let Some(req) = request {
                        frontlight.set(req.frontlight_brightness, req.frontlight_warmth);
                        // Even a failed sleep-frame flush leaves the old
                        // display invalidated (and possibly partially painted).
                        // Reinitialize and repaint before enabling row taps.
                        held_restore.hold(req);
                    }
                    send_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                }
            }
            Either5::Second(command) => match loop_arm(&command, sync_session) {
                LoopArm::UploadSession => {
                    let new_sleep =
                        crate::sd_session::upload_session(&mut sdmmc_bus, &mut frontlight).await;
                    if let Some((brightness, warmth)) = fw_common::latest_frontlight_change() {
                        current_frontlight = Some((brightness, warmth));
                        held_restore.update_frontlight(brightness, warmth);
                    }
                    // A later wireless upload must not overwrite a sleep
                    // already waiting behind the display-command queue.
                    pending_upload_sleep = pending_upload_sleep.or(new_sleep);
                }
                LoopArm::RefusedUpload => {
                    esp_println::println!("s3-storage: upload refused outside sync");
                }
                LoopArm::Apply => {
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
                            let _ = shown_has_images;
                            shown_has_images = false;
                            flush_plate(
                                &mut epd,
                                fb,
                                prev_fb,
                                &mut refresh_planner,
                                loading_request,
                            )
                            .await;
                        }
                    }
                    let portrait = last_portrait(&refresh_planner);
                    let command = fw_common::current_frontlight_command(
                        command,
                        fw_common::latest_frontlight_change(),
                    );
                    let owed = storage_task.handle(
                        command,
                        &mut crate::sd_session::card(&mut sdmmc_bus),
                        &mut FwHost,
                        sd_library,
                        font_metrics,
                        &mut sync_session,
                        portrait,
                    );
                    if let Some(owed) = owed {
                        if let Some(note) = rescan_plate_request(&refresh_planner) {
                            storage::library_sd::ensure_folder_page(
                                &mut crate::sd_session::card(&mut sdmmc_bus),
                                sd_library,
                                note.selection,
                                portrait,
                            );
                            crate::views::render(fb, note, sd_library);
                            flush_plate(&mut epd, fb, prev_fb, &mut refresh_planner, note).await;
                        }
                        owed_rescan = Some(owed);
                    }
                }
            },
        }
    }
}

async fn flush_plate(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &mut Framebuffer,
    refresh_planner: &mut RefreshPlanner,
    request: RenderRequest,
) {
    let mode = refresh_planner.mode_for_frame(request, false);
    if refresh_planner.screen_on()
        && refresh_planner.last_request().is_some()
        && mode == RefreshMode::Fast
        && fb.bytes() == prev_fb.bytes()
    {
        return;
    }
    // A storage-driven loading or rescan plate replaces the interactive list;
    // invalidate row gestures before touching the panel so in-flight touches
    // cannot select a row from the obscured list.
    fw_common::DISPLAY_FRAME_ACTIVE.store(false, Ordering::Release);
    fw_common::DISPLAY_FRAME_EPOCH.fetch_add(1, Ordering::Release);
    if display_flush::flush(epd, fb, prev_fb, mode, false, false)
        .await
        .is_ok()
    {
        if request.view == AppView::Reading {
            refresh_planner.record_loading_plate(request, mode);
        } else {
            refresh_planner.record_render_showing(request, mode, false);
        }
        prev_fb.copy_from(fb);
        let _ = display_flush::prestage_frame(epd, prev_fb).await;
    } else {
        refresh_planner.record_failure();
    }
}

struct RescanPainter<'a> {
    epd: &'a mut Epd,
    fb: &'a mut Framebuffer,
    prev_fb: &'a mut Framebuffer,
    refresh_planner: &'a mut RefreshPlanner,
    waker: Waker,
}

impl crate::sd_session::SessionPainter for RescanPainter<'_> {
    fn paint(&mut self, percent: u8) {
        let Some(request) = self.refresh_planner.rescan_progress_frame(percent) else {
            return;
        };
        self.fb.copy_from(self.prev_fb);
        ui::app_render::render_library_rescan_progress(self.fb, request);
        poll_to_completion(
            flush_plate(
                self.epd,
                self.fb,
                self.prev_fb,
                self.refresh_planner,
                request,
            ),
            &self.waker,
        );
    }
}

async fn owed_work_due(work: Option<OwedWork>, attempts: u8) {
    let Some(work) = work else {
        return core::future::pending::<()>().await;
    };
    embassy_time::Timer::after(Duration::from_millis(owed_work_delay_ms(work, attempts))).await;
}

async fn recover_failed_restore(request: RenderRequest, held: &mut HeldSleepRestore) {
    if held.retry_failed(request) {
        // Retry a transient controller BUSY/reset failure, but do not loop
        // indefinitely on a broken panel.
        embassy_time::Timer::after(Duration::from_millis(200)).await;
    } else {
        // Synthetic restores have no app render lock. After two retries,
        // release recovery to the app's bounded repaint path.
        esp_println::println!("s3-display: synthetic restore retries exhausted");
        let (display_event, power_event) = app_core::display_refresh_outcome(false, None);
        send_display_event(&display_event);
        let _ = POWER_EVENTS.try_send(power_event);
    }
}

async fn park_until_resumed(generation: u32) -> SleepResume {
    // Do not time out: Power may be held longer than five seconds while the
    // power task waits for release. Returning early would clear the EPD reset
    // and SD power pad holds before deep sleep. A successful deep sleep never
    // returns; only an explicit cancellation can safely resume this task.
    loop {
        if let Some(reason) = fw_common::sleep_is_cancelled(generation) {
            return reason;
        }
        fw_common::DISPLAY_RESUME.wait().await;
    }
}

#[inline(never)]
fn render_reader_from_card(
    bus: &mut SdmmcBus,
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut storage::custom_font::MetricCache,
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
        return crate::sd_session::with_root(bus, |root| {
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
    crate::sd_session::with_root(bus, |root| {
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

async fn send_required_power_event(event: PowerEvent) {
    // Never evict an Activity event to make room for the acknowledgment:
    // Activity preceding DisplayAsleep must be processed first to cancel
    // sleep. The power task is receiving while this handshake is active.
    POWER_EVENTS.send(event).await;
}

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
        fw_common::BUTTON_PRESSES.load(Ordering::Relaxed)
    }

    fn waiting_on_pick(&self, request_id: u32) -> bool {
        LIBRARY_BROWSE_REQUEST_ID.load(Ordering::Relaxed) == request_id
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
        let _ = fw_common::SYNC_EVENTS.try_send(fw_common::SyncEvent::NetworkSaved(ssid));
    }

    fn wifi_storage_result(&mut self, confirmed: bool) {
        let _ = fw_common::WIFI_STORAGE_RESULTS.try_send(confirmed);
    }

    fn grant_sync_loan(
        &mut self,
        card: &mut impl storage::card::Card,
        scratch: &'static mut ReaderCacheScratch<'static>,
    ) {
        let mut loan = crate::sync_mem::dismantle_scratch(scratch);
        let stored_wifi = book_build::load_wifi_credentials(card);
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
        loan.catalog_len = storage::library_sd::write_catalog_listing(card, loan.http_b);
        if crate::SYNC_LOANS.try_send(Ok(loan)).is_err() {
            esp_println::println!("s3-storage: sync loan channel full");
        }
    }

    fn refuse_sync_loan(&mut self) {
        let _ = crate::SYNC_LOANS.try_send(Err(app_core::SyncError::Storage));
    }
}

fn ensure_epub_scratch<'a>(
    epub_scratch: &'a mut Option<&'static mut ReaderCacheScratch<'static>>,
) -> &'a mut ReaderCacheScratch<'static> {
    epub_scratch
        .as_deref_mut()
        .expect("EPUB scratch is lent only to the sync session")
}

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
