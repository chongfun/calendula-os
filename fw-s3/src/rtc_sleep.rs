use embassy_time::Timer;
use esp_hal::gpio::{Event, Output, WakeupConfig};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::sleep::{LowPower, RtcSleepConfig};

use crate::display_flush::{self, Epd};
use crate::frontlight::Frontlight;
use crate::input;

pub async fn enter_deep_sleep(
    lpwr: LPWR<'static>,
    epd: &mut Epd,
    sdmmc_power: &mut Output<'static>,
    frontlight: &mut Frontlight,
) -> ! {
    frontlight.off();

    if let Err(err) = display_flush::sleep_panel(epd).await {
        esp_println::println!("s3-sleep: panel sleep err={:?}", err);
    }

    let mut wake = input::take_wake_pins().await;

    // Latch panel reset and active-low power rails high (idle/off) across
    // deep sleep, matching board::SLEEP_PAD_SPECS.
    epd.rst_mut().set_high();
    epd.rst_mut().set_pad_hold(true);

    sdmmc_power.set_high();
    sdmmc_power.set_pad_hold(true);

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
    low_power.sleep_deep(RtcSleepConfig::deep());
}
