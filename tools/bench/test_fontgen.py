#!/usr/bin/env python3
from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from bitmap_pool import BitmapPool  # noqa: E402
from font_pack_to_rust import rust_str  # noqa: E402


class TestBitmapPool(unittest.TestCase):
    def test_identical_bytes_different_dimensions_not_deduplicated(self) -> None:
        pool = BitmapPool()
        # 8 bytes: matching packed size for both 4x8 and 8x8 bitmaps (1 byte/row * 8 rows)
        rows = [0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00]
        offset1 = pool.add(rows, width=4, height=8)
        offset2 = pool.add(rows, width=8, height=8)
        offset3 = pool.add(rows, width=4, height=8)

        self.assertEqual(offset1, 0)
        self.assertEqual(offset2, 8)
        self.assertEqual(offset3, 0)
        self.assertEqual(len(pool.data), 16)


class TestRustStr(unittest.TestCase):
    """A pack's name lands in generated Rust, which has to compile."""

    def test_ascii_stays_readable(self) -> None:
        self.assertEqual(rust_str("Literata 14"), '"Literata 14"')

    def test_quotes_and_backslashes_are_escaped(self) -> None:
        self.assertEqual(rust_str('A "B" \\ C'), '"A \\"B\\" \\\\ C"')

    def test_non_ascii_uses_rust_unicode_escapes(self) -> None:
        # Python's unicode_escape wrote \xf3, which Rust refuses above 0x7F,
        # and bare four- and eight-digit \u and \U escapes, which Rust refuses.
        self.assertEqual(rust_str("Crimson Pr\u00f3"), '"Crimson Pr\\u{f3}"')
        self.assertEqual(rust_str("Gentium\u2019s"), '"Gentium\\u{2019}s"')
        self.assertEqual(rust_str("\u660e\u671d"), '"\\u{660e}\\u{671d}"')
        self.assertEqual(rust_str("Emoji \U0001f600"), '"Emoji \\u{1f600}"')

    def test_controls_are_escaped(self) -> None:
        self.assertEqual(rust_str("a\tb\nc\x01"), '"a\\tb\\nc\\u{1}"')


if __name__ == "__main__":
    unittest.main()
