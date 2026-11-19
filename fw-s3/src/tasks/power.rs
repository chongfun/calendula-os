use crate::input;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::{Event, WakeupConfig};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::sleep::{LowPower, RtcSleepConfig};
use fw_common::{
    power_activity_snapshot, signal_sleep_cancelled, AppView, DisplayCommand,
    PowerActivitySnapshot, PowerEvent, SleepResume, SleepTrigger, DISPLAY_COMMANDS, POWER_EVENTS,
};

const fn idle_timeout(view: AppView) -> Duration {
    Duration::from_secs(app_core::idle_timeout_secs(view))
}

#[embassy_executor::task]
pub async fn run(mut lpwr: LPWR<'static>) {
    esp_println::println!("s3-power: started");
    let mut idle = idle_timeout(AppView::Home);
    let mut deadline = Instant::now() + idle;
    let mut sleep_generation: u32 = 0;
    let mut seen_activity = power_activity_snapshot();

    loop {
        match select(POWER_EVENTS.receive(), Timer::at(deadline)).await {
            Either::First(event) => match event {
                PowerEvent::Activity(view, seq) => {
                    // A queued event can already be represented in the
                    // snapshot after canceled sleep. Ignore its idle tier
                    // unless its sequence is genuinely new.
                    if let Some(updated) = seen_activity.accept_ui(seq, view) {
                        seen_activity = updated;
                        idle = idle_timeout(view);
                        deadline = Instant::now() + idle;
                    }
                }
                PowerEvent::BrowserActivity(seq) => {
                    if let Some(updated) = seen_activity.accept_browser(seq) {
                        seen_activity = updated;
                        idle = idle_timeout(AppView::Wireless);
                        deadline = Instant::now() + idle;
                    }
                }
                PowerEvent::SleepNow {
                    ui_seq,
                    browser_seq,
                } => {
                    sleep_generation = sleep_generation.wrapping_add(1);
                    let res = enter_sleep(
                        sleep_generation,
                        SleepTrigger::Explicit,
                        idle,
                        PowerActivitySnapshot::for_sleep_request(ui_seq, browser_seq),
                    );
                    match res.await {
                        SleepOutcome::Continue(new_idle) => {
                            let latest = power_activity_snapshot();
                            // A cancellation may consume only the older of
                            // several activities. Derive the leash from the
                            // latest activity before acknowledging the batch.
                            idle = idle_after_sleep(seen_activity, latest, new_idle);
                            seen_activity = latest;
                            deadline = Instant::now() + idle;
                        }
                        SleepOutcome::DeepSleep {
                            activity_before_sleep,
                            trigger,
                        } => {
                            let (returned_lpwr, new_idle) =
                                deep_sleep(lpwr, sleep_generation, trigger, activity_before_sleep)
                                    .await;
                            lpwr = returned_lpwr;
                            idle = new_idle;
                            seen_activity = power_activity_snapshot();
                            deadline = Instant::now() + idle;
                        }
                    }
                }
                PowerEvent::DisplaySettled
                | PowerEvent::DisplayAsleep(_)
                | PowerEvent::DisplayRefreshFailed
                | PowerEvent::DisplaySleepFailed(_) => {}
            },
            Either::Second(()) => {
                let latest = power_activity_snapshot();
                if let Some(reason) = seen_activity.cancellation_since(latest, SleepTrigger::Idle) {
                    idle = match reason {
                        SleepResume::Activity(view) => idle_timeout(view),
                        _ => idle_timeout(AppView::Wireless),
                    };
                    seen_activity = latest;
                    deadline = Instant::now() + idle;
                    continue;
                }
                if fw_common::upload::UPLOAD_IN_FLIGHT.load(portable_atomic::Ordering::SeqCst) {
                    esp_println::println!("s3-power: idle timeout deferred; upload in flight");
                    idle = idle_timeout(AppView::Wireless);
                    deadline = Instant::now() + idle;
                    continue;
                }
                esp_println::println!("s3-power: idle timeout");
                sleep_generation = sleep_generation.wrapping_add(1);
                match enter_sleep(sleep_generation, SleepTrigger::Idle, idle, seen_activity).await {
                    SleepOutcome::Continue(new_idle) => {
                        let latest = power_activity_snapshot();
                        idle = idle_after_sleep(seen_activity, latest, new_idle);
                        seen_activity = latest;
                        deadline = Instant::now() + idle;
                    }
                    SleepOutcome::DeepSleep {
                        activity_before_sleep,
                        trigger,
                    } => {
                        let (returned_lpwr, new_idle) =
                            deep_sleep(lpwr, sleep_generation, trigger, activity_before_sleep).await;
                        lpwr = returned_lpwr;
                        idle = new_idle;
                        seen_activity = power_activity_snapshot();
                        deadline = Instant::now() + idle;
                    }
                }
            }
        }
    }
}

enum SleepOutcome {
    Continue(Duration),
    DeepSleep {
        activity_before_sleep: PowerActivitySnapshot,
        trigger: SleepTrigger,
    },
}

/// If multiple notifications accumulated during a sleep handshake, use the
/// latest producer-observed activity rather than the first queued event.
fn idle_after_sleep(
    before: PowerActivitySnapshot,
    latest: PowerActivitySnapshot,
    fallback: Duration,
) -> Duration {
    match before.cancellation_since(latest, SleepTrigger::Idle) {
        Some(SleepResume::Activity(view)) => idle_timeout(view),
        Some(SleepResume::BrowserActivity) => idle_timeout(AppView::Wireless),
        _ => fallback,
    }
}

async fn enter_sleep(
    generation: u32,
    mut trigger: SleepTrigger,
    fallback_idle: Duration,
    mut activity_before_sleep: PowerActivitySnapshot,
) -> SleepOutcome {
    esp_println::println!("s3-power: display sleep gen={}", generation);
    // The explicit request uses the activity stamp captured by the app when
    // Power was pressed, not a snapshot taken after waiting for queue space.
    DISPLAY_COMMANDS
        .send(DisplayCommand::Sleep {
            generation,
            trigger,
        })
        .await;

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::DisplayAsleep(acked) if acked == generation => {
                // Activity may arrive even when its notification cannot fit
                // in POWER_EVENTS. The snapshot must outrank the sleep ack.
                if let Some(reason) =
                    activity_before_sleep.cancellation_since(power_activity_snapshot(), trigger)
                {
                    signal_sleep_cancelled(generation, reason);
                    let idle = match reason {
                        SleepResume::Activity(view) => idle_timeout(view),
                        _ => idle_timeout(AppView::Wireless),
                    };
                    return SleepOutcome::Continue(idle);
                }
                esp_println::println!("s3-power: deep sleep gen={}", generation);
                return SleepOutcome::DeepSleep {
                    activity_before_sleep,
                    trigger,
                };
            }
            PowerEvent::Activity(view, seq) => {
                if activity_before_sleep.accept_ui(seq, view).is_some() {
                    signal_sleep_cancelled(generation, SleepResume::Activity(view));
                    return SleepOutcome::Continue(idle_timeout(view));
                }
            }
            PowerEvent::BrowserActivity(seq) if trigger == SleepTrigger::Idle => {
                if activity_before_sleep.accept_browser(seq).is_some() {
                    signal_sleep_cancelled(generation, SleepResume::BrowserActivity);
                    return SleepOutcome::Continue(idle_timeout(AppView::Wireless));
                }
            }
            PowerEvent::DisplaySleepFailed(acked) if acked == generation => {
                esp_println::println!("s3-power: display sleep failed; staying awake");
                return SleepOutcome::Continue(fallback_idle);
            }
            PowerEvent::SleepNow {
                ui_seq,
                browser_seq,
            } => {
                // A later explicit Power press supersedes an idle request.
                // Inputs preceding the press must not cancel its intent.
                trigger = SleepTrigger::Explicit;
                activity_before_sleep =
                    PowerActivitySnapshot::for_sleep_request(ui_seq, browser_seq);
            }
            PowerEvent::DisplayAsleep(_)
            | PowerEvent::DisplaySleepFailed(_)
            | PowerEvent::DisplayRefreshFailed
            | PowerEvent::DisplaySettled
            | PowerEvent::BrowserActivity(_) => {}
        }
    }
}

/// The panel is already asleep, but wake-pin handoff and Power release can
/// take arbitrarily long. Continue monitoring other input until the final
/// generation check. If activity arrived, restore the display and return the
/// wake pins so the input task can resume normal operation.
async fn deep_sleep(
    lpwr: LPWR<'static>,
    generation: u32,
    trigger: SleepTrigger,
    activity_before_sleep: PowerActivitySnapshot,
) -> (LPWR<'static>, Duration) {
    let mut wake = input::take_wake_pins().await;

    // Keep input_task polling Home, Left, Right and touch throughout this
    // interval; only the Power and touch-power GPIO handles are loaned.
    while wake.power.is_low() {
        Timer::after_millis(10).await;
    }

    // Allow a final input poll before committing power-down. Nothing may
    // await between the last generation check and sleep_deep().
    Timer::after_millis(20).await;
    if let Some(reason) =
        activity_before_sleep.cancellation_since(power_activity_snapshot(), trigger)
    {
        esp_println::println!(
            "s3-sleep: gen={} cancelled after wake-pin handoff ({:?})",
            generation,
            reason
        );
        signal_sleep_cancelled(generation, reason);
        input::return_wake_pins(wake).await;
        let idle = match reason {
            SleepResume::Activity(view) => idle_timeout(view),
            _ => idle_timeout(AppView::Wireless),
        };
        return (lpwr, idle);
    }

    // Pin ownership is no longer needed by the input task on the terminal
    // path. Apply both holds only after cancellation is no longer possible.
    wake.touch_power.set_high();
    wake.touch_power.set_pad_hold(true);
    if let Err(err) = wake
        .power
        .apply_wakeup_config(&WakeupConfig::default().with_low_power_path(true))
    {
        esp_println::println!("s3-sleep: wakeup config err={:?}", err);
    }
    wake.power.listen(Event::LowLevel);

    esp_println::println!("s3-sleep: entering deep sleep");
    esp_println::Printer::write_bytes(b"");

    let mut low_power = LowPower::new(lpwr);
    #[cfg(feature = "bench-selftest")]
    low_power.set_wakeup_deadline(
        esp_hal::time::Instant::now()
            + esp_hal::time::Duration::from_secs(crate::bench_selftest::SLEEP_WAKE_AFTER.as_secs()),
    );
    low_power.sleep_deep(RtcSleepConfig::deep());
}
