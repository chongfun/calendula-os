#!/usr/bin/env python3
from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from fontgen_common import BitmapPool  # noqa: E402


class TestBitmapPool(unittest.TestCase):
    def test_identical_bytes_different_dimensions_not_deduplicated(self) -> None:
        pool = BitmapPool()
        # 4 bytes: could represent a 4x8 bitmap (width=4, height=8)
        # or an 8x4 bitmap (width=8, height=4) or 16x2, etc.
        rows = [0xFF, 0x00, 0xFF, 0x00]
        offset1 = pool.add(rows, width=4, height=8)
        offset2 = pool.add(rows, width=8, height=4)
        offset3 = pool.add(rows, width=4, height=8)

        self.assertEqual(offset1, 0)
        self.assertEqual(offset2, 4)
        self.assertEqual(offset3, 0)
        self.assertEqual(len(pool.data), 8)


if __name__ == "__main__":
    unittest.main()
