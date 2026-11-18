use crate::input;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::{Event, WakeupConfig};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::sleep::{LowPower, RtcSleepConfig};
use fw_common::{
    signal_sleep_cancelled, AppView, DisplayCommand, PowerEvent, SleepResume, SleepTrigger,
    DISPLAY_COMMANDS, POWER_EVENTS,
};

const fn idle_timeout(view: AppView) -> Duration {
    Duration::from_secs(app_core::idle_timeout_secs(view))
}

#[embassy_executor::task]
pub async fn run(lpwr: LPWR<'static>) {
    esp_println::println!("s3-power: started");
    let mut idle = idle_timeout(AppView::Home);
    let mut deadline = Instant::now() + idle;
    let mut sleep_generation: u32 = 0;

    loop {
        match select(POWER_EVENTS.receive(), Timer::at(deadline)).await {
            Either::First(event) => match event {
                PowerEvent::Activity(view) => {
                    idle = idle_timeout(view);
                    deadline = Instant::now() + idle;
                }
                PowerEvent::BrowserActivity => {
                    idle = idle_timeout(AppView::Wireless);
                    deadline = Instant::now() + idle;
                }
                PowerEvent::SleepNow => {
                    sleep_generation = sleep_generation.wrapping_add(1);
                    let res = enter_sleep(sleep_generation, SleepTrigger::Explicit);
                    match res.await {
                        SleepOutcome::Continue(new_idle) => {
                            idle = new_idle;
                            deadline = Instant::now() + idle;
                        }
                        SleepOutcome::DeepSleep => deep_sleep(lpwr).await,
                    }
                }
                PowerEvent::DisplaySettled
                | PowerEvent::DisplayAsleep(_)
                | PowerEvent::DisplayRefreshFailed
                | PowerEvent::DisplaySleepFailed(_) => {}
            },
            Either::Second(()) => {
                if fw_common::upload::UPLOAD_IN_FLIGHT.load(portable_atomic::Ordering::SeqCst) {
                    esp_println::println!("s3-power: idle timeout deferred; upload in flight");
                    idle = idle_timeout(AppView::Wireless);
                    deadline = Instant::now() + idle;
                    continue;
                }
                esp_println::println!("s3-power: idle timeout");
                sleep_generation = sleep_generation.wrapping_add(1);
                match enter_sleep(sleep_generation, SleepTrigger::Idle).await {
                    SleepOutcome::Continue(new_idle) => {
                        idle = new_idle;
                        deadline = Instant::now() + idle;
                    }
                    SleepOutcome::DeepSleep => deep_sleep(lpwr).await,
                }
            }
        }
    }
}

enum SleepOutcome {
    Continue(Duration),
    DeepSleep,
}

async fn enter_sleep(generation: u32, mut trigger: SleepTrigger) -> SleepOutcome {
    esp_println::println!("s3-power: display sleep gen={}", generation);
    DISPLAY_COMMANDS
        .send(DisplayCommand::Sleep {
            generation,
            trigger,
        })
        .await;

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::DisplayAsleep(acked) if acked == generation => {
                esp_println::println!("s3-power: deep sleep gen={}", generation);
                return SleepOutcome::DeepSleep;
            }
            PowerEvent::Activity(view) => {
                signal_sleep_cancelled(generation, SleepResume::Activity(view));
                return SleepOutcome::Continue(idle_timeout(view));
            }
            PowerEvent::BrowserActivity if trigger == SleepTrigger::Idle => {
                signal_sleep_cancelled(generation, SleepResume::BrowserActivity);
                return SleepOutcome::Continue(idle_timeout(AppView::Wireless));
            }
            PowerEvent::DisplaySleepFailed(acked) if acked == generation => {
                esp_println::println!("s3-power: display sleep failed; staying awake");
                return SleepOutcome::Continue(idle_timeout(AppView::Home));
            }
            PowerEvent::SleepNow => {
                trigger = SleepTrigger::Explicit;
            }
            PowerEvent::DisplayAsleep(_)
            | PowerEvent::DisplaySleepFailed(_)
            | PowerEvent::DisplayRefreshFailed
            | PowerEvent::DisplaySettled
            | PowerEvent::BrowserActivity => {}
        }
    }
}

async fn deep_sleep(lpwr: LPWR<'static>) -> ! {
    let mut wake = input::take_wake_pins().await;
    wake.touch_power.set_high();
    wake.touch_power.set_pad_hold(true);

    while wake.power.is_low() {
        Timer::after_millis(10).await;
    }

    if let Err(err) = wake
        .power
        .apply_wakeup_config(&WakeupConfig::default().with_low_power_path(true))
    {
        esp_println::println!("s3-sleep: wakeup config err={:?}", err);
    }
    wake.power.listen(Event::LowLevel);

    esp_println::println!("s3-sleep: entering deep sleep");
    esp_println::Printer::write_bytes(b"");
    Timer::after_millis(20).await;

    let mut low_power = LowPower::new(lpwr);
    #[cfg(feature = "bench-selftest")]
    low_power.set_wakeup_deadline(
        esp_hal::time::Instant::now()
            + esp_hal::time::Duration::from_secs(crate::bench_selftest::SLEEP_WAKE_AFTER.as_secs()),
    );
    low_power.sleep_deep(RtcSleepConfig::deep());
}
