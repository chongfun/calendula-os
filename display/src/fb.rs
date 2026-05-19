use crate::{FB_BYTES, FB_WIDTH, FB_HEIGHT};

pub struct Framebuffer {
    pub data: [u8; FB_BYTES],
}

impl Framebuffer {
    /// Creates a new black framebuffer.
    pub const fn new() -> Self {
        Self {
            data: [0; FB_BYTES],
        }
    }

    /// Clears the framebuffer to either black or white.
    pub fn clear(&mut self, white: bool) {
        let val = if white { 0xFF } else { 0x00 };
        self.data.fill(val);
    }

    /// Sets the state of a single pixel.
    /// `white = true` maps to high voltage (white particle state/active).
    #[inline]
    pub fn set_pixel(&mut self, x: usize, y: usize, white: bool) {
        if x >= FB_WIDTH || y >= FB_HEIGHT {
            return;
        }
        let idx = y * (FB_WIDTH / 8) + (x / 8);
        let bit = 7 - (x % 8);
        if white {
            self.data[idx] |= 1 << bit;
        } else {
            self.data[idx] &= !(1 << bit);
        }
    }

    /// Gets the state of a single pixel.
    #[inline]
    pub fn get_pixel(&self, x: usize, y: usize) -> bool {
        if x >= FB_WIDTH || y >= FB_HEIGHT {
            return false;
        }
        let idx = y * (FB_WIDTH / 8) + (x / 8);
        let bit = 7 - (x % 8);
        (self.data[idx] & (1 << bit)) != 0
    }

    /// Returns a horizontal slice (band) ready for SPI DMA.
    /// `y_start..y_start+height` must be within `0..FB_HEIGHT`.
    pub fn band(&self, y_start: usize, height: usize) -> &[u8] {
        let row_bytes = FB_WIDTH / 8; // 100 bytes
        let start = y_start * row_bytes;
        let end = start + height * row_bytes;
        &self.data[start..end]
    }
}
