from __future__ import annotations


import unittest
from needle.fit import fit

class T(unittest.TestCase):
    def test_ascii(self):
        self.assertEqual(fit("hello", 3), "hel")
