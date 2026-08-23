from __future__ import annotations


import unittest
from needle.csvish import split_csv

class T(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(split_csv("a,b,c"), ["a","b","c"])
