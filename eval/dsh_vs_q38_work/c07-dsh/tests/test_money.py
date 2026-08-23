from __future__ import annotations


import unittest
from needle.money import cents

class T(unittest.TestCase):
    def test_half_even(self):
        self.assertEqual(cents("1.005"), 100)
        self.assertEqual(cents("1.015"), 102)
