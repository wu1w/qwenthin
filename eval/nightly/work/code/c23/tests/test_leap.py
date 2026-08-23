from __future__ import annotations


import unittest
from needle.leap import is_leap

class T(unittest.TestCase):
    def test_2024(self):
        self.assertTrue(is_leap(2024))
        self.assertFalse(is_leap(2023))

    def test_century(self):
        self.assertFalse(is_leap(1900))
        self.assertTrue(is_leap(2000))
        self.assertFalse(is_leap(2100))
