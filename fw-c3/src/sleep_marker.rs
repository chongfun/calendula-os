//! Deep-sleep handoff marker in RTC fast RAM.
//!
//! Deep sleep is terminal — waking reboots the chip — so a boot can only
//! consult the RTC wake cause and RTC memory. The wake cause alone proves
//! *how* the chip woke, not *what* the panel shows: the sleep handshake
//! still releases the power task to cut power when the sleep-frame flush or
//! panel-sleep command fails, because staying awake on a wedged SPI bus
//! would drain the battery. This marker records whether that final flush
//! actually settled, a 30-bit digest of the sleep screen's pixels, and
//! whether the sleep began on a reading page, so the next boot trusts the
//! panel contents only when both hold: woke by the armed source, and the
//! sleep frame really landed.
//!
//! The digest lets the first paint be a page turn. A fast refresh drives
//! each pixel from the previous frame to the new one, so it needs the
//! pixels on the panel, and the framebuffer that held them is gone. The
//! display task draws the sleep screen again from the restored state and
//! trusts it as the previous frame only if it digests to the same value;
//! otherwise the first paint takes the clean that needs no previous frame.

pub use app_core::{frame_digest, SleepScreen};
use core::sync::atomic::Ordering;
use display::fb::Framebuffer;
use portable_atomic::AtomicU32;

// `persistent`: zeroed once on the first power-on, then left untouched by
// the runtime across deep sleep and every reset — that retention is what
// carries the value over the deep-sleep reboot.
#[allow(unsafe_code)] // #[ram] expands to the unsafe link_section attribute.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static SLEEP_IMAGE: AtomicU32 = AtomicU32::new(0);

/// Whether `fb` is the sleep screen `screen` was taken from.
pub fn matches(screen: SleepScreen, fb: &Framebuffer) -> bool {
    screen.matches(fb)
}

/// Records the finished sleep screen now on the panel, or `None` when the
/// panel holds anything else. The display task calls this just before
/// `DisplayAsleep` releases the power task to cut power.
pub fn record_sleep_image(screen: Option<SleepScreen>) {
    SLEEP_IMAGE.store(SleepScreen::encode(screen), Ordering::Relaxed);
}

/// Consumes the marker: the settled sleep frame left on the panel by the
/// previous shutdown, if any. Clears it so one recorded sleep image can
/// never vouch for more than one boot.
pub fn take_sleep_screen() -> Option<SleepScreen> {
    SleepScreen::decode(SLEEP_IMAGE.swap(0, Ordering::Relaxed))
}
