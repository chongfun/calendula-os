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
    ("LITERATA_SMALL_REGULAR", 218, 6711, 0x32aec82ad7737300),
    ("LITERATA_SMALL_BOLD", 218, 10824, 0x6d89b483ec3122e9),
    ("LITERATA_SMALL_ITALIC", 218, 6337, 0xfdb40b3ddedca6d4),
    ("LITERATA_DISPLAY_REGULAR", 218, 54726, 0x953dfedac58560a9),
    ("LITERATA_REGULAR", 1631, 102029, 0x2889ecce2e9f4ef7),
    ("LITERATA_ITALIC", 1631, 105183, 0x6f90085e480acd11),
    ("LITERATA_BOLD", 1631, 132496, 0xf77f9bff14653ba7),
    ("LITERATA_BOLD_ITALIC", 1631, 136575, 0x4c5bb4fb74f59c63),
    ("LITERATA_19_SEMIBOLD", 1631, 99982, 0x354f9290cff96cc5),
    ("LITERATA_19_SEMIBOLD_ITALIC", 1631, 94094, 0x22e93afcd12f4a01),
    ("LITERATA_22_SEMIBOLD", 1631, 121705, 0x7df026969c2ff0af),
    ("LITERATA_22_SEMIBOLD_ITALIC", 1631, 115732, 0xd9fa8c8751144f80),
    ("LITERATA_26_SEMIBOLD", 1631, 195966, 0x58e2d4362aac5b04),
    ("LITERATA_26_SEMIBOLD_ITALIC", 1631, 188419, 0xe5e7987497a7b71e),
    ("LITERATA_19_REGULAR", 1631, 85165, 0xc9e5bfb49c891504),
    ("LITERATA_19_ITALIC", 1631, 81172, 0xa7cf480c443f0ed9),
    ("LITERATA_19_BOLD", 1631, 109479, 0xc7b63f3d2f95f13a),
    ("LITERATA_19_BOLD_ITALIC", 1631, 104072, 0x9c2c3efd779696ea),
    ("LITERATA_26_REGULAR", 1631, 170614, 0xce7161bd82eafabf),
    ("LITERATA_26_ITALIC", 1631, 163422, 0x83cda9fc094a7d2c),
    ("LITERATA_26_BOLD", 1631, 213788, 0x6166edf6a4e68e12),
    ("LITERATA_26_BOLD_ITALIC", 1631, 204015, 0xabe014d28eb9d32f),
    ("MERRIWEATHER_19_REGULAR", 1631, 145836, 0xd2cfad8f2f203eab),
    ("MERRIWEATHER_19_ITALIC", 1631, 136914, 0x99b1e2f0bf3d8da9),
    ("MERRIWEATHER_19_BOLD", 1631, 165369, 0x67409ede2196b3c5),
    ("MERRIWEATHER_19_BOLD_ITALIC", 1631, 151918, 0xdcba54470595e6b0),
    ("MERRIWEATHER_22_REGULAR", 1631, 197979, 0xacc55ace847a4c07),
    ("MERRIWEATHER_22_ITALIC", 1631, 183395, 0x23dbf8a0e243009a),
    ("MERRIWEATHER_22_BOLD", 1631, 218869, 0x1cc7370021c8a889),
    ("MERRIWEATHER_22_BOLD_ITALIC", 1631, 206767, 0xaeda534d70d5468c),
    ("MERRIWEATHER_26_REGULAR", 1631, 279348, 0xc8d537031dbfbf4f),
    ("MERRIWEATHER_26_ITALIC", 1631, 258483, 0xfc83cddb39f30bc3),
    ("MERRIWEATHER_26_BOLD", 1631, 310866, 0xaaa923b44fa2598b),
    ("MERRIWEATHER_26_BOLD_ITALIC", 1631, 294172, 0x989e889569670765),
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
