//! Deep-sleep handoff marker in ESP32-S3 RTC fast RAM.

pub use app_core::{frame_digest, SleepScreen};
use core::sync::atomic::Ordering;
use display::fb::Framebuffer;
use portable_atomic::AtomicU32;

#[allow(unsafe_code)] // #[ram] expands to the unsafe link_section attribute.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static SLEEP_IMAGE: AtomicU32 = AtomicU32::new(0);

/// Whether `fb` is the sleep screen `screen` was taken from.
pub fn matches(screen: SleepScreen, fb: &Framebuffer) -> bool {
    screen.matches(fb)
}

/// Record the finished sleep screen now on the panel before entering deep
/// sleep, or `None` when the panel holds anything else.
pub fn record_sleep_image(screen: Option<SleepScreen>) {
    SLEEP_IMAGE.store(SleepScreen::encode(screen), Ordering::Relaxed);
}

/// Consume the retained marker from the prior deep-sleep cycle.
pub fn take_sleep_screen() -> Option<SleepScreen> {
    SleepScreen::decode(SLEEP_IMAGE.swap(0, Ordering::Relaxed))
}
