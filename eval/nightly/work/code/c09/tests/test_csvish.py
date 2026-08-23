from __future__ import annotations


import unittest
from needle.csvish import split_csv

class T(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(split_csv("a,b,c"), ["a","b","c"])

    def test_quoted_comma(self):
        self.assertEqual(split_csv('a,"b,c",d'), ["a", "b,c", "d"])

    def test_escaped_quote(self):
        self.assertEqual(split_csv('a,"say ""hi""",b'), ["a", 'say "hi"', "b"])
