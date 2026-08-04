//! Every shipped glyph table, fingerprinted, plus the format invariants
//! `BitmapFont::glyph` relies on.
//!
//! The specimen goldens in `tools/emulator/tests/reading_golden.rs` render two
//! faces — default-size Literata and Merriweather, both regular. The other
//! thirty-two tables reach no pinned pixel anywhere: a rasterizer change that
//! clipped U+015F in `LITERATA_19_ITALIC` alone, or in
//! `MERRIWEATHER_26_BOLD_ITALIC`, would pass every render test in the repo.
//! Fingerprints cover all thirty-four without thirty-four more PNGs.
//!
//! Re-blessing is deliberate, and that is the point. A font regeneration emits
//! a diff of tens of thousands of lines in which toolchain drift and the
//! intended change are indistinguishable; these rows say which faces moved and
//! whether they gained or lost ink. Inspect the specimen frames, then paste the
//! table the failure prints.

use display::font::BitmapFont;
use display::{
    literata_extra_generated as extra, literata_generated as regular,
    literata_semibold_generated as semibold, literata_sizes_generated as sizes,
    merriweather_generated as merriweather,
};

/// Name, glyph count, inked pixels, and a fingerprint over everything that
/// decides how the face renders and wraps.
type Row = (&'static str, usize, u64, u64);

/// The pinned tables. Bless with the block the failure message prints.
///
/// `rustfmt::skip` for the same reason the generated font modules carry it: a
/// row is past the width rustfmt allows a tuple, and one face per line is the
/// only shape in which the diff of a regeneration can be read.
#[rustfmt::skip]
const PINNED: &[Row] = &[
    ("LITERATA_SMALL_REGULAR", 218, 6594, 0x039a2b6a3bf1fcdc),
    ("LITERATA_SMALL_BOLD", 218, 10506, 0x73cd48a33f9f9c64),
    ("LITERATA_SMALL_ITALIC", 218, 6169, 0x809993ecc49d9bae),
    ("LITERATA_DISPLAY_REGULAR", 218, 53577, 0x12cb62baad350644),
    ("LITERATA_REGULAR", 1631, 96984, 0x00efd0e0c88b3256),
    ("LITERATA_ITALIC", 1631, 90904, 0x3ed15e1de28336d0),
    ("LITERATA_BOLD", 1631, 129200, 0xdc89806dd08d2fcd),
    ("LITERATA_BOLD_ITALIC", 1631, 123511, 0x99d23b32359cf93b),
    ("LITERATA_19_SEMIBOLD", 1631, 96068, 0x4f33ad932ae08eca),
    ("LITERATA_19_SEMIBOLD_ITALIC", 1631, 91965, 0xb53a8ca7fadfafb5),
    ("LITERATA_22_SEMIBOLD", 1631, 117120, 0x2aa6ae1a376360a7),
    ("LITERATA_22_SEMIBOLD_ITALIC", 1631, 112390, 0xd8c236a0c2a06724),
    ("LITERATA_26_SEMIBOLD", 1631, 193174, 0x9e3751921b294f9b),
    ("LITERATA_26_SEMIBOLD_ITALIC", 1631, 186295, 0xfa18f33dec8e9f91),
    ("LITERATA_19_REGULAR", 1631, 83446, 0x4107a5fa12a1ffcf),
    ("LITERATA_19_ITALIC", 1631, 79533, 0xc284e50751297711),
    ("LITERATA_19_BOLD", 1631, 108077, 0xd76ce5e3c2b2da78),
    ("LITERATA_19_BOLD_ITALIC", 1631, 100856, 0xa0d2e442b3653279),
    ("LITERATA_26_REGULAR", 1631, 168867, 0xdd24b9c6fccb1e04),
    ("LITERATA_26_ITALIC", 1631, 161343, 0x65a87225375b2c52),
    ("LITERATA_26_BOLD", 1631, 211228, 0xc6780fb169e619d2),
    ("LITERATA_26_BOLD_ITALIC", 1631, 201488, 0x89c861a84b0a2af7),
    ("MERRIWEATHER_19_REGULAR", 1631, 141939, 0xfa8ce8d4f4237022),
    ("MERRIWEATHER_19_ITALIC", 1631, 129405, 0x89515359dc388630),
    ("MERRIWEATHER_19_BOLD", 1631, 161431, 0xbf8a35053fc0cf9b),
    ("MERRIWEATHER_19_BOLD_ITALIC", 1631, 144832, 0x1f9c3d8ff9bbc0f8),
    ("MERRIWEATHER_22_REGULAR", 1631, 192132, 0x2b5574a45178b9f2),
    ("MERRIWEATHER_22_ITALIC", 1631, 178323, 0x25e62484f1017210),
    ("MERRIWEATHER_22_BOLD", 1631, 206771, 0xa61f2eededfe1b5f),
    ("MERRIWEATHER_22_BOLD_ITALIC", 1631, 200665, 0xe0cdb85f4f4a7e5a),
    ("MERRIWEATHER_26_REGULAR", 1631, 265532, 0x3e03abed92527f42),
    ("MERRIWEATHER_26_ITALIC", 1631, 252432, 0x96e947641fd88634),
    ("MERRIWEATHER_26_BOLD", 1631, 298111, 0x5761c8e7c1d1c566),
    ("MERRIWEATHER_26_BOLD_ITALIC", 1631, 282380, 0x377857c13e76a6ea),
];

fn tables() -> Vec<(&'static str, &'static BitmapFont)> {
    vec![
        ("LITERATA_SMALL_REGULAR", &extra::LITERATA_SMALL_REGULAR),
        ("LITERATA_SMALL_BOLD", &extra::LITERATA_SMALL_BOLD),
        ("LITERATA_SMALL_ITALIC", &extra::LITERATA_SMALL_ITALIC),
        ("LITERATA_DISPLAY_REGULAR", &extra::LITERATA_DISPLAY_REGULAR),
        ("LITERATA_REGULAR", &regular::LITERATA_REGULAR),
        ("LITERATA_ITALIC", &regular::LITERATA_ITALIC),
        ("LITERATA_BOLD", &regular::LITERATA_BOLD),
        ("LITERATA_BOLD_ITALIC", &regular::LITERATA_BOLD_ITALIC),
        ("LITERATA_19_SEMIBOLD", &semibold::LITERATA_19_SEMIBOLD),
        (
            "LITERATA_19_SEMIBOLD_ITALIC",
            &semibold::LITERATA_19_SEMIBOLD_ITALIC,
        ),
        ("LITERATA_22_SEMIBOLD", &semibold::LITERATA_22_SEMIBOLD),
        (
            "LITERATA_22_SEMIBOLD_ITALIC",
            &semibold::LITERATA_22_SEMIBOLD_ITALIC,
        ),
        ("LITERATA_26_SEMIBOLD", &semibold::LITERATA_26_SEMIBOLD),
        (
            "LITERATA_26_SEMIBOLD_ITALIC",
            &semibold::LITERATA_26_SEMIBOLD_ITALIC,
        ),
        ("LITERATA_19_REGULAR", &sizes::LITERATA_19_REGULAR),
        ("LITERATA_19_ITALIC", &sizes::LITERATA_19_ITALIC),
        ("LITERATA_19_BOLD", &sizes::LITERATA_19_BOLD),
        ("LITERATA_19_BOLD_ITALIC", &sizes::LITERATA_19_BOLD_ITALIC),
        ("LITERATA_26_REGULAR", &sizes::LITERATA_26_REGULAR),
        ("LITERATA_26_ITALIC", &sizes::LITERATA_26_ITALIC),
        ("LITERATA_26_BOLD", &sizes::LITERATA_26_BOLD),
        ("LITERATA_26_BOLD_ITALIC", &sizes::LITERATA_26_BOLD_ITALIC),
        (
            "MERRIWEATHER_19_REGULAR",
            &merriweather::MERRIWEATHER_19_REGULAR,
        ),
        (
            "MERRIWEATHER_19_ITALIC",
            &merriweather::MERRIWEATHER_19_ITALIC,
        ),
        ("MERRIWEATHER_19_BOLD", &merriweather::MERRIWEATHER_19_BOLD),
        (
            "MERRIWEATHER_19_BOLD_ITALIC",
            &merriweather::MERRIWEATHER_19_BOLD_ITALIC,
        ),
        (
            "MERRIWEATHER_22_REGULAR",
            &merriweather::MERRIWEATHER_22_REGULAR,
        ),
        (
            "MERRIWEATHER_22_ITALIC",
            &merriweather::MERRIWEATHER_22_ITALIC,
        ),
        ("MERRIWEATHER_22_BOLD", &merriweather::MERRIWEATHER_22_BOLD),
        (
            "MERRIWEATHER_22_BOLD_ITALIC",
            &merriweather::MERRIWEATHER_22_BOLD_ITALIC,
        ),
        (
            "MERRIWEATHER_26_REGULAR",
            &merriweather::MERRIWEATHER_26_REGULAR,
        ),
        (
            "MERRIWEATHER_26_ITALIC",
            &merriweather::MERRIWEATHER_26_ITALIC,
        ),
        ("MERRIWEATHER_26_BOLD", &merriweather::MERRIWEATHER_26_BOLD),
        (
            "MERRIWEATHER_26_BOLD_ITALIC",
            &merriweather::MERRIWEATHER_26_BOLD_ITALIC,
        ),
    ]
}

/// The bitmap bytes glyph `index` owns.
fn glyph_bitmap(font: &BitmapFont, index: usize) -> &[u8] {
    let metric = &font.metrics[index];
    let start = metric.offset as usize;
    &font.bitmap[start..start + metric.len as usize]
}

/// FNV-1a over what decides the render: the codepoint, the box, the advance,
/// and the pixels.
///
/// `offset` and `len` stay out of it deliberately. They are storage layout,
/// not glyph identity, so packing the bitmap differently — sharing one slice
/// between the hundreds of codepoints that store the same `.notdef` box, say —
/// leaves every fingerprint here untouched, which is the correct answer for a
/// change that moves no pixel.
fn fingerprint(font: &BitmapFont) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(&[font.line_height, font.baseline]);
    for (index, codepoint) in font.codepoints.iter().enumerate() {
        let metric = &font.metrics[index];
        eat(&codepoint.to_le_bytes());
        eat(&[
            metric.width,
            metric.height,
            metric.x_offset as u8,
            metric.y_offset as u8,
        ]);
        eat(&metric.advance_fp.to_le_bytes());
        eat(glyph_bitmap(font, index));
    }
    hash
}

fn ink_bits(font: &BitmapFont) -> u64 {
    (0..font.metrics.len())
        .map(|index| {
            glyph_bitmap(font, index)
                .iter()
                .map(|byte| byte.count_ones() as u64)
                .sum::<u64>()
        })
        .sum()
}

fn row((name, font): (&'static str, &'static BitmapFont)) -> Row {
    let glyphs = font.codepoints.len();
    (name, glyphs, ink_bits(font), fingerprint(font))
}

/// One pinned row, in the shape it is pasted back as.
fn bless_line((name, count, ink, hash): &Row) -> String {
    format!("    (\"{name}\", {count}, {ink}, {hash:#018x}),\n")
}

#[test]
fn shipped_glyph_tables_match_pinned_fingerprints() {
    let observed: Vec<Row> = tables().into_iter().map(row).collect();

    if observed.as_slice() == PINNED {
        return;
    }

    let mut report = String::from(
        "shipped glyph tables changed. Inspect the specimen frames \
         (tools/emulator/tests/reading_golden.rs), then bless with:\n\n\
         #[rustfmt::skip]\nconst PINNED: &[Row] = &[\n",
    );
    for observed_row in &observed {
        report.push_str(&bless_line(observed_row));
    }
    report.push_str("];\n\nmoved:\n");
    for row in &observed {
        let was = PINNED.iter().find(|pinned| pinned.0 == row.0);
        match was {
            Some(was) if was == row => {}
            Some(was) => report.push_str(&format!(
                "  {}: glyphs {} -> {}, ink {} -> {}\n",
                row.0, was.1, row.1, was.2, row.2
            )),
            None => report.push_str(&format!("  {}: not pinned\n", row.0)),
        }
    }
    for pinned in PINNED {
        if !observed.iter().any(|row| row.0 == pinned.0) {
            report.push_str(&format!("  {}: gone\n", pinned.0));
        }
    }
    panic!("{report}");
}

/// The structural promises `BitmapFont::glyph` makes: it binary-searches the
/// codepoints, indexes `metrics` with what it finds, and slices `bitmap` by the
/// metric's own offset and length. Nothing else checks that a generator keeps
/// those three in step.
#[test]
fn shipped_glyph_tables_are_internally_consistent() {
    for (name, font) in tables() {
        assert_eq!(
            font.codepoints.len(),
            font.metrics.len(),
            "{name}: a metric per codepoint"
        );
        assert!(
            font.codepoints.windows(2).all(|pair| pair[0] < pair[1]),
            "{name}: codepoints must be sorted and unique for the binary search"
        );
        for (index, codepoint) in font.codepoints.iter().enumerate() {
            let metric = &font.metrics[index];
            let stride = metric.width.div_ceil(8) as usize;
            assert_eq!(
                metric.len as usize,
                stride * metric.height as usize,
                "{name}: U+{codepoint:04X} stores {} bytes for a {}x{} box",
                metric.len,
                metric.width,
                metric.height
            );
            assert!(
                metric.offset as usize + metric.len as usize <= font.bitmap.len(),
                "{name}: U+{codepoint:04X} runs past the bitmap"
            );
            assert_eq!(
                font.glyph(*codepoint).map(|(found, _)| found),
                Some(metric),
                "{name}: U+{codepoint:04X} does not look up to its own metric"
            );
        }
    }
}
