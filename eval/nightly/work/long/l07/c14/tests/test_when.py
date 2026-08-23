from __future__ import annotations


import unittest
from datetime import datetime
from needle.when import hours_between

class T(unittest.TestCase):
    def test_naive(self):
        a = datetime(2026, 1, 1, 0, 0, 0)
        b = datetime(2026, 1, 1, 6, 0, 0)
        self.assertEqual(hours_between(a, b), 6)

    def test_mixed_tz_raises(self):
        from datetime import timezone, timedelta
        a = datetime(2026, 1, 1, 0, 0, 0)  # naive
        b = datetime(2026, 1, 1, 6, 0, 0, tzinfo=timezone(timedelta(hours=0)))
        with self.assertRaises(TypeError):
            hours_between(a, b)
        # symmetric: aware first, naive second
        with self.assertRaises(TypeError):
            hours_between(b, a)
