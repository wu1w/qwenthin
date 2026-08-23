from __future__ import annotations


import unittest
from needle.ts import parse_ts

class T(unittest.TestCase):
    def test_naive(self):
        d = parse_ts("2026-08-22T00:00:00")
        self.assertEqual(d.year, 2026)

    def test_z_is_utc(self):
        from datetime import timezone
        d = parse_ts("2026-08-22T00:00:00Z")
        self.assertIsNotNone(d.tzinfo)
        self.assertEqual(d.utcoffset(), timezone.utc.utcoffset(None))
