import hashlib
import os
import struct
from pathlib import Path
from urllib.request import urlretrieve

import PIL
from PIL import Image, ImageDraw

# The tables in `display/src/*_generated.rs` reproduce byte for byte on this
# Pillow and not on later ones. Pillow 11 changed `getlength` from the hinted
# advance to the unhinted one, so every advance in every shipped face moves:
# Literata's space at 16px goes 3.0px -> 3.1875px, `A` 12.0 -> 11.75, `m`
# 16.0 -> 15.5. Advances are a wrap input, so accepting that silently would
# move every wrap point in every book, force a `READER_LAYOUT_VERSION` bump
# and repaginate every cached book on every device.
#
# Adopting the newer metrics may well be right -- unhinted advances are the
# more usual choice -- but it is a typography decision with a cache rebuild
# attached, not something a generator run should do by accident. Until someone
# makes that call deliberately, the version is pinned and a mismatch stops the
# run.
PILLOW_PIN = "10.4.0"

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
# `generate_mockup_fonts.py` antialiases and cuts here to decide its pixels.
# The shipped faces take their pixels from the monochrome raster instead, but
# still cut here to decide where the antialiased reference glyph's ink starts,
# which is what `rasterize_glyph` seats the monochrome raster against.
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
    """Stop before generating anything on a Pillow that changes the metrics."""
    if PIL.__version__ == PILLOW_PIN:
        return
    raise SystemExit(
        f"fontgen needs Pillow {PILLOW_PIN}, found {PIL.__version__}.\n"
        f"  Later Pillow returns unhinted advances, which moves every advance in\n"
        f"  every shipped face and repaginates every cached book on every device.\n"
        f"  Install the pin, ideally into a throwaway environment:\n"
        f"    python3.12 -m venv .fontgen && .fontgen/bin/pip install 'pillow=={PILLOW_PIN}'\n"
        f"    .fontgen/bin/python tools/generate_literata.py\n"
        f"  If the intent is to adopt the newer metrics, that is a deliberate\n"
        f"  typography change: bump READER_LAYOUT_VERSION, re-bless the goldens\n"
        f"  and the fingerprints in display/tests/glyph_tables.rs, and move this\n"
        f"  pin in the same commit."
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
        urlretrieve(url, path)
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


# Room around the reported box for the monochrome raster to land outside it,
# so its ink can be measured and moved rather than clipped where it falls.
RESEAT_PAD = 8


def _ink_bounds(image, threshold: int):
    """`(left, top, right, bottom)` of the inked pixels, or None if blank."""
    pixels = image.load()
    width, height = image.size
    columns = [x for x in range(width) if any(pixels[x, y] >= threshold for y in range(height))]
    rows = [y for y in range(height) if any(pixels[x, y] >= threshold for x in range(width))]
    if not columns or not rows:
        return None
    return (columns[0], rows[0], columns[-1] + 1, rows[-1] + 1)


def _clamped_shift(desired: int, ink_low: int, ink_high: int, box_low: int, box_high: int) -> int:
    """`desired`, reduced as far as needed to keep the ink inside the box.

    When the ink is larger than the box no shift avoids clipping, so the
    rasterizer's own placement stands rather than trading one clipped edge
    for the other.
    """
    low = box_low - ink_low
    high = box_high - ink_high
    if low > high:
        return 0
    return max(low, min(desired, high))


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
    # Rasterize straight to 1 bit rather than antialiasing to 8 and cutting at
    # a threshold. A "1"-mode canvas makes Pillow ask FreeType for its
    # monochrome target, which selects the hinting algorithm built for a
    # bilevel grid and applies dropout control; `FT_LOAD_MONOCHROME` on its
    # own does not change hinting, and neither does thresholding after the
    # fact. Thresholding an antialiased bitmap is what produced asymmetric
    # bowls and stems that changed width partway down a stroke: a stem landing
    # half-covered across two pixel columns rounds to one column or two
    # depending only on where it happens to fall.
    #
    # The mono hinter grid-fits vertical edges, and for some glyphs that lifts
    # the whole glyph a pixel: Literata's `f` at 22px and `t` at 19px, and
    # Merriweather's `k` and `r`, all render with their crossbar or foot one
    # row above where the antialiased raster puts it, while their neighbours
    # stay put. A crossbar breaking the x-height line is the most visible
    # defect this pipeline can produce -- the eye tracks that horizontal far
    # more strongly than a foot leaving the baseline. Asking `getbbox` for the
    # "1"-mode box does not address it: that box is identical to the
    # antialiased one for `r` and `k`, and the shift is in the ink, not the
    # box.
    #
    # So the antialiased render is used as the placement reference and the
    # monochrome render supplies the pixels. Both go onto one padded canvas,
    # the mono ink is moved so its top-left corner matches the antialiased
    # ink's, and the shift is clamped to keep it inside the box. Stems and
    # bowls stay the mono hinter's -- that is the quality this buys -- while
    # the glyph sits where the outline says it sits.
    canvas = (width + 2 * RESEAT_PAD, height + 2 * RESEAT_PAD)
    origin = (-left + RESEAT_PAD, -top + RESEAT_PAD)

    image = Image.new("1", canvas, 0)
    ImageDraw.Draw(image).text(origin, ch, font=font, fill=1, anchor="ls")
    reference = Image.new("L", canvas, 0)
    ImageDraw.Draw(reference).text(origin, ch, font=font, fill=255, anchor="ls")

    mono_ink = _ink_bounds(image, 1)
    reference_ink = _ink_bounds(reference, THRESHOLD)
    shift_x = shift_y = 0
    if mono_ink is not None and reference_ink is not None:
        shift_x = _clamped_shift(
            reference_ink[0] - mono_ink[0],
            mono_ink[0],
            mono_ink[2],
            RESEAT_PAD,
            RESEAT_PAD + width,
        )
        shift_y = _clamped_shift(
            reference_ink[1] - mono_ink[1],
            mono_ink[1],
            mono_ink[3],
            RESEAT_PAD,
            RESEAT_PAD + height,
        )

    pixels = image.load()
    rows = []
    for y in range(height):
        byte = 0
        bits = 0
        for x in range(width):
            source_x = RESEAT_PAD + x - shift_x
            source_y = RESEAT_PAD + y - shift_y
            if (
                0 <= source_x < canvas[0]
                and 0 <= source_y < canvas[1]
                and pixels[source_x, source_y]
            ):
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
