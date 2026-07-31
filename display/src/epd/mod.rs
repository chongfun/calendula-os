//! Panel-controller drivers behind one compile-time seam.
//!
//! The shared surface is deliberately small: `RefreshMode` (the contract
//! with `app_core::RefreshPlanner` and the emulators — never forked per
//! panel), `SpiOp` for table-driven command sequences. Everything
//! controller-specific — command bytes, init/update sequences, RAM-window
//! math, waveform handling — lives in the per-panel module re-exported
//! here, so firmware and tools import `display::epd::…` regardless of the
//! selected device.

/// Xteink X4: GDEQ0426T82 panel, SSD1677 controller. Also the panel the
/// desktop emulator simulates.
pub mod ssd1677;

/// Xteink X3: 792x528 panel, UC8253 controller. Fully implemented and
/// hardware-verified.
#[cfg(feature = "device-x3")]
pub mod uc8253;

#[cfg(not(feature = "device-x3"))]
pub use ssd1677::*;
#[cfg(feature = "device-x3")]
pub use uc8253::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    Full,
    Fast,
    /// One-flicker cleaning refresh: cleans ghosting in roughly half the
    /// full refresh time at a small contrast cost. Each controller
    /// realizes it differently (SSD1677: hot temperature-override OTP
    /// waveform; UC8253: half-scrub LUT bank).
    FastClean,
    PowerDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpiOp {
    Reset,
    WaitBusy,
    Command { cmd: u8, data: &'static [u8] },
}
