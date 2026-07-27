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
        if x >= self.frame_width() || y >= self.frame_height() {
            return None;
        }
        let (nx, ny) = match self.frame {
            FbFrame::Native => (x, y),
            FbFrame::Landscape => {
                #[cfg(not(feature = "device-x3"))]
                {
                    (x, HEIGHT - 1 - y)
                }
                #[cfg(feature = "device-x3")]
                {
                    (x, y)
                }
            }
            FbFrame::LandscapeFlipped => {
                #[cfg(not(feature = "device-x3"))]
                {
                    (WIDTH - 1 - x, y)
                }
                #[cfg(feature = "device-x3")]
                {
                    (WIDTH - 1 - x, HEIGHT - 1 - y)
                }
            }
            FbFrame::Portrait => {
                #[cfg(not(feature = "device-x3"))]
                {
                    (WIDTH - 1 - y, HEIGHT - 1 - x)
                }
                #[cfg(feature = "device-x3")]
                {
                    (WIDTH - 1 - y, x)
                }
            }
        };

        Some((nx, ny))
    }

    #[inline(always)]
    fn byte_x(native_x: usize) -> usize {
        ROW_BYTES - 1 - native_x / 8
    }

    #[inline]
    pub fn set_pixel(&mut self, x: usize, y: usize, white: bool) {
        let Some((native_x, native_y)) = self.map(x, y) else {
            return;
        };

        let index = native_y * ROW_BYTES + Self::byte_x(native_x);
        let mask = 0x01 << (native_x & 7);
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
    /// uses a dedicated strided column fast path whose output is tested
    /// against the per-pixel reference.
    pub fn fill_span(&mut self, x: usize, y: usize, len: usize, white: bool) {
        if self.frame == FbFrame::Portrait {
            let len = len.min(HEIGHT.saturating_sub(x));
            if len == 0 || y >= WIDTH {
                return;
            }
            let native_x = WIDTH - 1 - y;
            let byte_x = Self::byte_x(native_x);
            let mask = 0x01 << (native_x & 7);

            #[cfg(not(feature = "device-x3"))]
            let start_y = HEIGHT - 1 - x;
            #[cfg(feature = "device-x3")]
            let start_y = x;

            let mut index = start_y * ROW_BYTES + byte_x;

            #[cfg(not(feature = "device-x3"))]
            let stride = ROW_BYTES.wrapping_neg();
            #[cfg(feature = "device-x3")]
            let stride = ROW_BYTES;

            if white {
                for _ in 0..len {
                    self.data[index] |= mask;
                    index = index.wrapping_add(stride);
                }
            } else {
                let not_mask = !mask;
                for _ in 0..len {
                    self.data[index] &= not_mask;
                    index = index.wrapping_add(stride);
                }
            }
            return;
        }
        if y >= HEIGHT || x >= WIDTH || len == 0 {
            return;
        }
        let native_y = match self.frame {
            FbFrame::Landscape => {
                #[cfg(not(feature = "device-x3"))]
                {
                    HEIGHT - 1 - y
                }
                #[cfg(feature = "device-x3")]
                {
                    y
                }
            }
            FbFrame::LandscapeFlipped => {
                #[cfg(not(feature = "device-x3"))]
                {
                    y
                }
                #[cfg(feature = "device-x3")]
                {
                    HEIGHT - 1 - y
                }
            }
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
        let mem_right = base + (ROW_BYTES - 1 - x0 / 8);
        let mem_left = base + (ROW_BYTES - 1 - (x1 - 1) / 8);

        let right_mask = 0xFFu8 << (x0 & 7);
        let left_mask = 0xFFu8 >> (7 - ((x1 - 1) & 7));

        if mem_left == mem_right {
            Self::apply_mask(&mut self.data[mem_left], left_mask & right_mask, white);
            return;
        }
        Self::apply_mask(&mut self.data[mem_right], right_mask, white);
        self.data[mem_left + 1..mem_right].fill(if white { 0xFF } else { 0x00 });
        Self::apply_mask(&mut self.data[mem_left], left_mask, white);
    }

    /// Blit one packed MSB-first pixel row — a glyph row — at frame
    /// (x, y): every 1 bit sets (white) or clears the pixel under it, 0
    /// bits leave the framebuffer untouched. `width` pixels are consumed
    /// from `bits` (anything past `width` in the last byte is ignored),
    /// and `x` may be negative for left-clipped draws. The landscape
    /// frames blit whole source bytes into the row's byte pair (mirroring
    /// via bit reversal when flipped); Portrait uses a dedicated strided
    /// column fast path whose output is tested against the per-pixel
    /// reference.
    pub fn blit_row(&mut self, x: i32, y: i32, bits: &[u8], width: usize, white: bool) {
        if y < 0 {
            return;
        }
        let n = width.div_ceil(8).min(bits.len());
        if self.frame == FbFrame::Portrait {
            if y as usize >= WIDTH {
                return;
            }
            let native_x = WIDTH - 1 - y as usize;
            let byte_x = Self::byte_x(native_x);
            let mask = 0x01 << (native_x & 7);

            let width = (n * 8).min(width);
            let start_i = if x < 0 { (-x) as usize } else { 0 };
            let end_i = width.min(if x < HEIGHT as i32 {
                (HEIGHT as i32 - x) as usize
            } else {
                0
            });
            if start_i >= end_i {
                return;
            }

            let start_draw_x = (x + start_i as i32) as usize;
            #[cfg(not(feature = "device-x3"))]
            let native_y = HEIGHT - 1 - start_draw_x;
            #[cfg(feature = "device-x3")]
            let native_y = start_draw_x;

            let mut index = native_y * ROW_BYTES + byte_x;

            #[cfg(not(feature = "device-x3"))]
            let stride = ROW_BYTES.wrapping_neg();
            #[cfg(feature = "device-x3")]
            let stride = ROW_BYTES;

            for i in start_i..end_i {
                if bits[i / 8] & (0x80 >> (i & 7)) != 0 {
                    if white {
                        self.data[index] |= mask;
                    } else {
                        self.data[index] &= !mask;
                    }
                }
                index = index.wrapping_add(stride);
            }
            return;
        }
        if y as usize >= HEIGHT {
            return;
        }
        let native_y = match self.frame {
            FbFrame::Landscape => {
                #[cfg(not(feature = "device-x3"))]
                {
                    HEIGHT - 1 - y as usize
                }
                #[cfg(feature = "device-x3")]
                {
                    y as usize
                }
            }
            FbFrame::LandscapeFlipped => {
                #[cfg(not(feature = "device-x3"))]
                {
                    y as usize
                }
                #[cfg(feature = "device-x3")]
                {
                    HEIGHT - 1 - y as usize
                }
            }
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
                FbFrame::LandscapeFlipped => (byte, WIDTH as i32 - x - 8 * (k as i32 + 1)),
                _ => (byte.reverse_bits(), x + 8 * k as i32),
            };
            self.blit_native_bits(base, bit_x, byte, white);
        }
    }

    /// Blit a packed MSB-first bitmap — a glyph — at frame (x, y):
    /// `height` rows of `width` pixels, each row starting at a byte
    /// boundary. Identical in output to one [`Self::blit_row`] per row,
    /// which is exactly what the landscape frames do.
    ///
    /// Portrait takes a transposed path instead, and that is the point of
    /// this entry: a frame row runs down a native *column*, so the
    /// row-at-a-time loop can only place one pixel per read-modify-write.
    /// Eight consecutive frame rows, though, land in eight bits of the
    /// same native byte — transposing an 8x8 block of the glyph turns
    /// those 64 masked writes into 8, and amortizes the per-row column
    /// setup across the whole glyph. Reading pages are almost entirely
    /// glyph blits, so this is the portrait page's rasterizer cost.
    pub fn blit_bitmap(
        &mut self,
        x: i32,
        y: i32,
        bits: &[u8],
        width: usize,
        height: usize,
        white: bool,
    ) {
        let row_bytes = width.div_ceil(8);
        if row_bytes == 0 || height == 0 {
            return;
        }
        if self.frame != FbFrame::Portrait {
            for row in 0..height {
                let start = (row * row_bytes).min(bits.len());
                let end = ((row + 1) * row_bytes).min(bits.len());
                self.blit_row(x, y + row as i32, &bits[start..end], width, white);
            }
            return;
        }

        // Portrait frame: x runs across the frame's width (the panel's
        // HEIGHT), y down its height (the panel's WIDTH).
        let col_start = if x < 0 { (-x) as usize } else { 0 };
        let col_end = width.min((HEIGHT as i32 - x).max(0) as usize);
        let row_start = if y < 0 { (-y) as usize } else { 0 };
        let row_end = height.min((WIDTH as i32 - y).max(0) as usize);
        if col_start >= col_end || row_start >= row_end {
            return;
        }

        // Every frame column maps to one native row, walked at the column
        // stride: descending memory on the X4's inverted scan, ascending
        // on the X3's.
        #[cfg(not(feature = "device-x3"))]
        let stride = ROW_BYTES.wrapping_neg();
        #[cfg(feature = "device-x3")]
        let stride = ROW_BYTES;

        let mut row = row_start;
        while row < row_end {
            let native_x = WIDTH - 1 - (y + row as i32) as usize;
            let byte_x = Self::byte_x(native_x);
            // Rows walk native x downward, so this group runs from bit
            // `top_bit` down to bit 0 of one native byte.
            let top_bit = native_x & 7;
            let rows = (top_bit + 1).min(row_end - row);
            // The transpose lands row j at bit 7-j; this group wants it at
            // bit top_bit-j.
            let shift = 7 - top_bit;

            for byte_col in col_start / 8..col_end.div_ceil(8) {
                let mut source = [0u8; 8];
                let mut any = 0u8;
                for (j, slot) in source.iter_mut().enumerate().take(rows) {
                    *slot = bits
                        .get((row + j) * row_bytes + byte_col)
                        .copied()
                        .unwrap_or(0);
                    any |= *slot;
                }
                // Glyph bitmaps are mostly background; an empty 8x8 block
                // is worth neither the transpose nor eight masked writes.
                if any == 0 {
                    continue;
                }
                let column_bytes = transpose8(source);

                let first = (byte_col * 8).max(col_start);
                let last = (byte_col * 8 + 8).min(col_end);
                #[cfg(not(feature = "device-x3"))]
                let native_y = HEIGHT - 1 - (x + first as i32) as usize;
                #[cfg(feature = "device-x3")]
                let native_y = (x + first as i32) as usize;
                let mut index = native_y * ROW_BYTES + byte_x;
                for column in first..last {
                    let mask = column_bytes[column - byte_col * 8] >> shift;
                    if mask != 0 {
                        Self::apply_mask(&mut self.data[index], mask, white);
                    }
                    index = index.wrapping_add(stride);
                }
            }
            row += rows;
        }
    }

    /// Merge one source byte whose MSB lands at native bit position
    /// `bit_x` (possibly negative or past the right edge; off-row bits
    /// drop) into the native row starting at byte index `base`.
    #[inline]
    fn blit_native_bits(&mut self, base: usize, bit_x: i32, rev_bits: u8, white: bool) {
        if bit_x <= -8 || bit_x >= WIDTH as i32 {
            return;
        }

        let index = ROW_BYTES as i32 - 1 - bit_x.div_euclid(8);
        let shift = bit_x.rem_euclid(8) as u32;

        let aligned = (rev_bits as u16) << shift;

        if index >= 0 && index < ROW_BYTES as i32 {
            Self::apply_mask(&mut self.data[base + index as usize], aligned as u8, white);
        }

        let next_index = index - 1;

        if shift > 0 && next_index >= 0 && next_index < ROW_BYTES as i32 {
            Self::apply_mask(
                &mut self.data[base + next_index as usize],
                (aligned >> 8) as u8,
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
        let index = y * ROW_BYTES + Self::byte_x(x);
        let mask = 0x01 << (x & 7);
        self.data[index] & mask != 0
    }

    #[inline]
    pub fn pixel(&self, x: usize, y: usize) -> bool {
        let Some((native_x, native_y)) = self.map(x, y) else {
            return true;
        };

        let index = native_y * ROW_BYTES + Self::byte_x(native_x);
        let mask = 0x01 << (native_x & 7);
        self.data[index] & mask != 0
    }
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Transpose an 8x8 bit block held MSB-first: `source[j]` is row `j` with
/// bit 7 its leftmost pixel, and the result's byte `k` is column `k` with
/// bit 7 its topmost pixel. Six shift-mask-or steps (Hacker's Delight
/// 7-3) instead of the 64 bit tests the naive loop costs; the equivalence
/// is pinned by `transpose8_matches_per_bit_reference`.
#[inline]
fn transpose8(source: [u8; 8]) -> [u8; 8] {
    let mut x = u64::from_be_bytes(source);
    x = (x & 0xAA55_AA55_AA55_AA55)
        | ((x & 0x00AA_00AA_00AA_00AA) << 7)
        | ((x >> 7) & 0x00AA_00AA_00AA_00AA);
    x = (x & 0xCCCC_3333_CCCC_3333)
        | ((x & 0x0000_CCCC_0000_CCCC) << 14)
        | ((x >> 14) & 0x0000_CCCC_0000_CCCC);
    x = (x & 0xF0F0_F0F0_0F0F_0F0F)
        | ((x & 0x0000_0000_F0F0_F0F0) << 28)
        | ((x >> 28) & 0x0000_0000_F0F0_F0F0);
    x.to_be_bytes()
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
        flipped.set_frame(FbFrame::Native);

        for (i, x) in [0usize, 13, 400, 401, WIDTH - 1].iter().enumerate() {
            let y = i * 123 % HEIGHT;
            framed.set_pixel(*x, y, false);
            #[cfg(not(feature = "device-x3"))]
            let expected_y = HEIGHT - 1 - y;
            #[cfg(feature = "device-x3")]
            let expected_y = y;
            flipped.set_pixel(*x, expected_y, false);
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
        mirrored.set_frame(FbFrame::Native);

        for (i, x) in [0usize, 13, 400, 401, WIDTH - 1].iter().enumerate() {
            let y = i * 123 % HEIGHT;
            framed.set_pixel(*x, y, false);
            #[cfg(not(feature = "device-x3"))]
            let expected_y = y;
            #[cfg(feature = "device-x3")]
            let expected_y = HEIGHT - 1 - y;
            mirrored.set_pixel(WIDTH - 1 - x, expected_y, false);
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

    /// The transposed portrait path is only allowed to be faster, never
    /// different: whatever `blit_bitmap` writes, the row-at-a-time
    /// `blit_row` loop it replaced must write too. `blit_row` is itself
    /// pinned to the per-pixel reference above, so this chains to it.
    #[test]
    fn blit_bitmap_matches_row_at_a_time_in_every_frame() {
        // Heights that straddle the 8-row group boundary and widths that
        // leave padding bits set in the last byte of every row.
        let glyphs: [(&[u8], usize, usize); 5] = [
            (&[0b1011_0101], 8, 1),
            (&[0b1011_0111, 0b0100_1011, 0b1111_1110], 5, 3),
            (&[0xFF, 0xA5, 0x3C, 0x00, 0x81, 0x7E, 0x18, 0xDB], 8, 8),
            (
                &[
                    0xFF, 0xA5, 0x12, 0x3C, 0x00, 0x40, 0x81, 0x7E, 0x08, 0x18, 0xDB, 0xC0, 0x5A,
                    0x5A, 0x80, 0x01, 0x02, 0x03, 0xF0, 0x0F, 0xFF,
                ],
                17,
                7,
            ),
            (&[0x01, 0x80, 0x00, 0x00, 0xFF, 0xFF], 16, 3),
        ];
        let positions = [
            (-9i32, 3i32),
            (-3, -2),
            (0, 0),
            (1, HEIGHT as i32 - 1),
            (5, -1),
            (7, 9),
            (8, 16),
            (12, 761),
            (WIDTH as i32 - 3, 2),
            (2, WIDTH as i32 - 3),
            (HEIGHT as i32 - 2, 4),
            (4, HEIGHT as i32),
            (WIDTH as i32 + 2, 2),
        ];
        for frame in ALL_FRAMES {
            for &(bits, width, height) in &glyphs {
                let row_bytes = width.div_ceil(8);
                for &(x, y) in &positions {
                    for white in [false, true] {
                        let mut fast = Framebuffer::new();
                        let mut reference = Framebuffer::new();
                        fast.clear(!white);
                        reference.clear(!white);
                        fast.set_frame(frame);
                        reference.set_frame(frame);
                        fast.blit_bitmap(x, y, bits, width, height, white);
                        for row in 0..height {
                            let start = (row * row_bytes).min(bits.len());
                            let end = ((row + 1) * row_bytes).min(bits.len());
                            reference.blit_row(x, y + row as i32, &bits[start..end], width, white);
                        }
                        assert_eq!(
                            fast.bytes()[..],
                            reference.bytes()[..],
                            "frame {frame:?} bitmap {width}x{height} at ({x}, {y}) white={white}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn transpose8_matches_per_bit_reference() {
        let blocks: [[u8; 8]; 5] = [
            [0; 8],
            [0xFF; 8],
            [0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01],
            [0xA5, 0x5A, 0x3C, 0xC3, 0x0F, 0xF0, 0x99, 0x66],
            [0x01, 0x00, 0xFE, 0x13, 0x7F, 0x80, 0x24, 0xBD],
        ];
        for block in blocks {
            let mut expected = [0u8; 8];
            for (row, byte) in block.iter().enumerate() {
                for (column, out) in expected.iter_mut().enumerate() {
                    if byte & (0x80 >> column) != 0 {
                        *out |= 0x80 >> row;
                    }
                }
            }
            assert_eq!(transpose8(block), expected, "block {block:?}");
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

    #[test]
    fn native_frame_round_trips_raw_pixels() {
        let mut fb = Framebuffer::new(); // Native frame
        fb.clear(true);

        // Native (0, 0)
        fb.set_pixel(0, 0, false);
        assert!(!fb.native_pixel(0, 0));
        assert!(!fb.pixel(0, 0));

        // Native bottom-right corner
        fb.set_pixel(WIDTH - 1, HEIGHT - 1, false);
        assert!(!fb.native_pixel(WIDTH - 1, HEIGHT - 1));
        assert!(!fb.pixel(WIDTH - 1, HEIGHT - 1));

        // Native arbitrary point
        fb.set_pixel(123, 456 % HEIGHT, false);
        assert!(!fb.native_pixel(123, 456 % HEIGHT));
        assert!(!fb.pixel(123, 456 % HEIGHT));
    }

    #[cfg(feature = "device-x3")]
    #[test]
    fn x3_raw_byte_layout_matches_hardware_spec() {
        let mut fb = Framebuffer::new(); // Native frame
        fb.clear(true);

        // Hardware specification for X3:
        // ROW_BYTES = 99, HEIGHT = 528
        // MIRROR_X = true -> native x=0 maps to byte index ROW_BYTES - 1 (index 98 in row)
        // REVERSE_BITS = true -> bit mask for x & 7 is LSB-oriented (0x01 << (x & 7))

        // Native top-left corner (x=0, y=0) -> byte 98, mask 0x01
        fb.set_pixel(0, 0, false);
        assert_eq!(fb.bytes()[98] & 0x01, 0);

        // Native top-right corner (x=791, y=0) -> byte 0 (since 791/8 = 98, 98-98 = 0), mask 0x80 (791 & 7 = 7)
        fb.set_pixel(791, 0, false);
        assert_eq!(fb.bytes()[0] & 0x80, 0);

        // Native bottom-left corner (x=0, y=527) -> byte 527 * 99 + 98 = 52271, mask 0x01
        fb.set_pixel(0, 527, false);
        assert_eq!(fb.bytes()[52271] & 0x01, 0);
    }
}
