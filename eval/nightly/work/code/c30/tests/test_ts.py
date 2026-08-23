from __future__ import annotations


import unittest
from needle.ts import parse_ts

class T(unittest.TestCase):
    def test_naive(self):
        d = parse_ts("2026-08-22T00:00:00")
        self.assertEqual(d.year, 2026)
