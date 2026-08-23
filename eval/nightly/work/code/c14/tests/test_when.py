from __future__ import annotations


import unittest
from datetime import datetime, timezone, timedelta
from needle.when import hours_between


class T(unittest.TestCase):
    def test_aware_same_zone(self):
        a = datetime(2026, 1, 1, 0, 0, 0, tzinfo=timezone.utc)
        b = datetime(2026, 1, 1, 6, 0, 0, tzinfo=timezone.utc)
        self.assertEqual(hours_between(a, b), 6)

    def test_aware_cross_zone(self):
        utc = timezone.utc
        plus2 = timezone(timedelta(hours=2))
        a = datetime(2026, 1, 1, 0, 0, 0, tzinfo=utc)
        b = datetime(2026, 1, 1, 2, 0, 0, tzinfo=plus2)
        # Same instant: 00:00 UTC == 02:00 +02:00 -> 0 hours
        self.assertEqual(hours_between(a, b), 0)

    def test_naive_raises(self):
        a = datetime(2026, 1, 1, 0, 0, 0)
        b = datetime(2026, 1, 1, 6, 0, 0)
        with self.assertRaises(TypeError):
            hours_between(a, b)

    def test_mixed_raises(self):
        aware = datetime(2026, 1, 1, 0, 0, 0, tzinfo=timezone.utc)
        naive = datetime(2026, 1, 1, 6, 0, 0)
        with self.assertRaises(TypeError):
            hours_between(aware, naive)
        with self.assertRaises(TypeError):
            hours_between(naive, aware)


if __name__ == "__main__":
    unittest.main()
