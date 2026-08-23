from __future__ import annotations


import unittest
from datetime import date
from needle.weeks import week_number

class T(unittest.TestCase):
    def test_mid(self):
        self.assertEqual(week_number(date(2026, 6, 15)), int(date(2026,6,15).strftime("%U")))

    def test_iso_not_week_zero(self):
        # 2021-01-01 is ISO week 53 (of 2020), never week 0
        self.assertEqual(week_number(date(2021, 1, 1)), 53)

    def test_iso_matches_isocalendar(self):
        for d in (date(2021, 1, 1), date(2021, 1, 4), date(2016, 12, 31), date(2026, 6, 15)):
            self.assertEqual(week_number(d), d.isocalendar()[1])
