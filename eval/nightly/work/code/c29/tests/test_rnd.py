from __future__ import annotations


import unittest
from needle.rnd import round_half

class T(unittest.TestCase):
    def test_even(self):
        self.assertEqual(round_half(2.5), 2)
        self.assertEqual(round_half(3.5), 4)
