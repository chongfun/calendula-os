import hashlib
import os
import struct
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

import PIL
from bitmap_pool import BitmapPool as BitmapPool
from PIL import Image, ImageDraw, features

# The tables in `display/src/*_generated.rs` reproduce byte for byte on this
# Pillow/FreeType pair and not on the 10.4.0/2.13.2 pair pinned previously.
# Verified 2026-08-05 by running all five generators unmodified under each:
# 12.3.0/2.14.3 leaves the tree clean; 10.4.0/2.13.2 moves boxes and bitmaps
# in every face -- FreeType's monochrome rasterization changed between 2.13
# and 2.14, most visibly on the diacritic/fraction set (Ï ì ï ¾ _ ...) whose
# ink boxes grew a pixel under 2.14 -- so the old pin could not regenerate
# what is shipped. The getlength numbers the previous pin cited did not
# reproduce here either: 10.4.0 and 12.3.0 return identical hinted advances
# on this machine, so the advance risk that motivated 10.4.0 never applied
# to this environment. If a regeneration on another machine trips this guard,
# measure before moving the pin: the shipped bytes, not a version number,
# are the thing being protected.
#
# Boxes and advances are wrap inputs, so a toolchain that moves them silently
# would move wrap points in every book, force a `READER_LAYOUT_VERSION` bump
# and repaginate every cached book on every device. Adopting new metrics is a
# typography decision with a cache rebuild attached, not something a generator
# run should do by accident; a mismatch stops the run.
PILLOW_PIN = "12.3.0"
FREETYPE_PIN = "2.14.3"

# SHA-256 of every TTF the shipped tables were built from, checked on every
# run because `tools/fonts/` is gitignored and a stale local file is as
# damaging as a changed upstream.
FONT_SHA256 = {
    "Literata-Regular.ttf": "0390890de9bb9d5862a6ba4125b82c61792ccc3d66b63e73eee75c1a16fcd208",
    "Literata-Italic.ttf": "198f70cc9a17bab578553fa274b81984d58c440efe26bc06f1d841c194b6691a",
    "Literata-Bold.ttf": "b6af95b3b443cdbce964aa06741596987f2f5c3ede46a2bc846e5addd99d061f",
    "Literata-BoldItalic.ttf": "1dade59381ad02f4679d7e993a7a406472e587ac332af7a6168d3ad9214a99dc",
    "Literata-SemiBold.ttf": "ee8f9413ebc974e1c1cfc76f6bdb9d08ddaadc66eeddd7320a65f8c581284d6d",
    "Literata-SemiBoldItalic.ttf": "bf22f3e03804210abc5ee62362eb80fdd20829f69ef73fe1213347022b26963d",
    "Merriweather.ttf": "d0ed0e359e396af7ad05e73dffd11a3a4c326ea0d0283c56bd9361cb2cc86a96",
    "Merriweather-Italic.ttf": "f68a8f4989258679e4fbaf50aa42400132b5373c2d9d2514ba82ef6e85947a0b",
}
# Pinned to the commits the shipped tables were built from: Literata 3.103,
# unchanged upstream since 2023-05-19, and Merriweather 2.100, unchanged since
# 2025-01-29. google/fonts especially is a live monorepo, so a moving ref is one
# release away from moving every advance without anyone choosing it.
LITERATA_COMMIT = "0c2761b727a1b3a7cffd313c37f0f5163dfc7a63"
LITERATA_BASE = (
    f"https://raw.githubusercontent.com/googlefonts/literata/{LITERATA_COMMIT}/fonts/ttf"
)
MERRIWEATHER_COMMIT = "4fc3d16c59a4d5df700d37cbd9693e0d53f8d991"
MERRIWEATHER_BASE = f"https://github.com/google/fonts/raw/{MERRIWEATHER_COMMIT}/ofl/merriweather"

ADVANCE_SCALE = 16
# Only `generate_mockup_fonts.py` antialiases and cuts here to decide its
# pixels. The shipped faces cut at `AA_INK_THRESHOLD` in `rasterize_glyph`
# instead, and no longer read this.
DEFAULT_THRESHOLD = 128
MIN_KERNING_ADJUST_FP = 8
MAX_KERNING_ENTRIES = 1024
KERNING_CODEPOINTS = frozenset(
    list(range(0x20, 0x7F)) + [0x2018, 0x2019, 0x201C, 0x201D, 0x2013, 0x2014, 0x2026]
)


def text_render_threshold() -> int:
    value = int(os.environ.get("TEXT_RENDER_THRESHOLD", str(DEFAULT_THRESHOLD)))
    if not 0 <= value <= 255:
        raise ValueError("TEXT_RENDER_THRESHOLD must be in 0..=255")
    return value


THRESHOLD = text_render_threshold()


def require_pinned_pillow() -> None:
    """Stop before generating anything on a toolchain that changes metrics or pixels."""
    freetype_ver = features.version_module("freetype2")
    if PIL.__version__ != PILLOW_PIN or freetype_ver != FREETYPE_PIN:
        raise SystemExit(
            f"fontgen needs Pillow {PILLOW_PIN} and FreeType {FREETYPE_PIN}, "
            f"found Pillow {PIL.__version__} / FreeType {freetype_ver}.\n"
            f"  Different Pillow/FreeType builds alter glyph rasterization, placement,\n"
            f"  and sometimes advances.\n"
            f"  Install the pin, ideally into a throwaway environment:\n"
            f"    python3.12 -m venv .fontgen && .fontgen/bin/pip install 'pillow=={PILLOW_PIN}'\n"
            f"    .fontgen/bin/python tools/generate_literata.py\n"
            f"  If the intent is to adopt newer metrics or FreeType, that is a deliberate\n"
            f"  typography change: bump READER_LAYOUT_VERSION, re-bless the goldens\n"
            f"  and the fingerprints in display/tests/glyph_tables.rs, and move these\n"
            f"  pins in the same commit."
        )


def require_pinned_threshold() -> None:
    """Mockup-only guard. The shipped faces cut at `AA_INK_THRESHOLD` and never
    read `TEXT_RENDER_THRESHOLD`, so only `generate_mockup_fonts.py` calls this."""
    if THRESHOLD != DEFAULT_THRESHOLD and os.environ.get("ALLOW_UNPINNED_THRESHOLD") != "1":
        raise SystemExit(
            f"generate_mockup_fonts requires THRESHOLD {DEFAULT_THRESHOLD}, "
            f"found {THRESHOLD} via TEXT_RENDER_THRESHOLD.\n"
            f"  The mockup fonts cut antialiased coverage at this value, so overriding\n"
            f"  it changes their pixels. It has no effect on the shipped faces.\n"
            f"  For experiments, set ALLOW_UNPINNED_THRESHOLD=1 to bypass."
        )


def ensure_font(path: Path, url: str, sha256: str) -> None:
    """Download `url` to `path` if absent, and verify its hash either way.

    The hash is checked on every run, not only after a download, because the
    fonts live in a gitignored directory: a stale or hand-placed file is
    exactly as damaging as a changed upstream, and neither announces itself.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.exists():
        print(f"downloading {path.name}")
        try:
            with urlopen(url, timeout=30) as response:
                path.write_bytes(response.read())
        except (HTTPError, URLError, OSError) as err:
            if path.exists():
                path.unlink(missing_ok=True)
            raise SystemExit(f"Failed to download {path.name} from {url}: {err}") from err
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != sha256:
        raise SystemExit(
            f"{path} does not match its pinned hash.\n"
            f"  expected {sha256}\n"
            f"  found    {digest}\n"
            f"  source   {url}\n"
            f"  Delete the file to re-download. If upstream genuinely moved, adopting\n"
            f"  the new outlines is a deliberate change: it moves advances, so bump\n"
            f"  READER_LAYOUT_VERSION and re-bless goldens and fingerprints with it."
        )


def advance_fp(font, text: str) -> int:
    return max(round(font.getlength(text) * ADVANCE_SCALE), ADVANCE_SCALE)


# Ink cut for the antialiased raster: a pixel FreeType covers at 112/255
# (~44%) or more becomes black. Below 50% deliberately -- e-ink renders a
# stroke thinner than the same bitmap on an emissive screen, so the cut errs
# toward ink and a stem edge at half coverage keeps its column instead of
# losing it to rounding. crosspoint-reader ships the same idea at a far
# lower cut (any coverage >= ~12.5%), but its boxes are derived from the
# same render and grow with the ink; ours are frozen cache geometry, so a
# cut that low pushes the coverage halo a pixel past the box everywhere and
# clips whole edge rows off reading glyphs.
#
# The value is from a sweep of 32..128 over every shipped render. Ink gain
# against the previous tables by cut: 32 +25%, 64 +14%, 96 +8%, 112 +3.5%,
# 128 +0.3%; box clipping collapses to noise at 64 and above. 96 looked like
# the moderate choice until the per-face split: Literata's 22 px stems --
# the default reading configuration -- have their antialiased edges at
# coverage 96..112, so any cut at or below 96 swallows them wholesale
# (+20..31% on those faces alone, a weight change, not darkening) while
# every other size moves +4..7%. At 112 every face lands between +1% and
# +5% except the 22 px italics (+11..15%), whose thinner slanted strokes
# darkening slightly is the direction e-ink wants anyway.
AA_INK_THRESHOLD = 112

# Clip audit. The boxes are frozen cache geometry, so ink the antialiased
# raster puts outside them is cut off -- silently, unless someone counts it.
# `rasterize_glyph` renders on a padded canvas and records every thresholded
# pixel that falls outside the box; each shipped generator writes the tally
# to `tools/clip-reports/<name>.txt`, which is checked in. A regeneration
# rewrites the report alongside the table, so the git diff -- not anyone's
# memory of a scratch analysis -- is where new clipping shows up for review
# when the fonts, the cut, or the toolchain move.
CLIP_AUDIT_PAD = 8
# Tripwire, not a budget: the committed report is the allowlist, and every
# entry in it was accepted by the commit that blessed it. The hard failure
# below is only the absurdity backstop -- a render that loses more than
# CLIP_LIMIT_PER_GLYPH pixels AND more than twice what it keeps is a glyph
# rendering mostly outside its box, which no report review should wave
# through. Both arms are needed: U+25A0 at 22 px loses 17 px that are just
# the faint border ring of a 200+ px solid body, and Merriweather's 26 px
# em-dash legitimately loses as much as it keeps (25 px / 25 px) because its
# frozen box is one row shorter than the antialiased bar -- the same one-row
# dash the monochrome tables shipped, documented in the report, and exactly
# the kind of entry the report exists to keep visible.
CLIP_LIMIT_PER_GLYPH = 16
_clip_records = []
_clip_face = None


def set_clip_face(label) -> None:
    """Name the face being rasterized in clip records.

    Only needed where the font file does not identify the face: Merriweather's
    styles are axis instances of two variable TTFs, so path plus size collides.
    The static-cut generators never call this and key by filename.
    """
    global _clip_face
    _clip_face = label


def write_clip_report(path: Path) -> None:
    """Write this run's clipped-ink tally, ASCII first because it matters most."""
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        "# Ink clipped by the frozen glyph boxes, in pixels of >= AA_INK_THRESHOLD",
        "# coverage per render. Rewritten by the generator on every run; review the",
        "# diff of this file whenever the fonts, the cut, or the toolchain change.",
        "# An empty section means no render in it clips anything.",
        "",
        "[ascii]",
    ]
    records = sorted(set(_clip_records))
    for section, keep in (("ascii", lambda cp: cp < 0x80), ("non-ascii", lambda cp: cp >= 0x80)):
        if section == "non-ascii":
            lines.extend(["", "[non-ascii]"])
        for font_file, size, code, clipped in records:
            if keep(code):
                lines.append(f"{font_file} {size}px U+{code:04X} {chr(code)!r} {clipped}px")
    path.write_text("\n".join(lines) + "\n")


def rasterize_glyph(font, code: int):
    ch = chr(code)
    bbox = font.getbbox(ch, anchor="ls", mode="1")
    advance = advance_fp(font, ch)
    if bbox is None:
        return (0, 0, 0, 0, advance, [])
    left, top, right, bottom = bbox
    width = max(0, right - left)
    height = max(0, bottom - top)
    if width == 0 or height == 0:
        return (0, 0, left, top, advance, [])
    # One antialiased render, cut low, in place of the monochrome raster and
    # the re-seat that placed it. The mono hinter grid-fits each glyph on its
    # own, so neighbouring letters could come out with different stem weights
    # and a lifted crossbar; and because its ink had to be re-seated against a
    # separately-rendered antialiased reference, every disagreement between
    # the two renders became a one-pixel placement lottery. A single render
    # cannot disagree with itself: the ink and its seat come from the same
    # rasterization, which is how crosspoint-reader builds every face it
    # ships. The low cut (see AA_INK_THRESHOLD) is what keeps stems from
    # dropping columns, the failure the mono experiment was meant to fix.
    #
    # The box stays the shipped table's box -- cache geometry, frozen. Ink
    # the antialiased raster puts outside it clips where it falls, but never
    # silently: the canvas is padded so the clipped pixels can be counted,
    # recorded for the clip report, and capped. The padding cannot change the
    # kept pixels -- rendering at an integer offset shifts the raster without
    # re-hinting it -- which the byte-identical regeneration check proves.
    pad = CLIP_AUDIT_PAD
    image = Image.new("L", (width + 2 * pad, height + 2 * pad), 0)
    ImageDraw.Draw(image).text((pad - left, pad - top), ch, font=font, fill=255, anchor="ls")

    pixels = image.load()
    clipped = kept = 0
    for y in range(height + 2 * pad):
        inside_rows = pad <= y < pad + height
        for x in range(width + 2 * pad):
            if pixels[x, y] >= AA_INK_THRESHOLD:
                if inside_rows and pad <= x < pad + width:
                    kept += 1
                else:
                    clipped += 1
    if clipped:
        _clip_records.append((_clip_face or Path(font.path).name, font.size, code, clipped))
        if clipped > CLIP_LIMIT_PER_GLYPH and clipped > 2 * kept:
            raise SystemExit(
                f"U+{code:04X} {ch!r} at {font.size}px in {Path(font.path).name} would lose "
                f"{clipped} thresholded pixels to its frozen box while keeping only {kept}.\n"
                f"  That is a disfigured glyph, not halo trimming. Something moved the ink\n"
                f"  relative to the box: the font file, AA_INK_THRESHOLD, or the toolchain."
            )

    rows = []
    for y in range(height):
        byte = 0
        bits = 0
        for x in range(width):
            if pixels[pad + x, pad + y] >= AA_INK_THRESHOLD:
                byte |= 0x80 >> bits
            bits += 1
            if bits == 8:
                rows.append(byte)
                byte = 0
                bits = 0
        if bits:
            rows.append(byte)
    return (width, height, left, top, advance, rows)


def codepoints_from_ranges(ranges):
    values = []
    for start, end in ranges:
        values.extend(range(start, end + 1))
    return sorted(set(values))


def u16(data, offset):
    return struct.unpack_from(">H", data, offset)[0]


def i16(data, offset):
    return struct.unpack_from(">h", data, offset)[0]


def u32(data, offset):
    return struct.unpack_from(">I", data, offset)[0]


def font_tables(path: Path):
    data = path.read_bytes()
    num_tables = u16(data, 4)
    tables = {}
    for i in range(num_tables):
        offset = 12 + i * 16
        tag = data[offset : offset + 4].decode("ascii", errors="replace")
        tables[tag] = (u32(data, offset + 8), u32(data, offset + 12))
    return data, tables


def parse_cmap(data, tables):
    if "cmap" not in tables:
        return {}
    base, _ = tables["cmap"]
    count = u16(data, base + 2)
    best = None
    best_rank = -1
    for i in range(count):
        rec = base + 4 + i * 8
        platform = u16(data, rec)
        encoding = u16(data, rec + 2)
        offset = u32(data, rec + 4)
        table = base + offset
        fmt = u16(data, table)
        rank = {
            (12, 3, 10): 5,
            (12, 0, 4): 4,
            (4, 3, 1): 3,
            (4, 0, 3): 2,
            (4, 0, 1): 1,
        }.get((fmt, platform, encoding), 0)
        if rank > best_rank:
            best = table
            best_rank = rank
    if best is None:
        return {}
    fmt = u16(data, best)
    mapping = {}
    if fmt == 4:
        seg_count = u16(data, best + 6) // 2
        end_codes = best + 14
        start_codes = end_codes + seg_count * 2 + 2
        id_deltas = start_codes + seg_count * 2
        id_range_offsets = id_deltas + seg_count * 2
        for i in range(seg_count):
            end = u16(data, end_codes + i * 2)
            start = u16(data, start_codes + i * 2)
            delta = i16(data, id_deltas + i * 2)
            range_offset = u16(data, id_range_offsets + i * 2)
            for cp in range(start, end + 1):
                if cp == 0xFFFF:
                    continue
                if range_offset == 0:
                    glyph = (cp + delta) & 0xFFFF
                else:
                    glyph_offset = id_range_offsets + i * 2 + range_offset + (cp - start) * 2
                    glyph = u16(data, glyph_offset)
                    if glyph:
                        glyph = (glyph + delta) & 0xFFFF
                if glyph:
                    mapping[cp] = glyph
    elif fmt == 12:
        group_count = u32(data, best + 12)
        pos = best + 16
        for _ in range(group_count):
            start = u32(data, pos)
            end = u32(data, pos + 4)
            glyph_start = u32(data, pos + 8)
            for cp in range(start, end + 1):
                if cp <= 0xFFFF:
                    mapping[cp] = glyph_start + cp - start
            pos += 12
    return mapping


def parse_coverage(data, base):
    fmt = u16(data, base)
    glyphs = []
    if fmt == 1:
        count = u16(data, base + 2)
        glyphs = [u16(data, base + 4 + i * 2) for i in range(count)]
    elif fmt == 2:
        count = u16(data, base + 2)
        pos = base + 4
        for _ in range(count):
            start = u16(data, pos)
            end = u16(data, pos + 2)
            glyphs.extend(range(start, end + 1))
            pos += 6
    return glyphs


def parse_class_def(data, base):
    fmt = u16(data, base)
    classes = {}
    if fmt == 1:
        start = u16(data, base + 2)
        count = u16(data, base + 4)
        for i in range(count):
            classes[start + i] = u16(data, base + 6 + i * 2)
    elif fmt == 2:
        count = u16(data, base + 2)
        pos = base + 4
        for _ in range(count):
            start = u16(data, pos)
            end = u16(data, pos + 2)
            cls = u16(data, pos + 4)
            for glyph in range(start, end + 1):
                classes[glyph] = cls
            pos += 6
    return classes


def value_record_size(fmt):
    return sum(2 for bit in range(8) if fmt & (1 << bit))


def read_x_advance(data, offset, fmt):
    value = 0
    pos = offset
    for bit in range(8):
        if not (fmt & (1 << bit)):
            continue
        if bit == 2:
            value = i16(data, pos)
        pos += 2
    return value


def parse_gpos_pair_adjustments(data, tables):
    if "GPOS" not in tables:
        return {}
    base, _ = tables["GPOS"]
    feature_list = base + u16(data, base + 6)
    lookup_list = base + u16(data, base + 8)

    feature_count = u16(data, feature_list)
    lookup_indices = set()
    for i in range(feature_count):
        rec = feature_list + 2 + i * 6
        tag = data[rec : rec + 4].decode("ascii", errors="replace")
        if tag != "kern":
            continue
        feature = feature_list + u16(data, rec + 4)
        count = u16(data, feature + 2)
        for j in range(count):
            lookup_indices.add(u16(data, feature + 4 + j * 2))

    adjustments = {}
    lookup_count = u16(data, lookup_list)
    for lookup_index in sorted(lookup_indices):
        if lookup_index >= lookup_count:
            continue
        lookup = lookup_list + u16(data, lookup_list + 2 + lookup_index * 2)
        lookup_type = u16(data, lookup)
        if lookup_type not in (2, 9):
            continue
        subtable_count = u16(data, lookup + 4)
        for i in range(subtable_count):
            sub = lookup + u16(data, lookup + 6 + i * 2)
            if lookup_type == 9:
                if u16(data, sub) != 1 or u16(data, sub + 2) != 2:
                    continue
                sub = sub + u32(data, sub + 4)
            parse_gpos_pair_subtable(data, sub, adjustments)
    return adjustments


def parse_gpos_pair_subtable(data, sub, adjustments):
    pos_format = u16(data, sub)
    coverage = parse_coverage(data, sub + u16(data, sub + 2))
    value_format1 = u16(data, sub + 4)
    value_format2 = u16(data, sub + 6)
    size1 = value_record_size(value_format1)
    size2 = value_record_size(value_format2)
    if pos_format == 1:
        pair_set_count = u16(data, sub + 8)
        for first_index in range(min(pair_set_count, len(coverage))):
            first = coverage[first_index]
            pair_set = sub + u16(data, sub + 10 + first_index * 2)
            pair_count = u16(data, pair_set)
            pos = pair_set + 2
            for _ in range(pair_count):
                second = u16(data, pos)
                value = read_x_advance(data, pos + 2, value_format1)
                if value:
                    adjustments[(first, second)] = value
                pos += 2 + size1 + size2
    elif pos_format == 2:
        class_def1 = parse_class_def(data, sub + u16(data, sub + 8))
        class_def2 = parse_class_def(data, sub + u16(data, sub + 10))
        class1_count = u16(data, sub + 12)
        class2_count = u16(data, sub + 14)
        glyphs_by_class1 = {}
        glyphs_by_class2 = {}
        for glyph in coverage:
            glyphs_by_class1.setdefault(class_def1.get(glyph, 0), []).append(glyph)
        for glyph, cls in class_def2.items():
            glyphs_by_class2.setdefault(cls, []).append(glyph)
        pos = sub + 16
        cell_size = size1 + size2
        for c1 in range(class1_count):
            for c2 in range(class2_count):
                value = read_x_advance(data, pos, value_format1)
                if value:
                    for first in glyphs_by_class1.get(c1, []):
                        for second in glyphs_by_class2.get(c2, []):
                            adjustments[(first, second)] = value
                pos += cell_size


def parse_kern_adjustments(data, tables):
    if "kern" not in tables:
        return {}
    base, _ = tables["kern"]
    adjustments = {}
    version = u16(data, base)
    if version != 0:
        return adjustments
    count = u16(data, base + 2)
    pos = base + 4
    for _ in range(count):
        length = u16(data, pos + 2)
        coverage = u16(data, pos + 4)
        fmt = coverage >> 8
        horizontal = coverage & 1
        if fmt == 0 and horizontal:
            pair_count = u16(data, pos + 6)
            pair_pos = pos + 14
            for _ in range(pair_count):
                left = u16(data, pair_pos)
                right = u16(data, pair_pos + 2)
                value = i16(data, pair_pos + 4)
                if value:
                    adjustments[(left, right)] = value
                pair_pos += 6
        pos += length
    return adjustments


def kerning_entries(font_path: Path, cps, px: int):
    data, tables = font_tables(font_path)
    units_per_em = u16(data, tables["head"][0] + 18)
    cmap = parse_cmap(data, tables)
    adjustments = parse_kern_adjustments(data, tables)
    adjustments.update(parse_gpos_pair_adjustments(data, tables))

    cps_by_glyph = {}
    for cp in cps:
        if cp not in KERNING_CODEPOINTS:
            continue
        glyph = cmap.get(cp)
        if glyph is not None:
            cps_by_glyph.setdefault(glyph, []).append(cp)

    entries = []
    for (left_glyph, right_glyph), value in adjustments.items():
        adjust_fp = round(value * px * ADVANCE_SCALE / units_per_em)
        if abs(adjust_fp) < MIN_KERNING_ADJUST_FP:
            continue
        for left in cps_by_glyph.get(left_glyph, []):
            for right in cps_by_glyph.get(right_glyph, []):
                entries.append((left, right, adjust_fp))
    entries = sorted(set(entries), key=lambda entry: (-abs(entry[2]), entry[0], entry[1]))
    return sorted(entries[:MAX_KERNING_ENTRIES])


def write_kerning(out, name, entries, count_name=None):
    count = count_name or f"{name}_KERNING_COUNT"
    out.append(f"#[rustfmt::skip]\npub const {count}: usize = {len(entries)};\n")
    out.append(f"#[rustfmt::skip]\npub static {name}_KERNING: [KerningEntry; {count}] = [\n")
    for left, right, adjust_fp in entries:
        out.append(
            "    KerningEntry { "
            f"left: 0x{left:04X}, right: 0x{right:04X}, adjust_fp: {adjust_fp} }},\n"
        )
    out.append("];\n\n")
