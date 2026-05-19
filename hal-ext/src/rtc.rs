use esp_hal::rtc_cntl::Rtc;

/// Puts the CPU and digital core into deep sleep, leaving only the RTC subsystem active.
pub fn enter_deep_sleep(mut rtc: Rtc) -> ! {
    let wakeup_source = esp_hal::rtc_cntl::sleep::TimerWakeupSource::new(
        core::time::Duration::from_secs(10),
    );
    rtc.sleep_deep(&[&wakeup_source]);
}

/// Puts the CPU into a light sleep clock-gated state, keeping DRAM context intact.
pub fn enter_light_sleep(mut rtc: Rtc) {
    let wakeup_source = esp_hal::rtc_cntl::sleep::TimerWakeupSource::new(
        core::time::Duration::from_millis(100),
    );
    rtc.sleep_light(&[&wakeup_source]);
}
