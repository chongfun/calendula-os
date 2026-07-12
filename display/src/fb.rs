use crate::{FB_BYTES, HEIGHT, ROW_BYTES, WIDTH};

/// The upright drawing frame for one render: which way the device is held.
/// `set_pixel`/`pixel` take coordinates in this frame — x rightward, y
/// downward, exactly as the reader sees the screen — and map them onto the
/// panel's native row-major buffer. The mapping folds in the panel's
/// inverted row scan, which the renderers used to apply as whole-buffer
/// post-passes (`flip_vertical`, `rotate_180`) after drawing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FbFrame {
    /// Raw buffer coordinates, no transform. Boot fallbacks and byte-level
    /// tooling; not a frame the reader ever holds upright.
    #[default]
    Native,
    /// The default landscape hold.
    Landscape,
    /// The device rotated 180 degrees.
    LandscapeFlipped,
    /// The device rotated a quarter turn counter-clockwise: the long axis
    /// runs vertically, the front-button column sits below the screen.
    Portrait,
}

impl FbFrame {
    /// Drawing-frame width: the panel's long side lies horizontal in
    /// landscape, vertical in portrait.
    pub const fn width(self) -> usize {
        match self {
            FbFrame::Portrait => HEIGHT,
            _ => WIDTH,
        }
    }

    pub const fn height(self) -> usize {
        match self {
            FbFrame::Portrait => WIDTH,
            _ => HEIGHT,
        }
    }
}

// repr(C) pins the layout to FB_BYTES + 1 (both fields align 1): the
// firmware links one Framebuffer into an exactly-sized linker slot
// (fw/build.rs prev_fb_bytes), which must track this size.
#[repr(C)]
pub struct Framebuffer {
    data: [u8; FB_BYTES],
    frame: FbFrame,
}

impl Framebuffer {
    pub const fn new() -> Self {
        Self {
            data: [0xFF; FB_BYTES],
            frame: FbFrame::Native,
        }
    }

    #[inline]
    pub fn bytes(&self) -> &[u8; FB_BYTES] {
        &self.data
    }

    /// Set the drawing frame for the render about to happen. Framebuffers
    /// are long-lived statics, so every render sets its frame rather than
    /// trusting whatever the previous frame drew with.
    pub fn set_frame(&mut self, frame: FbFrame) {
        self.frame = frame;
    }

    pub const fn frame(&self) -> FbFrame {
        self.frame
    }

    pub const fn frame_width(&self) -> usize {
        self.frame.width()
    }

    pub const fn frame_height(&self) -> usize {
        self.frame.height()
    }

    pub fn clear(&mut self, white: bool) {
        self.data.fill(if white { 0xFF } else { 0x00 });
    }

    pub fn copy_from(&mut self, other: &Self) {
        self.data.copy_from_slice(other.bytes());
    }

    pub fn band(&self, y: usize, rows: usize) -> &[u8] {
        let start = y * ROW_BYTES;
        let end = start + rows.min(HEIGHT - y) * ROW_BYTES;
        &self.data[start..end]
    }

    /// Frame coordinates → native buffer coordinates. `None` when outside
    /// the frame. The landscape arms reproduce, pixel for pixel, what the
    /// old draw-then-flip pipeline wrote; the portrait arm is the quarter
    /// turn composed with the same scan inversion.
    #[inline]
    fn map(&self, x: usize, y: usize) -> Option<(usize, usize)> {
        match self.frame {
            FbFrame::Native => (x < WIDTH && y < HEIGHT).then_some((x, y)),
            FbFrame::Landscape => (x < WIDTH && y < HEIGHT).then(|| (x, HEIGHT - 1 - y)),
            FbFrame::LandscapeFlipped => (x < WIDTH && y < HEIGHT).then(|| (WIDTH - 1 - x, y)),
            FbFrame::Portrait => (x < HEIGHT && y < WIDTH).then(|| (WIDTH - 1 - y, HEIGHT - 1 - x)),
        }
    }

    #[inline]
    pub fn set_pixel(&mut self, x: usize, y: usize, white: bool) {
        let Some((native_x, native_y)) = self.map(x, y) else {
            return;
        };

        let index = native_y * ROW_BYTES + native_x / 8;
        let mask = 0x80 >> (native_x & 7);
        if white {
            self.data[index] |= mask;
        } else {
            self.data[index] &= !mask;
        }
    }

    /// Fill `len` pixels rightward from frame (x, y): the byte-run
    /// equivalent of `len` `set_pixel` calls. The landscape frames map a
    /// frame row onto one native row, so the run is written as whole bytes
    /// with masked edge bytes; Portrait transposes rows into columns and
    /// keeps the per-pixel path (portrait rendering is active work — its
    /// behavior stays the pixel-for-pixel reference one).
    pub fn fill_span(&mut self, x: usize, y: usize, len: usize, white: bool) {
        if self.frame == FbFrame::Portrait {
            for x in x..x.saturating_add(len) {
                self.set_pixel(x, y, white);
            }
            return;
        }
        if y >= HEIGHT || x >= WIDTH || len == 0 {
            return;
        }
        let native_y = match self.frame {
            FbFrame::Landscape => HEIGHT - 1 - y,
            _ => y,
        };
        let len = len.min(WIDTH - x);
        // One frame span is one native span: identical in Native and
        // Landscape, x-mirrored in LandscapeFlipped — a solid fill is
        // direction-blind, so only the endpoints move.
        let x0 = match self.frame {
            FbFrame::LandscapeFlipped => WIDTH - x - len,
            _ => x,
        };
        self.fill_native_span(native_y, x0, x0 + len, white);
    }

    /// Fill native bits [x0, x1) on native row `y`; `0 <= x0 < x1 <= WIDTH`.
    fn fill_native_span(&mut self, y: usize, x0: usize, x1: usize, white: bool) {
        let base = y * ROW_BYTES;
        let first = base + x0 / 8;
        let last = base + (x1 - 1) / 8;
        let head = 0xFFu8 >> (x0 & 7);
        let tail = 0xFFu8 << (7 - ((x1 - 1) & 7));
        if first == last {
            Self::apply_mask(&mut self.data[first], head & tail, white);
            return;
        }
        Self::apply_mask(&mut self.data[first], head, white);
        self.data[first + 1..last].fill(if white { 0xFF } else { 0x00 });
        Self::apply_mask(&mut self.data[last], tail, white);
    }

    /// Blit one packed MSB-first pixel row — a glyph row — at frame
    /// (x, y): every 1 bit sets (white) or clears the pixel under it, 0
    /// bits leave the framebuffer untouched. `width` pixels are consumed
    /// from `bits` (anything past `width` in the last byte is ignored),
    /// and `x` may be negative for left-clipped draws. The landscape
    /// frames blit whole source bytes into the row's byte pair (mirroring
    /// via bit reversal when flipped); Portrait keeps the per-pixel
    /// reference path.
    pub fn blit_row(&mut self, x: i32, y: i32, bits: &[u8], width: usize, white: bool) {
        if y < 0 {
            return;
        }
        let n = width.div_ceil(8).min(bits.len());
        if self.frame == FbFrame::Portrait {
            for i in 0..(n * 8).min(width) {
                if bits[i / 8] & (0x80 >> (i & 7)) != 0 {
                    let draw_x = x + i as i32;
                    if draw_x >= 0 {
                        self.set_pixel(draw_x as usize, y as usize, white);
                    }
                }
            }
            return;
        }
        if y as usize >= HEIGHT {
            return;
        }
        let native_y = match self.frame {
            FbFrame::Landscape => HEIGHT - 1 - y as usize,
            _ => y as usize,
        };
        let base = native_y * ROW_BYTES;
        for (k, &byte) in bits[..n].iter().enumerate() {
            // Zero the padding bits past `width`: they are not part of the
            // row and may hold anything.
            let valid = (width - 8 * k).min(8);
            let byte = byte & ((0xFF00u16 >> valid) as u8);
            if byte == 0 {
                continue;
            }
            let (byte, bit_x) = match self.frame {
                // Mirroring x reverses the byte's bits and re-anchors it
                // from the right edge.
                FbFrame::LandscapeFlipped => {
                    (byte.reverse_bits(), WIDTH as i32 - x - 8 * (k as i32 + 1))
                }
                _ => (byte, x + 8 * k as i32),
            };
            self.blit_native_bits(base, bit_x, byte, white);
        }
    }

    /// Merge one source byte whose MSB lands at native bit position
    /// `bit_x` (possibly negative or past the right edge; off-row bits
    /// drop) into the native row starting at byte index `base`.
    #[inline]
    fn blit_native_bits(&mut self, base: usize, bit_x: i32, bits: u8, white: bool) {
        if bit_x <= -8 || bit_x >= WIDTH as i32 {
            return;
        }
        let index = bit_x.div_euclid(8);
        let shift = bit_x.rem_euclid(8) as u32;
        if index >= 0 {
            Self::apply_mask(&mut self.data[base + index as usize], bits >> shift, white);
        }
        // `index + 1 >= 0` always holds here (`bit_x > -8` puts `index`
        // at -1 or later), so only the right edge needs a bound.
        if shift > 0 && index + 1 < ROW_BYTES as i32 {
            Self::apply_mask(
                &mut self.data[base + (index + 1) as usize],
                bits << (8 - shift),
                white,
            );
        }
    }

    #[inline]
    fn apply_mask(byte: &mut u8, mask: u8, white: bool) {
        if white {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }

    /// Read a pixel in raw buffer coordinates, ignoring the drawing frame.
    /// For tooling that serializes the buffer (PNG dumps, canvas blits,
    /// panel models) and must not re-apply the frame transform on the way
    /// out.
    #[inline]
    pub fn native_pixel(&self, x: usize, y: usize) -> bool {
        if x >= WIDTH || y >= HEIGHT {
            return true;
        }
        let index = y * ROW_BYTES + x / 8;
        let mask = 0x80 >> (x & 7);
        self.data[index] & mask != 0
    }

    #[inline]
    pub fn pixel(&self, x: usize, y: usize) -> bool {
        let Some((native_x, native_y)) = self.map(x, y) else {
            return true;
        };

        let index = native_y * ROW_BYTES + native_x / 8;
        let mask = 0x80 >> (native_x & 7);
        self.data[index] & mask != 0
    }
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Landscape frame must write the same bytes the old pipeline
    /// produced by drawing in native coordinates and then flipping
    /// vertically (swapping row y with HEIGHT - 1 - y).
    #[test]
    fn landscape_frame_matches_draw_then_flip() {
        let mut framed = Framebuffer::new();
        framed.set_frame(FbFrame::Landscape);
        let mut flipped = Framebuffer::new();

        for (i, x) in [0usize, 13, 400, 401, WIDTH - 1].iter().enumerate() {
            let y = i * 123 % HEIGHT;
            framed.set_pixel(*x, y, false);
            flipped.set_pixel(*x, HEIGHT - 1 - y, false);
        }

        assert_eq!(framed.bytes()[..], flipped.bytes()[..]);
    }

    /// LandscapeFlipped must match the old draw + flip_vertical +
    /// rotate_180 composition, which reduces to mirroring x.
    #[test]
    fn landscape_flipped_frame_matches_draw_flip_rotate() {
        let mut framed = Framebuffer::new();
        framed.set_frame(FbFrame::LandscapeFlipped);
        let mut mirrored = Framebuffer::new();

        for (i, x) in [0usize, 13, 400, 401, WIDTH - 1].iter().enumerate() {
            let y = i * 123 % HEIGHT;
            framed.set_pixel(*x, y, false);
            mirrored.set_pixel(WIDTH - 1 - x, y, false);
        }

        assert_eq!(framed.bytes()[..], mirrored.bytes()[..]);
    }

    #[test]
    fn portrait_frame_swaps_dimensions_and_maps_corners() {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Portrait);
        assert_eq!(fb.frame_width(), HEIGHT);
        assert_eq!(fb.frame_height(), WIDTH);

        // Frame top-left, top-right, bottom-left, bottom-right.
        fb.set_pixel(0, 0, false);
        fb.set_pixel(HEIGHT - 1, 0, false);
        fb.set_pixel(0, WIDTH - 1, false);
        fb.set_pixel(HEIGHT - 1, WIDTH - 1, false);

        let mut expected = Framebuffer::new();
        expected.set_pixel(WIDTH - 1, HEIGHT - 1, false);
        expected.set_pixel(WIDTH - 1, 0, false);
        expected.set_pixel(0, HEIGHT - 1, false);
        expected.set_pixel(0, 0, false);

        assert_eq!(fb.bytes()[..], expected.bytes()[..]);
    }

    #[test]
    fn portrait_frame_round_trips_reads() {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Portrait);
        for (i, x) in [3usize, 99, 200, HEIGHT - 2].iter().enumerate() {
            fb.set_pixel(*x, i * 331 % WIDTH, false);
        }
        for (i, x) in [3usize, 99, 200, HEIGHT - 2].iter().enumerate() {
            assert!(!fb.pixel(*x, i * 331 % WIDTH));
        }
    }

    const ALL_FRAMES: [FbFrame; 4] = [
        FbFrame::Native,
        FbFrame::Landscape,
        FbFrame::LandscapeFlipped,
        FbFrame::Portrait,
    ];

    /// The naive per-pixel loop `fill_span` must be byte-identical to.
    fn fill_span_reference(fb: &mut Framebuffer, x: usize, y: usize, len: usize, white: bool) {
        for x in x..x.saturating_add(len) {
            fb.set_pixel(x, y, white);
        }
    }

    #[test]
    fn fill_span_matches_per_pixel_reference_in_every_frame() {
        let spans = [
            (0usize, 0usize, 1usize),
            (0, 0, WIDTH),
            (3, 7, 2),
            (5, 11, 3),
            (7, 2, 9),
            (8, 3, 16),
            (1, HEIGHT - 1, 14),
            (WIDTH - 9, 4, 9),
            (WIDTH - 2, 1, 50), // crosses the right edge
            (WIDTH, 5, 4),      // fully off-frame
            (2, HEIGHT + 3, 4),
            (6, 9, 0),
        ];
        for frame in ALL_FRAMES {
            for &(x, y, len) in &spans {
                for white in [false, true] {
                    let mut fast = Framebuffer::new();
                    let mut reference = Framebuffer::new();
                    fast.clear(!white);
                    reference.clear(!white);
                    fast.set_frame(frame);
                    reference.set_frame(frame);
                    fast.fill_span(x, y, len, white);
                    fill_span_reference(&mut reference, x, y, len, white);
                    assert_eq!(
                        fast.bytes()[..],
                        reference.bytes()[..],
                        "frame {frame:?} span ({x}, {y})+{len} white={white}"
                    );
                }
            }
        }
    }

    /// The naive per-pixel loop `blit_row` must be byte-identical to this.
    fn blit_row_reference(
        fb: &mut Framebuffer,
        x: i32,
        y: i32,
        bits: &[u8],
        width: usize,
        white: bool,
    ) {
        if y < 0 {
            return;
        }
        for i in 0..width.min(bits.len() * 8) {
            if bits[i / 8] & (0x80 >> (i & 7)) != 0 {
                let draw_x = x + i as i32;
                if draw_x >= 0 {
                    fb.set_pixel(draw_x as usize, y as usize, white);
                }
            }
        }
    }

    #[test]
    fn blit_row_matches_per_pixel_reference_in_every_frame() {
        // Widths that don't fill the last byte leave garbage padding bits
        // set on purpose: both paths must ignore them.
        let rows: [(&[u8], usize); 6] = [
            (&[0b1011_0101], 8),
            (&[0b1011_0111], 5),
            (&[0xFF, 0xA5, 0x3C], 24),
            (&[0xFF, 0xA5, 0xFF], 17),
            (&[0x01, 0x80], 16),
            (&[0x00, 0x00], 16),
        ];
        let positions = [
            (-9i32, 3i32),
            (-3, 0),
            (0, 7),
            (1, HEIGHT as i32 - 1),
            (5, -1),
            (8, 12),
            (761, 40),
            (WIDTH as i32 - 3, 2),
            (WIDTH as i32 + 2, 2),
            (4, HEIGHT as i32),
        ];
        for frame in ALL_FRAMES {
            for &(bits, width) in &rows {
                for &(x, y) in &positions {
                    for white in [false, true] {
                        let mut fast = Framebuffer::new();
                        let mut reference = Framebuffer::new();
                        fast.clear(!white);
                        reference.clear(!white);
                        fast.set_frame(frame);
                        reference.set_frame(frame);
                        fast.blit_row(x, y, bits, width, white);
                        blit_row_reference(&mut reference, x, y, bits, width, white);
                        assert_eq!(
                            fast.bytes()[..],
                            reference.bytes()[..],
                            "frame {frame:?} row at ({x}, {y}) width {width} bits {bits:?} white={white}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn blit_row_sets_white_where_bits_are_one() {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Landscape);
        fb.clear(false);
        fb.blit_row(3, 5, &[0b1100_0001], 8, true);
        for x in 0..16 {
            assert_eq!(fb.pixel(x, 5), matches!(x, 3 | 4 | 10), "x={x}");
        }
    }

    #[test]
    fn out_of_frame_writes_are_dropped() {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Portrait);
        let before = *fb.bytes();
        // Legal in landscape, outside the portrait frame's width.
        fb.set_pixel(HEIGHT, 10, false);
        fb.set_pixel(10, WIDTH, false);
        assert_eq!(fb.bytes()[..], before[..]);
        assert!(fb.pixel(HEIGHT, 10));
    }
}
