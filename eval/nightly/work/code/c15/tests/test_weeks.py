from __future__ import annotations


import unittest
from datetime import date
from needle.weeks import week_number


class T(unittest.TestCase):
    def test_mid(self):
        # ISO-8601: 2026-06-15 is in ISO week 25.
        self.assertEqual(week_number(date(2026, 6, 15)), 25)

    def test_2020_is_a_53_week_year(self):
        # 2020 has 53 ISO weeks; Dec 31 (Thu) sits in ISO week 53, not week 1.
        self.assertEqual(week_number(date(2020, 12, 31)), 53)

    def test_jan1_first_iso_week(self):
        # 2020-01-01 (Wed) is in ISO week 1 of 2020.
        self.assertEqual(week_number(date(2020, 1, 1)), 1)


if __name__ == "__main__":
    unittest.main()
