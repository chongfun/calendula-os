//! Deep-sleep handoff marker in ESP32-S3 RTC fast RAM.

use core::sync::atomic::Ordering;
use portable_atomic::AtomicU32;

const SLEEP_IMAGE_SETTLED: u32 = 0xC0DE_51EE;

#[allow(unsafe_code)] // #[ram] expands to the unsafe link_section attribute.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static SLEEP_IMAGE: AtomicU32 = AtomicU32::new(0);

/// Record whether the panel was left showing a settled sleep frame before
/// entering deep sleep.
pub fn record_sleep_image(settled: bool) {
    let value = if settled { SLEEP_IMAGE_SETTLED } else { 0 };
    SLEEP_IMAGE.store(value, Ordering::Relaxed);
}

/// Consume the retained marker from the prior deep-sleep cycle.
pub fn take_sleep_image_settled() -> bool {
    SLEEP_IMAGE.swap(0, Ordering::Relaxed) == SLEEP_IMAGE_SETTLED
}
