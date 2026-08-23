from __future__ import annotations


import unittest
from needle.ident import same_number

class T(unittest.TestCase):
    def test_big(self):
        self.assertTrue(same_number(10**9 + 7, 10**9 + 7))
        self.assertFalse(same_number(10**9, 10**9 + 1))
