use esp_hal::peripherals::WIFI;
use embassy_time::Timer;

#[embassy_executor::task]
pub async fn run(_wifi: WIFI) {
    esp_println::println!("WiFi background task started!");
    loop {
        // Run esp-wifi protocol engine in the background
        Timer::after_secs(30).await;
    }
}
