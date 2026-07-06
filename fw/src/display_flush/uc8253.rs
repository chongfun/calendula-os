//! UC8253 flush backend — Xteink X3. Skeleton: signatures only.
//!
//! Implementation notes live in docs/plans/2026-07-06-x3-support-plan.md
//! (Phase 2) and display/src/epd/uc8253.rs. The shape to preserve:
//! `flush` streams the new frame into DTM2 (and, when `prev_staged` is
//! false, the previous frame into DTM1), selects the LUT bank + CDI mode
//! for `mode`, activates, and waits out the X3's two-phase BUSY;
//! `prestage_previous` refills DTM1 off the page-turn critical path.

use super::{Epd, SpiError};
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::BAND_BYTES;

pub(crate) async fn init_panel(_epd: &mut Epd) {
    todo!("UC8253 init: port from CrossPoint's Uc8253X3Driver::begin")
}

pub(crate) async fn flush(
    _epd: &mut Epd,
    _fb: &Framebuffer,
    _prev_fb: &Framebuffer,
    _tx_band: &mut [u8; BAND_BYTES],
    _screen_on: bool,
    _mode: RefreshMode,
    _prev_staged: bool,
) -> Result<(), SpiError> {
    todo!("UC8253 flush: port from CrossPoint's Uc8253X3Driver::display")
}

pub(crate) async fn prestage_previous(
    _epd: &mut Epd,
    _fb: &Framebuffer,
    _tx_band: &mut [u8; BAND_BYTES],
) -> Result<(), SpiError> {
    todo!("UC8253 prestage: stage the just-shown frame into DTM1")
}

pub(crate) async fn sleep_panel(_epd: &mut Epd) -> Result<(), SpiError> {
    todo!("UC8253 sleep: port from CrossPoint's Uc8253X3Driver::deepSleep")
}
