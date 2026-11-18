use core::cell::Cell;
use core::task::{Context, Poll, Waker};

use crate::display_flush::{self, Epd};
use crate::frontlight::Frontlight;
use crate::sd_session::SdmmcBus;
use app_core::storage_loop::{
    loop_arm, owed_work, owed_work_delay_ms, storage_may_run, Drained, LoopArm, OpenAction,
    OpenSequence, OwedWork, SleepAction, SleepRefusal, SleepSequence,
};
use app_core::{
    display_orientation_from_u8, refresh_policy_from_u8, AppView, ChapterCursor, DisplayCommand,
    DisplayEvent, DisplayEventHolder, DisplayHoldOutcome, DisplayOrientation, EvictionStep,
    EvictionWalk, HoldOutcome, LibraryEvent, LibraryEventHolder, PowerEvent, RefreshPlanner,
    RenderKind, RenderRequest, SleepResume, SyncSession, SyncStatus,
};
use core::sync::atomic::Ordering;
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use embassy_futures::select::{select5, Either5};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Instant};
use fw_common::{
    ReaderSource, StorageCommand, DISPLAY_COMMANDS, DISPLAY_EVENTS, LATEST_READER_REQUEST_ID,
    LIBRARY_BROWSE_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS,
};
use proto::nvm::AppStateRecord;
use reader_cache::store::ReaderStore;
use reader_cache::{
    READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH, READER_HEADER_SCRATCH, READER_OPF_SCRATCH,
    READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use static_cell::ConstStaticCell;
use storage::book_build::{self, ReaderCacheScratch};
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
    deep_sleep_wake: bool,
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
    let mut refresh_planner = RefreshPlanner::new().with_panel_shows_sleep_screen(deep_sleep_wake);

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

    let mut pending_restore_render: Option<RenderRequest> = None;
    let mut shown_has_images = false;

    loop {
        if let Some(req) = pending_restore_render {
            if DISPLAY_COMMANDS.is_empty()
                && DISPLAY_COMMANDS
                    .try_send(DisplayCommand::Render(req))
                    .is_ok()
            {
                esp_println::println!("s3-display: queued held restore request");
                pending_restore_render = None;
            }
            // An unrelated queued command, such as SetFrontlight, cannot
            // replace the sleep frame. Only an actual Render/Sleep dequeue
            // below can supersede this held restore.
        }

        let due = owed_work(
            owed_rescan.is_some(),
            storage_task.background_owed(sd_library, refresh_planner.last_request())
                && !sd_library.text_holds_toc(),
        )
        .filter(|_| !sync_session.active() && holder().storage_may_run());

        let incoming = select5(
            DISPLAY_COMMANDS.receive(),
            storage_command_while_free(owed_rescan.is_some()),
            place_held_library_event(),
            place_held_display_event(),
            owed_work_due(
                due,
                storage_task.background_attempts(sd_library, refresh_planner.last_request()),
            ),
        )
        .await;

        match incoming {
            Either5::Fifth(()) => match due {
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
                // A held sleep-restore frame was frozen before this toggle.
                // Keep its PWM fields current so restoring the pixels does
                // not undo the frontlight change that cancelled sleep.
                if let Some(restore) = pending_restore_render.as_mut() {
                    restore.frontlight_brightness = brightness;
                    restore.frontlight_warmth = warmth;
                }
            }
            Either5::First(DisplayCommand::Render(mut request)) => {
                let dequeued_at_ms = Instant::now().as_millis();
                if pending_restore_render.take().is_some() {
                    esp_println::println!("s3-display: held restore superseded by render");
                }
                // Touches while a panel update is in progress must not
                // select rows from either the stale or the incoming list.
                fw_common::DISPLAY_FRAME_ACTIVE.store(false, Ordering::Release);
                crate::input::set_touch_context(&request);
                frontlight.set(request.frontlight_brightness, request.frontlight_warmth);

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
                        let (display_event, power_event) =
                            app_core::display_refresh_outcome(false, None);
                        send_display_event(&display_event);
                        send_required_power_event(power_event).await;
                        continue;
                    }
                }

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
                    display_flush::flush(&mut epd, fb, mode, false, false).await
                };

                if flushed.is_ok() {
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
                    let (display_event, power_event) =
                        app_core::display_refresh_outcome(true, chapter_cursor);
                    let settled_at_ms = Instant::now().as_millis();
                    send_display_event(&display_event);
                    send_required_power_event(power_event).await;
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
                    }
                } else {
                    refresh_planner.record_failure();
                    esp_println::println!(
                        "s3-display: flush err view={:?} mode={:?}",
                        request.view,
                        mode,
                    );
                    let (display_event, power_event) =
                        app_core::display_refresh_outcome(false, None);
                    send_display_event(&display_event);
                    send_required_power_event(power_event).await;
                }
            }
            Either5::First(DisplayCommand::Sleep { generation, .. }) => {
                if pending_restore_render.take().is_some() {
                    esp_println::println!("s3-display: held restore superseded by sleep");
                }
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
                                    let portrait = last_portrait(&refresh_planner);
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
                                    may_keep_draining = holder().sleep_may_proceed();
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

                let sleep_holds_an_event = refusal.is_none() && !holder().sleep_may_proceed();
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

                let request = refresh_planner.last_request().or_else(|| {
                    sleep_request_from_saved_state(
                        &mut sdmmc_bus,
                        sd_library,
                        &storage_task.pending_progress,
                    )
                });
                if let Some(reason) = fw_common::sleep_is_cancelled(generation) {
                    esp_println::println!(
                        "s3-display: sleep gen={} cancelled before flush ({:?})",
                        generation,
                        reason
                    );
                    continue;
                }

                bench_log!(
                    "bench: sleep phase=requested screen_on={} t_ms={}",
                    refresh_planner.screen_on(),
                    Instant::now().as_millis(),
                );
                // Invalidate row taps only when the sleep image is really
                // about to replace the list. A cancelled/refused sleep leaves
                // the original screen available and must not disable taps.
                fw_common::DISPLAY_FRAME_ACTIVE.store(false, Ordering::Release);
                fw_common::DISPLAY_FRAME_EPOCH.fetch_add(1, Ordering::Release);
                frontlight.off();

                if let Some(request) = request {
                    crate::views::render_sleep(fb, request, sd_library);
                } else {
                    crate::views::render_sleep_blank(fb);
                }
                let sleep_flush_start = Instant::now();
                let sleep_frame_settled = if panel_ready {
                    match display_flush::flush(&mut epd, fb, RefreshMode::Full, false, true).await {
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
                }
                crate::sleep_marker::record_sleep_image(panel_slept && sleep_frame_settled);

                if panel_slept {
                    epd.rst_mut().set_high();
                    epd.rst_mut().set_pad_hold(true);
                    let sd_pwr = sdmmc_bus.power_pin_mut();
                    sd_pwr.set_high();
                    sd_pwr.set_pad_hold(true);

                    send_display_event(&DisplayEvent::Asleep);
                    send_required_power_event(PowerEvent::DisplayAsleep(generation)).await;
                    let resume_reason = park_until_resumed(generation).await;

                    epd.rst_mut().set_pad_hold(false);
                    sdmmc_bus.power_pin_mut().set_pad_hold(false);

                    // Do not use !DISPLAY_COMMANDS.is_empty() as evidence of
                    // a replacement frame: frontlight-only commands queue
                    // during a sleep cancellation but paint no pixels.
                    // Hold the restore until an actual queued Render (or
                    // another Sleep) supersedes it, or until the queue drains.
                    if let Some(req) = resume_reason
                        .and_then(|r| app_core::sleep_resume_restore_render(r, request, false))
                    {
                        pending_restore_render = Some(req);
                    }
                } else {
                    refresh_planner.record_failure();
                    send_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                }
            }
            Either5::Second(command) => match loop_arm(&command, sync_session) {
                LoopArm::UploadSession => {
                    crate::sd_session::upload_session(&mut sdmmc_bus, &mut frontlight).await;
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
    if display_flush::flush(epd, fb, mode, false, false)
        .await
        .is_ok()
    {
        refresh_planner.record_render_showing(request, mode, false);
        prev_fb.copy_from(fb);
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

async fn own_waker() -> Waker {
    core::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await
}

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

async fn place_held_library_event() {
    let Some(event) = holder().pending() else {
        return core::future::pending::<()>().await;
    };
    LIBRARY_EVENTS.send(event).await;
    let _ = with_holder(LibraryEventHolder::placed);
}

async fn storage_command_while_free(rescan_owed: bool) -> StorageCommand {
    if !storage_may_run(holder().storage_may_run(), rescan_owed) {
        return core::future::pending::<StorageCommand>().await;
    }
    STORAGE_COMMANDS.receive().await
}

async fn owed_work_due(work: Option<OwedWork>, attempts: u8) {
    let Some(work) = work else {
        return core::future::pending::<()>().await;
    };
    embassy_time::Timer::after(Duration::from_millis(owed_work_delay_ms(work, attempts))).await;
}

async fn park_until_resumed(generation: u32) -> Option<SleepResume> {
    if let Some(reason) = fw_common::sleep_is_cancelled(generation) {
        return Some(reason);
    }
    const ABANDONED_HANDSHAKE_CEILING_SECS: u64 = 5;
    let deadline = Instant::now() + Duration::from_secs(ABANDONED_HANDSHAKE_CEILING_SECS);
    loop {
        match embassy_futures::select::select(
            fw_common::DISPLAY_RESUME.wait(),
            embassy_time::Timer::at(deadline),
        )
        .await
        {
            embassy_futures::select::Either::First(()) => {
                if let Some(reason) = fw_common::sleep_is_cancelled(generation) {
                    return Some(reason);
                }
            }
            embassy_futures::select::Either::Second(_) => {
                esp_println::println!(
                    "s3-display: no deep sleep or resume within {} s",
                    ABANDONED_HANDSHAKE_CEILING_SECS
                );
                return None;
            }
        }
    }
}

pub(crate) fn send_library_event(event: &LibraryEvent) {
    if event.must_be_delivered() {
        send_required_library_event(event);
        return;
    }
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("s3-display: library event queue full");
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
    if !ReaderSource::from_book_id(book_id).is_sd() {
        return None;
    }
    if sd_library.type_settings() == type_settings
        && sd_library.portrait() == portrait
        && (sd_library.covers_global_page(index as usize, target_pages as u32)
            || sd_library.section_indexed(index as usize, target_pages as u32))
    {
        return None;
    }
    refresh_planner.reading_plate_frame(book_id, target_pages as u32, type_settings)
}

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
                for _ in 0..walk.remaining() {
                    if let Ok(item) = LIBRARY_EVENTS.try_receive() {
                        let _ = LIBRARY_EVENTS.try_send(item);
                    }
                }
                if LIBRARY_EVENTS.try_send(*event).is_ok() {
                    return;
                }
                break;
            }
            EvictionStep::Requeue => {
                let _ = LIBRARY_EVENTS.try_send(head);
            }
        }
    }
    hold_library_event(event);
}

static HELD_LIBRARY_EVENT: Mutex<CriticalSectionRawMutex, Cell<LibraryEventHolder>> =
    Mutex::new(Cell::new(LibraryEventHolder::new()));

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

fn hold_library_event(event: &LibraryEvent) {
    let outcome = with_holder(|holder| holder.hold(event));
    if outcome != HoldOutcome::Held && LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("s3-display: dropped library event {:?}", event);
    }
}

async fn send_required_power_event(event: PowerEvent) {
    if embassy_time::with_timeout(Duration::from_millis(20), POWER_EVENTS.send(event))
        .await
        .is_err()
    {
        esp_println::println!("s3-display: dropped power event {:?}", event);
    }
}

fn send_display_event(event: &DisplayEvent) {
    if event.must_be_delivered() {
        send_required_display_event(event);
        return;
    }
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("s3-display: dropped display event {:?}", event);
    }
}

fn send_required_display_event(event: &DisplayEvent) {
    if DISPLAY_EVENTS.try_send(*event).is_ok() {
        return;
    }
    let outcome = with_display_holder(|holder| holder.hold(event));
    if outcome != DisplayHoldOutcome::Held && DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("s3-display: dropped required display event {:?}", event);
    }
}

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

async fn place_held_display_event() {
    let Some(event) = display_holder().pending() else {
        return core::future::pending::<()>().await;
    };
    DISPLAY_EVENTS.send(event).await;
    let _ = with_display_holder(DisplayEventHolder::placed);
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

fn last_portrait(planner: &RefreshPlanner) -> bool {
    planner
        .last_request()
        .map(|last| app_core::is_portrait(last.orientation))
        .unwrap_or(true)
}

fn sleep_request_from_saved_state(
    bus: &mut SdmmcBus,
    library: &mut ReaderStore,
    pending_progress: &Option<AppStateRecord>,
) -> Option<RenderRequest> {
    let (record, unflushed) = match *pending_progress {
        Some(record) => (record, true),
        None => (
            book_build::load_app_state(&mut crate::sd_session::card(bus))?,
            false,
        ),
    };
    let index = storage::library_sd::find_index_by_identity(
        &mut crate::sd_session::card(bus),
        record.source_hash,
        record.source_size,
        record.legacy_source_identity,
    )?;
    storage::library_sd::load_active_entry(
        &mut crate::sd_session::card(bus),
        library,
        usize::from(index),
    );
    let (chapter, screen) = if unflushed {
        (record.chapter, record.screen)
    } else {
        storage::task::book_position(&mut crate::sd_session::card(bus), library, index, record)
    };
    book_build::load_chapter_title(
        &mut crate::sd_session::card(bus),
        usize::from(index),
        chapter,
        library,
    );
    let page_count = book_build::restore_book_page_count(
        &mut crate::sd_session::card(bus),
        usize::from(index),
        library,
    );
    Some(RenderRequest {
        kind: RenderKind::Page,
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
        alignment: display::font::Alignment::from_u8(record.alignment)
            .unwrap_or(display::font::Alignment::Justified),
        frontlight_available: true,
        frontlight_brightness: 0,
        frontlight_warmth: 0,
        last_button: None,
        aux_raw: 0,
        nav_raw: 0,
        page_raw: 0,
        battery_mv: 0,
        battery_percent: 100,
        library_count: library.catalog_count_u16(),
        chapters_expanded: None,
        sync_status: SyncStatus::NotConfigured,
        wifi_ssid: [0; 32],
        wifi_ssid_len: 0,
        dirty: display::Rect::FULL,
    })
}
