from __future__ import annotations


import unittest
from needle.fit import fit

class T(unittest.TestCase):
    def test_ascii(self):
        self.assertEqual(fit("hello", 3), "hel")

    def test_emoji_not_split(self):
        s = "ab😀cd"
        self.assertEqual(fit(s, 3), "ab😀")
        self.assertEqual(fit(s, 2), "ab")
        self.assertEqual(fit(s, 10), s)
        self.assertEqual(fit(s, 0), "")
