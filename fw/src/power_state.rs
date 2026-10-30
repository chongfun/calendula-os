//! Sleep coordination between the tasks and the power task.
//!
//! Light sleep halts the whole chip, so the one invariant is that nothing
//! needing continuous CPU or a live bus clock is in flight when a slice
//! begins. Subsystems say so by holding a [`SleepInhibitGuard`]; the input
//! task publishes when it last saw a key and whether a change is still being
//! debounced; the USB-Serial-JTAG SOF index is sampled into a tri-state host
//! verdict. The power task reads all of it and is the only caller of
//! `hal_ext::rtc::light_sleep_timer` (see `tasks::power`).
//!
//! The decisions live in `app_core::light_sleep` under host tests; this
//! module is the atomics, the register read, and the RAII guard. Everything
//! here is live in every build. Only the admission policy in the power task
//! is behind the `light-sleep` feature, so a build without it carries the
//! same guards and the same bookkeeping and differs only in never sleeping.

use app_core::light_sleep::{inhibit_acquire, inhibit_release, UsbHostState, UsbSofDetector};
use core::cell::RefCell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::Instant;
use portable_atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Live [`SleepInhibitGuard`]s. Saturating in both directions (see
/// `app_core::light_sleep::inhibit_acquire`).
static SLEEP_INHIBITORS: AtomicU32 = AtomicU32::new(0);

/// The USB host verdict, as `UsbHostState::code`.
static USB_STATE: AtomicU8 = AtomicU8::new(0);

/// The detector behind [`sample_usb`]; the input task feeds it each tick.
static USB_DETECTOR: Mutex<CriticalSectionRawMutex, RefCell<UsbSofDetector>> =
    Mutex::new(RefCell::new(UsbSofDetector::new()));

/// Embassy milliseconds of the last input the input task emitted.
static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);

/// Whether a button change is inside its debounce window.
static DEBOUNCE_PENDING: AtomicBool = AtomicBool::new(false);

/// Raised by the power task after each slice so the input task samples at
/// once instead of waiting out a 15 ms Embassy tick that did not advance
/// while the chip slept.
pub static POLL_NUDGE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Holds the chip awake. Zero-sized, allocation-free: creating one
/// increments the global count, dropping it decrements. Hold it across the
/// awaits of anything that must run to completion on a live bus clock: an
/// SD session, a display DMA flush and its BUSY wait, the gauge's I2C read,
/// the radio session, one input tick's ADC conversions.
#[must_use = "a dropped guard releases the inhibit at once"]
pub struct SleepInhibitGuard {
    _private: (),
}

/// Take a sleep inhibitor for the guard's lifetime.
pub fn inhibit_light_sleep() -> SleepInhibitGuard {
    // fetch_update rather than fetch_add so the count saturates instead of
    // wrapping; see the policy module for why wrapping is the one bug that
    // must not happen here.
    let _ = SLEEP_INHIBITORS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        Some(inhibit_acquire(n))
    });
    SleepInhibitGuard { _private: () }
}

impl Drop for SleepInhibitGuard {
    fn drop(&mut self) {
        let _ = SLEEP_INHIBITORS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(inhibit_release(n))
        });
    }
}

/// Live inhibitor count. Read inside the admission critical section; a
/// value read outside it is only advisory.
pub fn inhibitor_count() -> u32 {
    SLEEP_INHIBITORS.load(Ordering::SeqCst)
}

pub fn light_sleep_inhibited() -> bool {
    inhibitor_count() != 0
}

pub fn usb_state() -> UsbHostState {
    UsbHostState::from_code(USB_STATE.load(Ordering::SeqCst))
}

/// Feed the detector one SOF frame-index read. Returns the new verdict when
/// it changed. Called from the input task every 15 ms tick; the detector
/// ignores samples closer than its window, so a faster caller is harmless.
pub fn sample_usb(sof_frame_index: u16, now: Instant) -> Option<UsbHostState> {
    let changed =
        USB_DETECTOR.lock(|cell| cell.borrow_mut().sample(sof_frame_index, now.as_millis()));
    if let Some(state) = changed {
        USB_STATE.store(state.code(), Ordering::SeqCst);
    }
    changed
}

/// The USB-Serial-JTAG peripheral's last received SOF frame index. A host
/// advances it every millisecond while the bus is enumerated; with no host
/// it holds still. One register read, safe code: the field reader is
/// read-only.
pub fn read_usb_sof_frame_index() -> u16 {
    esp_hal::peripherals::USB_DEVICE::regs()
        .fram_num()
        .read()
        .sof_frame_index()
        .bits()
}

/// Record that the input task emitted a key event now.
pub fn note_input_activity(now: Instant) {
    LAST_INPUT_MS.store(now.as_millis(), Ordering::SeqCst);
}

/// Embassy milliseconds of the last emitted key event; zero before the first.
pub fn last_input_ms() -> u64 {
    LAST_INPUT_MS.load(Ordering::SeqCst)
}

pub fn set_debounce_pending(pending: bool) {
    DEBOUNCE_PENDING.store(pending, Ordering::SeqCst);
}

pub fn is_debounce_pending() -> bool {
    DEBOUNCE_PENDING.load(Ordering::SeqCst)
}

/// Whether this build carries the static-page light-sleep policy. The
/// chip supports it on both boards; the gate is the `light-sleep` feature,
/// off until the Phase 0 selftest has passed on the board in question (the
/// X3 is the only one with hardware to run it on). See `fw/Cargo.toml`.
pub const fn device_supports_light_sleep() -> bool {
    cfg!(feature = "light-sleep")
}
