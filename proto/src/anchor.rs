//! The coordinate a reading position is stored as: a spine item, and how far
//! into that item's logical content stream the position sits.
//!
//! A page number means something only to the layout that produced it, so it
//! cannot be durable state. This survives every layout change.
//!
//! # The logical content stream
//!
//! A coordinate space, not a buffer: nothing materializes it and no file holds
//! it. The XHTML block parser defines it for one spine item.
//!
//! - blocks count in the order the parser emits them, document order;
//! - a block occupies `text.len() + 1` bytes, the extra byte standing for the
//!   boundary after it;
//! - a block's offset is the sum of the sizes of every block before it in the
//!   same item, so an item's first block sits at 0.
//!
//! The extra byte lets content with no text hold a place. An image emits no
//! characters, and without it two images in a row would share one offset.
//!
//! The parser takes XHTML and CSS and nothing else. No font, viewport, margin
//! or line spacing reaches it, so the same bytes always produce the same
//! stream.
//!
//! # Versioning
//!
//! These rules are persistence ABI, covered by [`CONTENT_STREAM_VERSION`].
//! Change what the parser emits, how it normalizes text, or how a block's size
//! is counted, and every stored anchor moves.

/// The rules that define the logical content stream an offset indexes.
///
/// Bump this when the parser's emitted blocks, its text normalization, or the
/// size a block occupies changes. A reader that finds a version it does not
/// know cannot interpret the offset, and falls back under the position
/// format's own rules rather than guessing.
pub const CONTENT_STREAM_VERSION: u8 = 1;

/// A place in a book, independent of how the book is laid out.
///
/// Ordered the way a reader moves through one: by spine item, then by offset
/// within it. The derived `Ord` gives exactly that, which turns "the last page
/// starting at or before this anchor" into a comparison rather than a search
/// through content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentAnchor {
    /// Index of the spine item, in the order the OPF lists it.
    pub spine: u16,
    /// Bytes into that item's logical content stream.
    pub offset: u32,
}

/// Bytes one anchor occupies on the card.
pub const CONTENT_ANCHOR_BYTES: usize = 6;

impl ContentAnchor {
    /// The beginning of the book: the first spine item, before its first
    /// block. Where a book with no stored position opens.
    pub const START: Self = Self {
        spine: 0,
        offset: 0,
    };

    /// The anchor for a place, named rather than built positionally, since
    /// `(spine, offset)` and `(offset, spine)` are both plausible readings of
    /// a bare two-number constructor.
    pub const fn at(spine: u16, offset: u32) -> Self {
        Self { spine, offset }
    }

    /// Whether this anchor falls in `[start, end)`.
    ///
    /// Half-open, and the convention the whole feature keeps: a page owns its
    /// start and not its end, so one layout's page boundaries tile the book
    /// with no place belonging to two pages and none belonging to none.
    pub fn is_within(self, start: Self, end: Self) -> bool {
        self >= start && self < end
    }

    /// Little-endian, spine then offset. Fixed width, so an array of anchors
    /// indexes without a scan.
    pub fn encode(self, out: &mut [u8; CONTENT_ANCHOR_BYTES]) {
        out[0..2].copy_from_slice(&self.spine.to_le_bytes());
        out[2..6].copy_from_slice(&self.offset.to_le_bytes());
    }

    /// The inverse of [`encode`](Self::encode). Total: every six-byte pattern
    /// is a legal anchor, so a torn read yields a wrong place rather than a
    /// parse failure, and the caller's own integrity check catches it.
    pub fn decode(input: &[u8; CONTENT_ANCHOR_BYTES]) -> Self {
        Self {
            spine: u16::from_le_bytes([input[0], input[1]]),
            offset: u32::from_le_bytes([input[2], input[3], input[4], input[5]]),
        }
    }
}

/// How much of the logical stream a block holding this much text occupies.
///
/// The one place the rule lives, so the parser that assigns offsets and any
/// reader that walks them cannot disagree about it.
pub const fn block_stream_len(text_len: usize) -> u32 {
    // Saturating rather than wrapping: a block longer than u32 cannot exist,
    // since the parser's own block buffer is 384 bytes, and a silent wrap
    // would put a later place before an earlier one.
    (text_len as u32).saturating_add(1)
}

/// Quantize a page index into a 16-bit fixed-point progression (`0..=u16::MAX`).
///
/// Uses ceiling division so that integer-floor decoding restores to the exact
/// same page index for unchanged totals up to `u16::MAX`.
pub const fn encode_progression(screen: u32, total: u32) -> u16 {
    let total = if total == 0 { 1 } else { total as u64 };
    let scaled = (screen as u64 * u16::MAX as u64).div_ceil(total);
    if scaled > u16::MAX as u64 {
        u16::MAX
    } else {
        scaled as u16
    }
}

/// Decode a 16-bit progression back to a 0-indexed page number within `total` pages.
pub const fn decode_progression(progression: u16, total: u32) -> u32 {
    let total_u64 = total as u64;
    let page = (progression as u64 * total_u64) / u16::MAX as u64;
    let page = page as u32;
    let max_page = total.saturating_sub(1);
    if page > max_page {
        max_page
    } else {
        page
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_order_by_spine_then_offset() {
        let first = ContentAnchor::at(0, 100);
        let later_in_item = ContentAnchor::at(0, 200);
        let next_item = ContentAnchor::at(1, 0);
        assert!(first < later_in_item);
        assert!(later_in_item < next_item);
        assert!(ContentAnchor::START < first);
    }

    #[test]
    fn a_round_trip_keeps_the_place() {
        for anchor in [
            ContentAnchor::START,
            ContentAnchor::at(1, 0),
            ContentAnchor::at(0, u32::MAX),
            ContentAnchor::at(u16::MAX, u32::MAX),
            ContentAnchor::at(7, 4_099),
        ] {
            let mut bytes = [0u8; CONTENT_ANCHOR_BYTES];
            anchor.encode(&mut bytes);
            assert_eq!(ContentAnchor::decode(&bytes), anchor);
        }
    }

    #[test]
    fn a_page_owns_its_start_and_not_its_end() {
        let start = ContentAnchor::at(2, 40);
        let end = ContentAnchor::at(2, 90);
        assert!(start.is_within(start, end), "the start is inside");
        assert!(ContentAnchor::at(2, 89).is_within(start, end));
        assert!(
            !end.is_within(start, end),
            "the end belongs to the next page"
        );
        assert!(!ContentAnchor::at(2, 39).is_within(start, end));
        assert!(
            !ContentAnchor::at(3, 0).is_within(start, end),
            "and a later spine item is outside whatever its offset"
        );
    }

    #[test]
    fn content_with_no_text_still_takes_a_place_of_its_own() {
        // Two images in a row: no characters between them, and the reader
        // still has to be able to come back to the second rather than the
        // first.
        let first = 0;
        let second = first + block_stream_len(0);
        assert_ne!(first, second);
        assert_eq!(block_stream_len(0), 1);
        assert_eq!(block_stream_len(383), 384);
    }

    #[test]
    fn progression_round_trips_without_early_page_truncation() {
        // Test various totals and small counts.
        for total in [1, 2, 3, 4, 5, 10, 50, 100, 384, 1000, u16::MAX as u32] {
            for screen in 0..total.min(200) {
                let encoded = encode_progression(screen, total);
                let decoded = decode_progression(encoded, total);
                assert_eq!(
                    decoded, screen,
                    "round-trip failed for screen={screen}, total={total}, encoded={encoded}"
                );
            }
            // Also test end boundaries
            for screen in [total.saturating_sub(2), total.saturating_sub(1)] {
                let encoded = encode_progression(screen, total);
                let decoded = decode_progression(encoded, total);
                assert_eq!(
                    decoded, screen,
                    "boundary round-trip failed for screen={screen}, total={total}, encoded={encoded}"
                );
            }
        }

        // Test screen >= total clamps safely to last page
        assert_eq!(decode_progression(encode_progression(100, 100), 100), 99);
        assert_eq!(decode_progression(encode_progression(150, 100), 100), 99);

        // Test total == 0 does not divide by zero
        assert_eq!(encode_progression(0, 0), 0);
        assert_eq!(decode_progression(0, 0), 0);
    }
}
