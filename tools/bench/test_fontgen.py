#!/usr/bin/env python3
from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from bitmap_pool import BitmapPool  # noqa: E402


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


if __name__ == "__main__":
    unittest.main()
