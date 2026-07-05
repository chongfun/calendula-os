use crate::board::{MIRROR_X, MIRROR_Y, REVERSE_BITS};
use crate::{fb::Framebuffer, Rect, BAND_BYTES, BAND_ROWS, HEIGHT, ROW_BYTES, WIDTH};

#[cfg(feature = "board-x4")]
mod x4;

#[cfg(feature = "board-x4")]
pub use x4::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    Full,
    Fast,
    /// One-flicker cleaning refresh: the display-mode-1 waveform run with
    /// the temperature register overridden upward, selecting the hotter
    /// (shorter) OTP waveform. Cleans ghosting in roughly half the full
    /// refresh time at a small contrast cost; the panel's rated "fast"
    /// mode (~1.5 s vs ~3.5 s full at room temperature).
    FastClean,
    PowerDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpiOp {
    Reset,
    WaitBusy,
    Command { cmd: u8, data: &'static [u8] },
}

pub const fn is_byte_aligned(rect: Rect) -> bool {
    rect.x & 7 == 0 && rect.w & 7 == 0 && rect.w > 0 && rect.h > 0 && rect.x < WIDTH as u16
}

/// Bit-reversal lookup. RV32IMC has no bit-manipulation extension, so
/// `u8::reverse_bits` lowers to a shift/mask sequence; one rodata load
/// per byte is cheaper across the 96 K transforms of a full flush.
static REVERSE_BITS_LUT: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = (i as u8).reverse_bits();
        i += 1;
    }
    lut
};

pub fn fill_transformed_band(fb: &Framebuffer, band_y: usize, out: &mut [u8; BAND_BYTES]) -> usize {
    let rows = BAND_ROWS.min(HEIGHT - band_y);
    let len = rows * ROW_BYTES;

    if !MIRROR_X && !MIRROR_Y && !REVERSE_BITS {
        out[..len].copy_from_slice(fb.band(band_y, rows));
        return len;
    }

    #[inline(always)]
    fn panel_byte(value: u8) -> u8 {
        if MIRROR_X || REVERSE_BITS {
            REVERSE_BITS_LUT[value as usize]
        } else {
            value
        }
    }

    for out_row in 0..rows {
        let panel_y = band_y + out_row;
        let src_y = if MIRROR_Y {
            HEIGHT - 1 - panel_y
        } else {
            panel_y
        };
        let src_row = fb.band(src_y, 1);
        let dst_row = &mut out[out_row * ROW_BYTES..(out_row + 1) * ROW_BYTES];
        if MIRROR_X {
            for (dst, src) in dst_row.iter_mut().zip(src_row.iter().rev()) {
                *dst = panel_byte(*src);
            }
        } else {
            for (dst, src) in dst_row.iter_mut().zip(src_row.iter()) {
                *dst = panel_byte(*src);
            }
        }
    }

    len
}
