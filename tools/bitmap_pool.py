#!/usr/bin/env python3
"""Bitmap byte deduplication pool for font generator tools."""

from __future__ import annotations


class BitmapPool:
    """The shared bitmap array, storing each distinct glyph once.

    The shipped ranges run past what the TTFs cover, and every uncovered
    codepoint rasterizes to the same hollow `.notdef` rectangle -- 792 copies
    of one 11x16 box in Literata's regular face alone. Each copy used to get
    its own bytes, so across the 34 shipped tables the most-repeated glyph in
    each accounted for 741,627 of 2,014,522 bitmap bytes, about 37%.

    Nothing reads the array sequentially: `BitmapFont::glyph` slices it by the
    metric's own offset and length, so two codepoints can point at one slice
    and render identically. Only `offset` changes, which is storage layout
    rather than glyph identity -- advances, boxes and pixels are untouched, so
    no wrap input moves and the fingerprints in `display/tests/glyph_tables.rs`
    are unchanged by construction, since they deliberately exclude `offset`.
    """

    def __init__(self) -> None:
        self.data = bytearray()
        self._offsets: dict[tuple[bytes, int, int], int] = {}

    def add(self, rows: list[int] | bytes | bytearray, width: int = 0, height: int = 0) -> int:
        """The offset `rows` lives at, appending it only if it is new."""
        key = (bytes(rows), width, height)
        offset = self._offsets.get(key)
        if offset is None:
            offset = len(self.data)
            self._offsets[key] = offset
            self.data.extend(rows)
        return offset
