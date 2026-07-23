//! RAM cache of custom-font glyph metrics, and the run measurement built on
//! it, with the font pack behind a sans-IO seam.
//!
//! The firmware owns the card; this module owns the decision of *when* the
//! card is worth touching. That decision is the whole optimization — see
//! [`for_each_metric`] — so it lives here, where a host test can count the
//! opens, rather than on the far side of `embedded_sdmmc`.

use display::font::{FontStyle, GlyphMetric, StyledChars};
use proto::font_pack::{font_pack_codepoint_index, FontPackFaceRecord, FONT_PACK_METRIC_BYTES};

/// One metric record as it is stored in the pack, before decoding.
pub type MetricRecord = [u8; FONT_PACK_METRIC_BYTES];

/// Printable ASCII is the first codepoint range of the pack
/// (`font_pack_codepoint_index(0x20) == 0`), so a face's ASCII metrics are
/// one contiguous run at the start of its metric table.
const ASCII_METRIC_COUNT: usize = 95;
const ASCII_FIRST: u16 = 0x20;
const ASCII_LAST: u16 = 0x7E;
/// Regular/Italic/Bold/BoldItalic of the active size all stay resident.
const METRIC_CACHE_SLOTS: usize = 4;
/// Enough to retain a multilingual paragraph's working set of non-ASCII
/// metrics without consuming the X3's tightly budgeted static RAM
/// (16 slots x ~20 B on top of the ASCII slots).
const NON_ASCII_METRIC_SLOTS: usize = 16;

/// Record reads from an already-open font pack. The only I/O the metric
/// cache performs.
pub trait PackReader {
    /// Read `records.len()` consecutive metric records starting at
    /// `offset`. Records are contiguous on the card, so an implementation
    /// seeks once for the whole run. `None` on any read failure.
    fn read_records(&mut self, offset: u32, records: &mut [MetricRecord]) -> Option<()>;
}

/// The font pack as a slow source that must be opened before it can be
/// read. On the device, opening means a directory walk plus a file open on
/// the SD card — the cost the metric cache exists to avoid — so it is its
/// own step, and an implementation must touch nothing until `with_reader`
/// is called.
pub trait PackSource {
    /// Open the pack and hand a reader to `read` exactly once, then close
    /// it. Returns `false` if the pack could not be opened, in which case
    /// `read` was never called.
    fn with_reader(&self, read: &mut dyn FnMut(&mut dyn PackReader)) -> bool;
}

/// RAM cache of the printable-ASCII metric rows for custom font faces.
///
/// Line measurement during a cold book build asks for a metric once per
/// character of the whole book; without this cache each ask was a directory
/// walk, a file open, a seek, and a 12-byte read. A slot fills once per
/// face from an open pack and then serves the overwhelming majority of
/// characters from RAM; non-ASCII falls through to a small ring of decoded
/// metrics. Slots are keyed by pack identity plus the face's metric-table
/// offset (unique per size and style within a pack), so a changed pack or
/// size misses and refills naturally.
pub struct MetricCache {
    slots: [MetricSlot; METRIC_CACHE_SLOTS],
    next_evict: usize,
    non_ascii: [NonAsciiMetricSlot; NON_ASCII_METRIC_SLOTS],
    next_non_ascii_evict: usize,
}

#[derive(Clone, Copy)]
struct MetricSlot {
    key: Option<(u64, u32)>,
    records: [MetricRecord; ASCII_METRIC_COUNT],
}

const EMPTY_METRIC_SLOT: MetricSlot = MetricSlot {
    key: None,
    records: [[0u8; FONT_PACK_METRIC_BYTES]; ASCII_METRIC_COUNT],
};

/// One decoded non-ASCII metric, keyed like the ASCII slots plus the
/// codepoint. Pages in scripts beyond ASCII hit the same few dozen
/// characters repeatedly, so even a small ring removes most per-char
/// pack reads.
#[derive(Clone, Copy)]
struct NonAsciiMetricSlot {
    key: Option<(u64, u32, u16)>,
    metric: GlyphMetric,
}

const EMPTY_GLYPH_METRIC: GlyphMetric = GlyphMetric {
    offset: 0,
    len: 0,
    width: 0,
    height: 0,
    x_offset: 0,
    y_offset: 0,
    advance_fp: 0,
};

const EMPTY_NON_ASCII_SLOT: NonAsciiMetricSlot = NonAsciiMetricSlot {
    key: None,
    metric: EMPTY_GLYPH_METRIC,
};

impl MetricCache {
    pub const fn new() -> Self {
        Self {
            slots: [EMPTY_METRIC_SLOT; METRIC_CACHE_SLOTS],
            next_evict: 0,
            non_ascii: [EMPTY_NON_ASCII_SLOT; NON_ASCII_METRIC_SLOTS],
            next_non_ascii_evict: 0,
        }
    }

    fn ascii_index(ch: char) -> Option<usize> {
        let code = ch as u32;
        (code >= ASCII_FIRST as u32 && code <= ASCII_LAST as u32)
            .then(|| (code - ASCII_FIRST as u32) as usize)
    }

    /// Serve a cached metric without touching the card. `None` means the
    /// caller must open the pack: uncached non-ASCII char or unfilled face.
    pub fn lookup(&self, identity: u64, face: FontPackFaceRecord, ch: char) -> Option<GlyphMetric> {
        if Self::ascii_index(ch).is_none() {
            let codepoint = u16::try_from(ch as u32).unwrap_or(b'?' as u16);
            return self
                .non_ascii
                .iter()
                .find(|slot| slot.key == Some((identity, face.metrics_offset, codepoint)))
                .map(|slot| slot.metric);
        }
        let index = Self::ascii_index(ch)?;
        let key = (identity, face.metrics_offset);
        self.slots
            .iter()
            .find(|slot| slot.key == Some(key))
            .map(|slot| decode_metric(&slot.records[index]))
    }

    /// Fill the face's slot from an open pack (one sequential sweep of its
    /// ASCII run), then serve the char. Non-ASCII and packs without the
    /// full ASCII range fall back to the per-char read.
    pub fn fill_and_lookup(
        &mut self,
        reader: &mut dyn PackReader,
        identity: u64,
        face: FontPackFaceRecord,
        ch: char,
    ) -> Option<GlyphMetric> {
        let Some(index) = Self::ascii_index(ch) else {
            let codepoint = u16::try_from(ch as u32).unwrap_or(b'?' as u16);
            let key = (identity, face.metrics_offset, codepoint);
            if let Some(slot) = self.non_ascii.iter().find(|slot| slot.key == Some(key)) {
                return Some(slot.metric);
            }
            let metric = metric_for_char(reader, face, ch)?;
            let slot_index = self
                .non_ascii
                .iter()
                .position(|slot| slot.key.is_none())
                .unwrap_or_else(|| {
                    let evict = self.next_non_ascii_evict;
                    self.next_non_ascii_evict =
                        (self.next_non_ascii_evict + 1) % NON_ASCII_METRIC_SLOTS;
                    evict
                });
            self.non_ascii[slot_index] = NonAsciiMetricSlot {
                key: Some(key),
                metric,
            };
            return Some(metric);
        };
        let key = (identity, face.metrics_offset);
        if let Some(slot) = self.slots.iter().find(|slot| slot.key == Some(key)) {
            return Some(decode_metric(&slot.records[index]));
        }
        if (face.metric_count as usize) < ASCII_METRIC_COUNT {
            return metric_for_char(reader, face, ch);
        }
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.key.is_none())
            .unwrap_or_else(|| {
                let evict = self.next_evict;
                self.next_evict = (self.next_evict + 1) % METRIC_CACHE_SLOTS;
                evict
            });
        let slot = &mut self.slots[slot_index];
        slot.key = None;
        reader.read_records(face.metrics_offset, &mut slot.records)?;
        slot.key = Some(key);
        Some(decode_metric(&slot.records[index]))
    }
}

impl Default for MetricCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Measure a whole styled text run, opening the font pack only if the run
/// actually needs it.
///
/// Every character is offered to `cache` first, so a run that is entirely
/// warm — the common case once a face's ASCII range is filled, and the case
/// pagination hits over and over as it feeds short XHTML chunks through
/// here — never opens anything. [`PackSource::with_reader`] is called
/// lazily at the first character the cache cannot answer, and the rest of
/// the run then reuses that one open pack instead of paying an
/// open/seek/close cycle per miss.
///
/// A pack that will not open degrades to the cache alone rather than
/// discarding metrics already in RAM: a transient SD failure must not
/// change line widths and repaginate the book.
pub fn for_each_metric(
    pack: &impl PackSource,
    cache: &mut MetricCache,
    identity: u64,
    face_for: impl Fn(FontStyle) -> Option<FontPackFaceRecord>,
    text: &str,
    default_style: FontStyle,
    mut visit: impl FnMut(FontStyle, Option<GlyphMetric>),
) {
    let mut chars = StyledChars::new(text, default_style);
    let mut missed = None;
    for (style, ch) in chars.by_ref() {
        let Some(face) = face_for(style) else {
            // The pack has no face for this style; opening it cannot help.
            visit(style, None);
            continue;
        };
        match cache.lookup(identity, face, ch) {
            Some(metric) => visit(style, Some(metric)),
            None => {
                missed = Some((style, ch));
                break;
            }
        }
    }
    let Some((style, ch)) = missed else {
        return;
    };
    let opened = pack.with_reader(&mut |reader| {
        finish_run(style, ch, &mut chars, &mut visit, |style, ch| {
            face_for(style).and_then(|face| cache.fill_and_lookup(reader, identity, face, ch))
        });
    });
    if opened {
        return;
    }
    finish_run(style, ch, &mut chars, &mut visit, |style, ch| {
        face_for(style).and_then(|face| cache.lookup(identity, face, ch))
    });
}

/// Walk a run whose first character has already been taken off the cursor,
/// resolving that character and every remaining one through `resolve`.
fn finish_run(
    style: FontStyle,
    first: char,
    chars: &mut StyledChars<'_>,
    visit: &mut impl FnMut(FontStyle, Option<GlyphMetric>),
    mut resolve: impl FnMut(FontStyle, char) -> Option<GlyphMetric>,
) {
    let mut next = Some((style, first));
    while let Some((style, ch)) = next {
        let metric = resolve(style, ch);
        visit(style, metric);
        next = chars.next();
    }
}

/// Read one character's metric from the pack, falling back to `?` for
/// characters the pack does not carry.
fn metric_for_char(
    reader: &mut dyn PackReader,
    face: FontPackFaceRecord,
    ch: char,
) -> Option<GlyphMetric> {
    let codepoint = if ch as u32 > u16::MAX as u32 {
        b'?' as u16
    } else {
        ch as u16
    };
    metric(reader, face, codepoint).or_else(|| metric(reader, face, b'?' as u16))
}

fn metric(
    reader: &mut dyn PackReader,
    face: FontPackFaceRecord,
    codepoint: u16,
) -> Option<GlyphMetric> {
    let index = font_pack_codepoint_index(codepoint)?;
    if index >= face.metric_count as usize {
        return None;
    }
    let offset = face
        .metrics_offset
        .checked_add((index * FONT_PACK_METRIC_BYTES) as u32)?;
    let mut bytes = [0u8; FONT_PACK_METRIC_BYTES];
    reader.read_records(offset, core::slice::from_mut(&mut bytes))?;
    Some(decode_metric(&bytes))
}

pub fn decode_metric(bytes: &MetricRecord) -> GlyphMetric {
    GlyphMetric {
        offset: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        len: u16::from_le_bytes([bytes[4], bytes[5]]),
        width: bytes[6],
        height: bytes[7],
        x_offset: bytes[8] as i8,
        y_offset: bytes[9] as i8,
        advance_fp: u16::from_le_bytes([bytes[10], bytes[11]]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use display::font::{style_marker_code, STYLE_MARKER};
    use proto::font_pack::FONT_PACK_CODEPOINT_COUNT;

    const IDENTITY: u64 = 0x0123_4567_89ab_cdef;
    const METRICS_OFFSET: u32 = 64;

    fn encode_metric(metric: &GlyphMetric) -> MetricRecord {
        let mut out = [0u8; FONT_PACK_METRIC_BYTES];
        out[..4].copy_from_slice(&metric.offset.to_le_bytes());
        out[4..6].copy_from_slice(&metric.len.to_le_bytes());
        out[6] = metric.width;
        out[7] = metric.height;
        out[8] = metric.x_offset as u8;
        out[9] = metric.y_offset as u8;
        out[10..].copy_from_slice(&metric.advance_fp.to_le_bytes());
        out
    }

    /// The metric a face carries for `ch`, derived from the codepoint index
    /// so every record in the table is distinguishable.
    fn expected_metric(face: FontPackFaceRecord, ch: char) -> GlyphMetric {
        let index = font_pack_codepoint_index(ch as u16).expect("char is in the pack") as u16;
        GlyphMetric {
            offset: u32::from(index),
            len: index,
            width: (index % 251) as u8,
            height: 16,
            x_offset: 1,
            y_offset: -2,
            advance_fp: index.wrapping_add(u16::from(face.style)) | 1,
        }
    }

    fn face(style: u8, metrics_offset: u32) -> FontPackFaceRecord {
        FontPackFaceRecord {
            size_px: 22,
            style,
            line_height: 28,
            baseline: 22,
            flags: 0,
            metrics_offset,
            metric_count: u32::from(FONT_PACK_CODEPOINT_COUNT),
            bitmap_offset: 0,
            bitmap_len: 0,
            kerning_offset: 0,
            kerning_count: 0,
        }
    }

    /// A font pack held in memory that counts what a card would charge for:
    /// every `with_reader` is a directory walk plus a file open on the SD.
    struct FakePack {
        bytes: Vec<u8>,
        opens: Cell<usize>,
        record_reads: Cell<usize>,
        openable: Cell<bool>,
    }

    impl FakePack {
        /// Build a pack carrying a full metric table for each of `faces`.
        fn with_faces(faces: &[FontPackFaceRecord]) -> Self {
            let table_bytes = FONT_PACK_CODEPOINT_COUNT as usize * FONT_PACK_METRIC_BYTES;
            let end = faces
                .iter()
                .map(|face| face.metrics_offset as usize + table_bytes)
                .max()
                .unwrap_or(0);
            let mut bytes = vec![0u8; end];
            for face in faces {
                for index in 0..FONT_PACK_CODEPOINT_COUNT {
                    let metric = GlyphMetric {
                        offset: u32::from(index),
                        len: index,
                        width: (index % 251) as u8,
                        height: 16,
                        x_offset: 1,
                        y_offset: -2,
                        advance_fp: index.wrapping_add(u16::from(face.style)) | 1,
                    };
                    let at = face.metrics_offset as usize + index as usize * FONT_PACK_METRIC_BYTES;
                    bytes[at..at + FONT_PACK_METRIC_BYTES].copy_from_slice(&encode_metric(&metric));
                }
            }
            Self {
                bytes,
                opens: Cell::new(0),
                record_reads: Cell::new(0),
                openable: Cell::new(true),
            }
        }
    }

    struct FakeReader<'a>(&'a FakePack);

    impl PackReader for FakeReader<'_> {
        fn read_records(&mut self, offset: u32, records: &mut [MetricRecord]) -> Option<()> {
            self.0
                .record_reads
                .set(self.0.record_reads.get() + records.len());
            let mut at = offset as usize;
            for record in records.iter_mut() {
                let end = at.checked_add(FONT_PACK_METRIC_BYTES)?;
                record.copy_from_slice(self.0.bytes.get(at..end)?);
                at = end;
            }
            Some(())
        }
    }

    impl PackSource for FakePack {
        fn with_reader(&self, read: &mut dyn FnMut(&mut dyn PackReader)) -> bool {
            if !self.openable.get() {
                return false;
            }
            self.opens.set(self.opens.get() + 1);
            read(&mut FakeReader(self));
            true
        }
    }

    /// Measure a run against one face, collecting what the visitor saw.
    fn measure(
        pack: &FakePack,
        cache: &mut MetricCache,
        only: FontPackFaceRecord,
        text: &str,
    ) -> Vec<Option<GlyphMetric>> {
        let mut seen = Vec::new();
        for_each_metric(
            pack,
            cache,
            IDENTITY,
            |_| Some(only),
            text,
            FontStyle::Regular,
            |_, metric| seen.push(metric),
        );
        seen
    }

    fn styled(parts: &[(&str, FontStyle)]) -> String {
        let mut out = String::new();
        for (text, style) in parts {
            out.push(STYLE_MARKER);
            out.push(style_marker_code(*style));
            out.push_str(text);
        }
        out
    }

    /// The regression this whole path exists for: once a face's ASCII range
    /// is warm, measuring further ASCII runs must not open the pack at all.
    /// Pagination sends many short XHTML chunks through `for_each_metric`,
    /// so an open per call would be more SD traffic than the per-char path
    /// this replaced.
    #[test]
    fn a_warm_ascii_face_is_measured_without_opening_the_pack() {
        let regular = face(0, METRICS_OFFSET);
        let pack = FakePack::with_faces(&[regular]);
        let mut cache = MetricCache::new();

        measure(&pack, &mut cache, regular, "Hello");
        assert_eq!(pack.opens.get(), 1, "the cold run fills the face");
        let reads_after_warmup = pack.record_reads.get();

        let seen = measure(&pack, &mut cache, regular, "the rest of the chapter, warm");
        assert_eq!(pack.opens.get(), 1, "a warm run must not open the pack");
        assert_eq!(
            pack.record_reads.get(),
            reads_after_warmup,
            "a warm run must not read from the pack"
        );
        assert_eq!(
            seen,
            "the rest of the chapter, warm"
                .chars()
                .map(|ch| Some(expected_metric(regular, ch)))
                .collect::<Vec<_>>()
        );
    }

    /// A cold run pays one open for the whole run, not one per miss.
    #[test]
    fn a_cold_run_opens_the_pack_exactly_once() {
        let regular = face(0, METRICS_OFFSET);
        let pack = FakePack::with_faces(&[regular]);
        let mut cache = MetricCache::new();

        let seen = measure(&pack, &mut cache, regular, "cold, and full of misses");

        assert_eq!(pack.opens.get(), 1);
        assert_eq!(seen.len(), "cold, and full of misses".chars().count());
        assert!(seen.iter().all(Option::is_some));
    }

    /// A run that switches faces mid-way still opens the pack once: the
    /// style marker's face fills through the handle the first miss opened,
    /// not through a second open.
    #[test]
    fn a_run_that_misses_two_faces_still_opens_once() {
        let table = FONT_PACK_CODEPOINT_COUNT as usize * FONT_PACK_METRIC_BYTES;
        let regular = face(0, METRICS_OFFSET);
        let italic = face(1, METRICS_OFFSET + table as u32);
        let pack = FakePack::with_faces(&[regular, italic]);
        let mut cache = MetricCache::new();
        let text = styled(&[
            ("plain ", FontStyle::Regular),
            ("leaning", FontStyle::Italic),
        ]);

        let mut seen = Vec::new();
        for_each_metric(
            &pack,
            &mut cache,
            IDENTITY,
            |style| match style {
                FontStyle::Italic => Some(italic),
                _ => Some(regular),
            },
            &text,
            FontStyle::Regular,
            |style, metric| seen.push((style, metric)),
        );

        assert_eq!(
            pack.opens.get(),
            1,
            "the italic face fills through the handle the regular miss opened"
        );
        assert_eq!(
            seen.first(),
            Some(&(FontStyle::Regular, Some(expected_metric(regular, 'p'))))
        );
        assert_eq!(
            seen.last(),
            Some(&(FontStyle::Italic, Some(expected_metric(italic, 'g'))))
        );

        let before = pack.opens.get();
        for_each_metric(
            &pack,
            &mut cache,
            IDENTITY,
            |style| match style {
                FontStyle::Italic => Some(italic),
                _ => Some(regular),
            },
            &text,
            FontStyle::Regular,
            |_, _| {},
        );
        assert_eq!(pack.opens.get(), before, "both faces are warm now");
    }

    /// Non-ASCII characters miss the ASCII slots, so they are served by the
    /// ring instead; the second sighting of one must not reopen the pack.
    #[test]
    fn a_repeated_non_ascii_char_is_served_from_the_ring() {
        let regular = face(0, METRICS_OFFSET);
        let pack = FakePack::with_faces(&[regular]);
        let mut cache = MetricCache::new();

        let first = measure(&pack, &mut cache, regular, "café");
        assert_eq!(pack.opens.get(), 1);

        let second = measure(&pack, &mut cache, regular, "café");
        assert_eq!(pack.opens.get(), 1, "the ring holds the accented char");
        assert_eq!(first, second);
        assert_eq!(
            second.last(),
            Some(&Some(expected_metric(regular, 'é'))),
            "the ring must serve the same metric the pack held"
        );
    }

    /// A pack that will not open must not wipe out metrics already in RAM:
    /// falling back to a default advance for cached characters would change
    /// line widths and repaginate the book over a transient SD failure.
    #[test]
    fn an_unopenable_pack_still_serves_cached_metrics() {
        let regular = face(0, METRICS_OFFSET);
        let pack = FakePack::with_faces(&[regular]);
        let mut cache = MetricCache::new();

        measure(&pack, &mut cache, regular, "warm");
        pack.openable.set(false);
        let opens = pack.opens.get();

        let seen = measure(&pack, &mut cache, regular, "ab\u{2014}c");

        assert_eq!(
            pack.opens.get(),
            opens,
            "the failed open must not count as one"
        );
        assert_eq!(
            seen,
            [
                Some(expected_metric(regular, 'a')),
                Some(expected_metric(regular, 'b')),
                // Never read, and unreachable now: the caller's fallback
                // advance applies to this one character only.
                None,
                Some(expected_metric(regular, 'c')),
            ]
        );
    }

    /// A style with no face in the pack is answered without an open: no
    /// amount of card access would produce a metric for it.
    #[test]
    fn a_style_without_a_face_never_opens_the_pack() {
        let regular = face(0, METRICS_OFFSET);
        let pack = FakePack::with_faces(&[regular]);
        let mut cache = MetricCache::new();
        let mut seen = Vec::new();

        for_each_metric(
            &pack,
            &mut cache,
            IDENTITY,
            |_| None,
            "no face for this",
            FontStyle::Regular,
            |_, metric| seen.push(metric),
        );

        assert_eq!(pack.opens.get(), 0);
        assert!(seen.iter().all(Option::is_none));
        assert_eq!(seen.len(), "no face for this".chars().count());
    }

    /// A different pack (or a different size, which moves the face's metric
    /// table) must not be answered out of the previous pack's slots.
    #[test]
    fn a_changed_pack_identity_refills() {
        let regular = face(0, METRICS_OFFSET);
        let pack = FakePack::with_faces(&[regular]);
        let mut cache = MetricCache::new();

        measure(&pack, &mut cache, regular, "warm");
        assert_eq!(pack.opens.get(), 1);

        let mut seen = Vec::new();
        for_each_metric(
            &pack,
            &mut cache,
            IDENTITY ^ 1,
            |_| Some(regular),
            "warm",
            FontStyle::Regular,
            |_, metric| seen.push(metric),
        );

        assert_eq!(pack.opens.get(), 2, "a new pack identity must refill");
        assert!(seen.iter().all(Option::is_some));
    }
}
