use crate::{POWER_EVT, PowerEvent};
use embassy_time::Timer;
use esp_hal::peripherals::LPWR;

#[embassy_executor::task]
pub async fn run(_lpwr: LPWR) {
    esp_println::println!("Power management task started!");
    loop {
        match POWER_EVT.receive().await {
            PowerEvent::PageRendered => {
                // Allow display particles to settle completely
                Timer::after_millis(500).await;

                // Enter Deep Sleep state to conserve battery.
                // In actual deployment, this calls:
                // hal_ext::rtc::enter_deep_sleep(rtc, 3, false); // GPIO3 low triggers wake
            }
            PowerEvent::WifiSyncRequired => {
                // Enter light sleep until WiFi completes synchronization
            }
            PowerEvent::GoToSleep => {}
            PowerEvent::WakeUp => {}
        }
    }
}
