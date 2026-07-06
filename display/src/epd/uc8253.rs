//! UC8253 panel driver — Xteink X3 (792x528). Skeleton.
//!
//! Not implemented yet: this module exists so the device-x3 build has the
//! seam in place. Port the controller model from CrossPoint's production
//! X3 driver (MIT): clone github.com/crosspoint-reader/crosspoint-reader
//! with --recurse-submodules and read
//! freeink-sdk/libs/display/FreeInkDisplay/src/driver/Uc8253X3Driver.{h,cpp}
//! and lut/Uc8253X3Luts.h. Concept mapping and the trap list (two-phase
//! BUSY, CDI differential vs absolute, LUT banks per RefreshMode) are in
//! docs/plans/2026-07-06-x3-support-plan.md, Phase 2.

use super::fill_transformed_band_impl;
use crate::{fb::Framebuffer, BAND_BYTES};

/// Placeholder orientation: copied from the SSD1677/X4 values, NOT
/// verified for the X3 panel. Derive the real values from CrossPoint's
/// addressing setup and expect to fix them on first boot (mirrored or
/// bit-scrambled text is the symptom).
pub const MIRROR_X: bool = true;
pub const MIRROR_Y: bool = false;
pub const REVERSE_BITS: bool = true;

pub fn fill_transformed_band(fb: &Framebuffer, band_y: usize, out: &mut [u8; BAND_BYTES]) -> usize {
    fill_transformed_band_impl::<MIRROR_X, MIRROR_Y, REVERSE_BITS>(fb, band_y, out)
}
