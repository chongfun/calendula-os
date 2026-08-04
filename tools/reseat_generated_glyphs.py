#!/usr/bin/env python3
"""One-time repair: seat the shipped monochrome glyphs where the antialiased
ones sat.

`rasterize_glyph` now renders an antialiased reference alongside the
monochrome raster and moves the mono ink onto it, because the mono hinter
grid-fits vertical edges and lifts some glyphs a whole pixel -- Literata's `f`
at 22px and `t` at 19px, Merriweather's `k` and `r` -- so their crossbar or
foot breaks the line their neighbours hold.

Regenerating the tables from that fixed generator is the natural way to apply
it, and it is not available: the generators download Literata and Merriweather
from a moving `main` branch, and upstream has since revised the fonts. A
regeneration today changes 198 of 218 advances in the smallest table alone,
which would move every wrap point, force a `READER_LAYOUT_VERSION` bump and a
full cache rebuild on every device, and bury a one-pixel seating fix inside an
unreviewed font revision.

So the repair is applied to the checked-in tables directly. The reference is
`main`'s copy of the same tables, which is the antialiased rasterization of the
identical font -- all 49,802 advances match between `main` and the current
tables, which is what proves the outlines never changed. For every glyph the
mono ink is moved so its top-left corner sits where `main`'s antialiased ink
did, clamped to stay inside the box.

Only bitmap bytes change. Every `GlyphMetric` field -- offset, len, width,
height, x_offset, y_offset, advance_fp -- is left exactly as it is, so no wrap
input moves, `READER_LAYOUT_VERSION` stays at 19, and devices keep the caches
they have already rebuilt.

Usage: tools/reseat_generated_glyphs.py [--reference main] [--check]
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FILES = [
    "display/src/literata_extra_generated.rs",
    "display/src/literata_generated.rs",
    "display/src/literata_semibold_generated.rs",
    "display/src/literata_sizes_generated.rs",
    "display/src/merriweather_generated.rs",
]

METRICS_RE = re.compile(r"static (\w+)_METRICS: \[GlyphMetric; \w+\] = \[(.*?)\];", re.S)
BITMAP_RE = re.compile(r"static (\w+)_BITMAP: \[u8; (\d+)\] = \[(.*?)\];", re.S)
GLYPH_RE = re.compile(
    r"GlyphMetric \{ offset: (-?\d+), len: (-?\d+), width: (-?\d+), height: (-?\d+), "
    r"x_offset: (-?\d+), y_offset: (-?\d+), advance_fp: (-?\d+) \}"
)


def read_revision(revision: str, path: str) -> str:
    return subprocess.run(
        ["git", "show", f"{revision}:{path}"],
        capture_output=True,
        text=True,
        check=True,
        cwd=ROOT,
    ).stdout


def parse(text: str):
    """{table name: (metrics, bitmap bytes)}."""
    bitmaps = {
        match.group(1): [int(b, 16) for b in re.findall(r"0x([0-9A-Fa-f]{2})", match.group(3))]
        for match in BITMAP_RE.finditer(text)
    }
    tables = {}
    for match in METRICS_RE.finditer(text):
        name = match.group(1)
        metrics = [tuple(int(g) for g in m.groups()) for m in GLYPH_RE.finditer(match.group(2))]
        tables[name] = (metrics, bitmaps[name])
    return tables


def unpack(metric, data):
    """The glyph as a list of pixel rows."""
    offset, _length, width, height, _x, _y, _advance = metric
    stride = (width + 7) // 8
    return [
        [(data[offset + row * stride + (col >> 3)] >> (7 - (col & 7))) & 1 for col in range(width)]
        for row in range(height)
    ]


def ink_origin(pixels):
    """(left, top) of the inked pixels within the glyph box, or None."""
    rows = [r for r, row in enumerate(pixels) if any(row)]
    if not rows:
        return None
    columns = [c for c in range(len(pixels[0])) if any(row[c] for row in pixels)]
    if not columns:
        return None
    return (columns[0], rows[0])


def ink_extent(pixels):
    """(left, top, right, bottom) of the inked pixels, or None."""
    rows = [r for r, row in enumerate(pixels) if any(row)]
    if not rows:
        return None
    columns = [c for c in range(len(pixels[0])) if any(row[c] for row in pixels)]
    return (columns[0], rows[0], columns[-1] + 1, rows[-1] + 1)


def clamped_shift(desired, ink_low, ink_high, box_high):
    """`desired`, reduced as far as needed to keep the ink inside the box."""
    low = -ink_low
    high = box_high - ink_high
    if low > high:
        return 0
    return max(low, min(desired, high))


def reseat(metric, data, reference_metric, reference_data):
    """The glyph's bytes, with its ink moved onto the reference's placement."""
    offset, length, width, height, x_offset, y_offset, _advance = metric
    if width == 0 or height == 0 or length == 0:
        return data[offset : offset + length], 0, 0

    pixels = unpack(metric, data)
    extent = ink_extent(pixels)
    reference_origin = ink_origin(unpack(reference_metric, reference_data))
    if extent is None or reference_origin is None:
        return data[offset : offset + length], 0, 0

    # Both origins are box-relative, so the bearings have to come back in to
    # compare them against the same pen position.
    reference_x = reference_metric[4] + reference_origin[0]
    reference_y = reference_metric[5] + reference_origin[1]
    shift_x = clamped_shift(reference_x - (x_offset + extent[0]), extent[0], extent[2], width)
    shift_y = clamped_shift(reference_y - (y_offset + extent[1]), extent[1], extent[3], height)

    stride = (width + 7) // 8
    out = bytearray(stride * height)
    for row in range(height):
        source_row = row - shift_y
        if not 0 <= source_row < height:
            continue
        for col in range(width):
            source_col = col - shift_x
            if 0 <= source_col < width and pixels[source_row][source_col]:
                out[row * stride + (col >> 3)] |= 0x80 >> (col & 7)
    return list(out), shift_x, shift_y


def render_bitmap(name, data):
    # `BITMAP_RE` matches from `static`, so the `#[rustfmt::skip]` and `pub`
    # ahead of it stay in the file and must not be emitted again here.
    lines = [f"static {name}_BITMAP: [u8; {len(data)}] = [\n"]
    for start in range(0, len(data), 16):
        chunk = data[start : start + 16]
        lines.append("    " + ", ".join(f"0x{b:02X}" for b in chunk) + ",\n")
    lines.append("];")
    return "".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--reference", default="main", help="revision holding the antialiased tables"
    )
    parser.add_argument("--check", action="store_true", help="report without writing")
    args = parser.parse_args()

    moved_total = glyphs_total = 0
    for path in FILES:
        current_text = (ROOT / path).read_text()
        current = parse(current_text)
        reference = parse(read_revision(args.reference, path))
        rebuilt = {}
        moved_here = 0
        for name, (metrics, data) in current.items():
            reference_metrics, reference_data = reference[name]
            out = []
            for metric, reference_metric in zip(metrics, reference_metrics, strict=True):
                glyphs_total += 1
                bytes_, shift_x, shift_y = reseat(metric, data, reference_metric, reference_data)
                if shift_x or shift_y:
                    moved_here += 1
                out.extend(bytes_)
            if len(out) != len(data):
                raise SystemExit(f"{name}: bitmap length changed {len(data)} -> {len(out)}")
            rebuilt[name] = out
        moved_total += moved_here
        print(f"{path}: {moved_here} glyphs re-seated")
        if args.check:
            continue
        for name, data in rebuilt.items():
            current_text = BITMAP_RE.sub(
                lambda m, n=name, d=data: render_bitmap(n, d) if m.group(1) == n else m.group(0),
                current_text,
            )
        (ROOT / path).write_text(current_text)

    print(f"\n{moved_total} of {glyphs_total} glyphs moved")
    return 0


if __name__ == "__main__":
    sys.exit(main())
