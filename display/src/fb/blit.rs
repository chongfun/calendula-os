//! Drawing an image's 1 bpp rows into the frame, a band of rows at a time.
//!
//! Rows come as an image pack stores them (`proto::image::pack`): `stride`
//! bytes a row, top to bottom in reading orientation, the leftmost pixel in
//! the most significant bit, 1 for white. The framebuffer keeps a native
//! panel row the other way about: its last byte holds the leftmost pixels,
//! least significant bit first (`Framebuffer::set_pixel`), so a native row
//! read as one long bit string from its first byte's top bit is the panel
//! row mirrored. Each frame turns rows its own way:
//!
//! - `Native` and `Landscape` keep a row a row (bottom up in `Landscape` on
//!   the X4), mirrored into place: the bytes go in from the other end, each
//!   bit-reversed.
//! - `LandscapeFlipped` mirrors each row itself, which undoes the
//!   mirroring: a plain shifted copy.
//! - `Portrait` turns rows into native columns. Eight frame rows fill one
//!   native byte column, so each 8 x 8 block of bits is transposed in two
//!   words and lands as one byte in each of eight native rows.
//!
//! Which native row a frame row or column lands on differs between the X4
//! and the X3, as `Framebuffer::map` says; the per-frame helpers below
//! follow it. Whatever the frame, the framebuffer ends up exactly as a
//! `set_pixel` per pixel would leave it, clipping included; the tests hold
//! it to that on both panels.

use super::{FbFrame, Framebuffer};
use crate::{HEIGHT, ROW_BYTES, WIDTH};

// The row copies take a native row as one bit string of `WIDTH` bits.
const _: () = assert!(ROW_BYTES * 8 == WIDTH);

/// A band of an image's rows.
#[derive(Clone, Copy, Debug)]
pub struct ImageRows<'a> {
    /// Pixels in a row. Bits past it, to the end of the row's last byte,
    /// are padding and never drawn.
    pub width: usize,
    /// Bytes per row.
    pub stride: usize,
    /// The image row `bits` starts at.
    pub first_row: usize,
    /// Whole rows, `stride` bytes each; a trailing part row is ignored.
    pub bits: &'a [u8],
}

/// The native row a `Landscape` frame row lands on.
const fn landscape_native_y(frame_y: usize) -> usize {
    #[cfg(not(feature = "device-x3"))]
    {
        HEIGHT - 1 - frame_y
    }
    #[cfg(feature = "device-x3")]
    {
        frame_y
    }
}

/// The native row a `LandscapeFlipped` frame row lands on.
const fn flipped_native_y(frame_y: usize) -> usize {
    #[cfg(not(feature = "device-x3"))]
    {
        frame_y
    }
    #[cfg(feature = "device-x3")]
    {
        HEIGHT - 1 - frame_y
    }
}

/// The native row a `Portrait` frame column lands on.
const fn portrait_native_y(frame_x: usize) -> usize {
    #[cfg(not(feature = "device-x3"))]
    {
        HEIGHT - 1 - frame_x
    }
    #[cfg(feature = "device-x3")]
    {
        frame_x
    }
}

impl Framebuffer {
    /// Draws a band of an image's rows, with the image's top-left corner at
    /// frame coordinates (`x`, `y`): the band's first row lands on frame row
    /// `y + first_row`. Every pixel of the band inside the frame is written,
    /// black or white; the rest is clipped.
    pub fn blit(&mut self, x: i32, y: i32, rows: ImageRows<'_>) {
        if rows.stride == 0 {
            return;
        }
        let width = rows.width.min(rows.stride.saturating_mul(8));
        let count = rows.bits.len() / rows.stride;
        let top = (y as isize).saturating_add(rows.first_row.min(isize::MAX as usize) as isize);
        let x = x as isize;
        // The image columns and band rows that land in the frame.
        let (frame_width, frame_height) =
            (self.frame_width() as isize, self.frame_height() as isize);
        let first_column = (-x).max(0) as usize;
        let end_column = (frame_width - x).clamp(0, width as isize) as usize;
        let first_line = (-top).max(0) as usize;
        let end_line = (frame_height.saturating_sub(top)).clamp(0, count as isize) as usize;
        if first_column >= end_column || first_line >= end_line {
            return;
        }
        let columns = first_column..end_column;
        // Frame column of the first shown column.
        let left = (x + first_column as isize) as usize;
        let line = |index: usize| &rows.bits[index * rows.stride..][..rows.stride];
        match self.frame {
            FbFrame::Native | FbFrame::Landscape => {
                // Frame column c is native column c, which sits at bit
                // WIDTH - 1 - c of the row's bit string: the shown columns
                // land mirrored, ending at bit WIDTH - 1 - left.
                let dst_bit = WIDTH - (left + columns.len());
                for index in first_line..end_line {
                    let frame_y = (top + index as isize) as usize;
                    let native_y = if self.frame == FbFrame::Landscape {
                        landscape_native_y(frame_y)
                    } else {
                        frame_y
                    };
                    let row = &mut self.data[native_y * ROW_BYTES..][..ROW_BYTES];
                    copy_bits_reversed(row, dst_bit, line(index), columns.clone());
                }
            }
            FbFrame::LandscapeFlipped => {
                // Frame column c is native column WIDTH - 1 - c, bit c of
                // the row's bit string: a plain copy from bit `left`.
                for index in first_line..end_line {
                    let frame_y = (top + index as isize) as usize;
                    let native_y = flipped_native_y(frame_y);
                    let row = &mut self.data[native_y * ROW_BYTES..][..ROW_BYTES];
                    copy_bits(row, left, line(index), columns.clone());
                }
            }
            FbFrame::Portrait => {
                let start = (top + first_line as isize) as usize;
                let end = (top + end_line as isize) as usize;
                // Frame rows 8g..8g + 8 are one native byte column.
                let mut group = start / 8 * 8;
                while group < end {
                    let mut lines: [&[u8]; 8] = [&[]; 8];
                    let mut present = 0u8;
                    for (bit, slot) in lines.iter_mut().enumerate() {
                        let frame_y = group + bit;
                        if (start..end).contains(&frame_y) {
                            *slot = line((frame_y as isize - top) as usize);
                            present |= 1 << bit;
                        }
                    }
                    self.portrait_group(group, &lines, present, left, columns.clone());
                    group += 8;
                }
            }
        }
    }

    /// Draws the image columns `columns` of up to eight rows, those at
    /// frame rows `group + bit` for each bit set in `present`, into the
    /// native byte column those rows map to. The first of `columns` sits at
    /// frame column `left`.
    fn portrait_group(
        &mut self,
        group: usize,
        lines: &[&[u8]; 8],
        present: u8,
        left: usize,
        columns: core::ops::Range<usize>,
    ) {
        // Frame row group + bit is native column WIDTH - 1 - group - bit,
        // which `set_pixel` files in byte group / 8 of its row, at bit
        // 7 - bit from the least significant end: bit `bit` counting from
        // the most significant, which is where the transpose below puts row
        // `bit` of the block.
        let native_byte = group / 8;
        // Bits of `present` the same way round as the block's rows.
        let mask = present.reverse_bits();
        for byte in columns.start / 8..columns.end.div_ceil(8) {
            let at = |bit: usize| u32::from(lines[bit].get(byte).copied().unwrap_or(0));
            let high = at(0) << 24 | at(1) << 16 | at(2) << 8 | at(3);
            let low = at(4) << 24 | at(5) << 16 | at(6) << 8 | at(7);
            let columns_out = transpose8(high, low);
            let first = (byte * 8).max(columns.start);
            let end = (byte * 8 + 8).min(columns.end);
            for column in first..end {
                let frame_x = left + (column - columns.start);
                let native_y = portrait_native_y(frame_x);
                let out = &mut self.data[native_y * ROW_BYTES + native_byte];
                let bits = columns_out[column - byte * 8];
                *out = if present == 0xFF {
                    bits
                } else {
                    (*out & !mask) | (bits & mask)
                };
            }
        }
    }
}

/// Copies source bits `columns` of `src` into `dst`, starting at bit
/// `dst_bit`. Bits count from the most significant bit of byte 0.
fn copy_bits(dst: &mut [u8], dst_bit: usize, src: &[u8], columns: core::ops::Range<usize>) {
    let end = dst_bit + columns.len();
    let (first, last) = (dst_bit / 8, (end - 1) / 8);
    // Destination byte `j` takes the eight source bits from `8 * j + delta`.
    let delta = columns.start as isize - dst_bit as isize;
    let take = |byte: usize| bits_at(src, 8 * byte as isize + delta);
    let head = 0xFFu8 >> (dst_bit % 8);
    let tail = 0xFFu8 << (7 - (end - 1) % 8);
    if first == last {
        merge(&mut dst[first], take(first), head & tail);
        return;
    }
    merge(&mut dst[first], take(first), head);
    merge(&mut dst[last], take(last), tail);
    // The bytes between are whole, and so are their source bits: no edge
    // left to check, a byte or two of `src` each.
    let start = (8 * (first + 1) as isize + delta) as usize;
    let (from, shift) = (start / 8, (start % 8) as u32);
    let middle = &mut dst[first + 1..last];
    let Some(sources) = src.get(from..from + middle.len() + usize::from(shift > 0)) else {
        return;
    };
    if shift == 0 {
        middle.copy_from_slice(sources);
        return;
    }
    for (out, pair) in middle.iter_mut().zip(sources.windows(2)) {
        *out = pair[0] << shift | pair[1] >> (8 - shift);
    }
}

/// Copies source bits `columns` of `src` into `dst` mirrored: destination
/// bit `dst_bit + i` takes source bit `columns.end - 1 - i`.
fn copy_bits_reversed(
    dst: &mut [u8],
    dst_bit: usize,
    src: &[u8],
    columns: core::ops::Range<usize>,
) {
    let end = dst_bit + columns.len();
    let (first, last) = (dst_bit / 8, (end - 1) / 8);
    // Destination byte `j` takes the eight source bits from
    // `edge - 8 * j` on, in reverse: the first of them is its last bit.
    let edge = columns.end as isize - 8 + dst_bit as isize;
    let take = |byte: usize| bits_at(src, edge - 8 * byte as isize).reverse_bits();
    let head = 0xFFu8 >> (dst_bit % 8);
    let tail = 0xFFu8 << (7 - (end - 1) % 8);
    if first == last {
        merge(&mut dst[first], take(first), head & tail);
        return;
    }
    merge(&mut dst[first], take(first), head);
    merge(&mut dst[last], take(last), tail);
    // As in `copy_bits`, from the last whole byte's source bits, which come
    // first in `src`, on.
    let start = (edge - 8 * (last - 1) as isize) as usize;
    let (from, shift) = (start / 8, (start % 8) as u32);
    let middle = &mut dst[first + 1..last];
    let Some(sources) = src.get(from..from + middle.len() + usize::from(shift > 0)) else {
        return;
    };
    if shift == 0 {
        for (out, &byte) in middle.iter_mut().rev().zip(sources) {
            *out = byte.reverse_bits();
        }
        return;
    }
    for (out, pair) in middle.iter_mut().rev().zip(sources.windows(2)) {
        *out = (pair[0] << shift | pair[1] >> (8 - shift)).reverse_bits();
    }
}

/// Eight bits of `src` from bit `start` on, the first in the top bit; bits
/// before or past `src` read as zero.
fn bits_at(src: &[u8], start: isize) -> u8 {
    let byte = |index: isize| {
        usize::try_from(index)
            .ok()
            .and_then(|index| src.get(index))
            .copied()
            .unwrap_or(0)
    };
    let index = start.div_euclid(8);
    let pair = u16::from(byte(index)) << 8 | u16::from(byte(index + 1));
    (pair << start.rem_euclid(8) >> 8) as u8
}

/// Sets the bits of `out` that `mask` selects to those of `bits`.
fn merge(out: &mut u8, bits: u8, mask: u8) {
    *out = (*out & !mask) | (bits & mask);
}

/// Transposes an 8 x 8 bit block (Hacker's Delight, `transpose8rS32`).
/// The block's rows are the bytes of `high` then `low`, most significant
/// first, each row's first column in its top bit. Returns its columns,
/// first column first, each with row 0 in its top bit.
#[inline(always)]
fn transpose8(mut high: u32, mut low: u32) -> [u8; 8] {
    let mut t = (high ^ (high >> 7)) & 0x00AA_00AA;
    high ^= t ^ (t << 7);
    t = (low ^ (low >> 7)) & 0x00AA_00AA;
    low ^= t ^ (t << 7);
    t = (high ^ (high >> 14)) & 0x0000_CCCC;
    high ^= t ^ (t << 14);
    t = (low ^ (low >> 14)) & 0x0000_CCCC;
    low ^= t ^ (t << 14);
    t = (high & 0xF0F0_F0F0) | ((low >> 4) & 0x0F0F_0F0F);
    low = ((high << 4) & 0xF0F0_F0F0) | (low & 0x0F0F_0F0F);
    high = t;
    let (h, l) = (high.to_be_bytes(), low.to_be_bytes());
    [h[0], h[1], h[2], h[3], l[0], l[1], l[2], l[3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transpose8_matches_a_bit_by_bit_transpose() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let rows = seed.to_be_bytes();
            let high = u32::from_be_bytes([rows[0], rows[1], rows[2], rows[3]]);
            let low = u32::from_be_bytes([rows[4], rows[5], rows[6], rows[7]]);
            let columns = transpose8(high, low);
            for (column, out) in columns.iter().enumerate() {
                for (row, byte) in rows.iter().enumerate() {
                    let bit = byte >> (7 - column) & 1;
                    assert_eq!(out >> (7 - row) & 1, bit, "row {row} column {column}");
                }
            }
        }
    }

    fn bits_of(bytes: &[u8], range: core::ops::Range<usize>) -> std::vec::Vec<bool> {
        range
            .map(|bit| bytes[bit / 8] & (0x80 >> (bit % 8)) != 0)
            .collect()
    }

    #[test]
    fn row_copies_move_exactly_the_bits_asked_for() {
        let src = [0b1011_0011u8, 0x5A, 0xC3, 0x0F, 0x96];
        for start in 0..src.len() * 8 {
            for end in start + 1..=src.len() * 8 {
                for dst_bit in 0..20 {
                    let mut dst = [0x6Cu8; 8];
                    let before = dst;
                    copy_bits(&mut dst, dst_bit, &src, start..end);
                    let len = end - start;
                    assert_eq!(
                        bits_of(&dst, dst_bit..dst_bit + len),
                        bits_of(&src, start..end)
                    );
                    assert_eq!(bits_of(&dst, 0..dst_bit), bits_of(&before, 0..dst_bit));
                    assert_eq!(
                        bits_of(&dst, dst_bit + len..64),
                        bits_of(&before, dst_bit + len..64)
                    );

                    let mut mirrored = [0x6Cu8; 8];
                    copy_bits_reversed(&mut mirrored, dst_bit, &src, start..end);
                    let mut expected = bits_of(&src, start..end);
                    expected.reverse();
                    assert_eq!(bits_of(&mirrored, dst_bit..dst_bit + len), expected);
                    assert_eq!(bits_of(&mirrored, 0..dst_bit), bits_of(&before, 0..dst_bit));
                    assert_eq!(
                        bits_of(&mirrored, dst_bit + len..64),
                        bits_of(&before, dst_bit + len..64)
                    );
                }
            }
        }
    }
}
