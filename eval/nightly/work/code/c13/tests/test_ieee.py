from __future__ import annotations


import unittest
from needle.ieee import is_zero

class T(unittest.TestCase):
    def test_signed_zero(self):
        self.assertTrue(is_zero(0.0))
        self.assertTrue(is_zero(-0.0))
