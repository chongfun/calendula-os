use crate::{
    AppView, DisplayCommand, PowerEvent, DISPLAY_COMMANDS, POWER_EVENTS, WAKE_PIN_HANDOFF,
    WAKE_PIN_REQUESTS,
};
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO3, LPWR};
use esp_hal::rtc_cntl::Rtc;

/// Idle time with no input before the device puts itself into deep sleep,
/// tiered by the view the last input left the app in. Reading keeps the
/// long leash for slow readers; the shell views sleep much sooner — the
/// seeded deep-sleep wake is a ~1.5 s one-flicker refresh, so an early
/// sleep costs the user little and saves the 160 MHz / 66 Hz-polling idle
/// tail (order 10–20 mA) on every walk-away. Wireless keeps the long
/// leash too: a running sync session is legitimately button-free.
const READING_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const SHELL_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

const fn idle_timeout(view: AppView) -> Duration {
    match view {
        AppView::Reading | AppView::Wireless => READING_IDLE_TIMEOUT,
        AppView::Home | AppView::Library | AppView::Chapters | AppView::Settings => {
            SHELL_IDLE_TIMEOUT
        }
    }
}

#[embassy_executor::task]
pub async fn run(lpwr: LPWR<'static>) {
    esp_println::println!("power: started");
    let mut rtc = Rtc::new(lpwr);
    // Boot lands on the Home shell, so start on the short leash.
    let mut idle = idle_timeout(AppView::Home);
    let mut deadline = Instant::now() + idle;

    loop {
        match select(POWER_EVENTS.receive(), Timer::at(deadline)).await {
            Either::First(event) => match event {
                // Any input pushes the idle deadline back out, at the leash
                // of the view the input landed in.
                PowerEvent::Activity(view) => {
                    idle = idle_timeout(view);
                    deadline = Instant::now() + idle;
                }
                // Power button: sleep on demand.
                PowerEvent::SleepNow => {
                    // Only reached if a late button press aborted the
                    // handshake; resume on that press's idle tier.
                    idle = enter_sleep(&mut rtc).await;
                    deadline = Instant::now() + idle;
                }
                PowerEvent::DisplaySettled
                | PowerEvent::DisplayAsleep
                | PowerEvent::DisplayFailed => {}
            },
            // Idle timeout elapsed with no activity.
            Either::Second(_) => {
                esp_println::println!("power: idle timeout");
                idle = enter_sleep(&mut rtc).await;
                deadline = Instant::now() + idle;
            }
        }
    }
}

/// Renders the sleep screen, lets the display flush in-flight work and any
/// pending reading progress, then powers the SoC down into deep sleep with the
/// Power button as the wake source.
///
/// Deep sleep is terminal — the chip reboots on wake — so this only returns if
/// a button press arrives during the display handshake and aborts the
/// transition, and the returned value is the idle tier of the view that press
/// landed in, so the resumed idle clock ticks at the cancelling view's leash.
/// Waiting for `DisplayAsleep` before cutting power guarantees the e-ink panel
/// has settled on its sleep image and progress is safely on the SD card.
async fn enter_sleep(rtc: &mut Rtc<'_>) -> Duration {
    esp_println::println!("power: display sleep");
    DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::DisplayAsleep => {
                esp_println::println!("power: deep sleep");
                let mut button = take_wake_button().await;
                hal_ext::rtc::enter_deep_sleep_button(rtc, &mut button);
            }
            PowerEvent::Activity(view) => return idle_timeout(view),
            PowerEvent::DisplayFailed => {
                // The display task could not complete the sleep transition
                // (progress flush or panel handshake failed). Cutting power
                // anyway would lose reading position or freeze a mid-refresh
                // panel; stay awake and retry at the shell leash.
                esp_println::println!("power: display sleep failed; staying awake");
                return idle_timeout(AppView::Home);
            }
            PowerEvent::DisplaySettled | PowerEvent::SleepNow => {}
        }
    }
}

/// Takes sole ownership of the Power button (GPIO3) and re-materialises it as
/// a deep-sleep wake source.
///
/// The input task owns GPIO3 as an `Input<'static>` and polls it for the whole
/// run, so the pin has to change hands before it can be armed: this asks the
/// input task to stop polling and surrender that handle, waits for it, and
/// drops it. Only then is this task the pin's single owner, which is what
/// makes the steal below sound. The wait costs the terminal sleep path about
/// one 15 ms poll tick, plus any in-progress sample.
///
/// Dropping the `Input` leaves the pad configured as it was — esp-hal's pin
/// drivers have no `Drop` glue — so the button's pull-up survives the gap
/// until the wake source re-enables it.
async fn take_wake_button() -> GPIO3<'static> {
    WAKE_PIN_REQUESTS.send(()).await;
    // The received handle is a temporary: its scope ends at this semicolon,
    // which retires the last `Input` on GPIO3 before the steal below.
    WAKE_PIN_HANDOFF.receive().await;
    steal_wake_button()
}

/// Re-materialises the Power button (GPIO3) once its previous handle is gone.
#[expect(
    unsafe_code,
    reason = "Caller guarantees exclusive GPIO3 ownership via handoff protocol"
)]
fn steal_wake_button() -> GPIO3<'static> {
    // SAFETY: the caller has taken the input task's `Input<'static>` handle on
    // GPIO3 and dropped it, and the input task stops polling the pin before it
    // gives that handle up, so no other handle on GPIO3 is live. Reached only
    // on the terminal deep-sleep path, which never returns the pin to the
    // input task: the chip resets on wake.
    unsafe { GPIO3::steal() }
}
