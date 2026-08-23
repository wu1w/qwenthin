from __future__ import annotations


import unittest
from needle.first_true import first_true

class T(unittest.TestCase):
    def test_threshold(self):
        for t in range(9):
            self.assertEqual(first_true(lambda i, t=t: i >= t, 8), min(t, 8))
    def test_none(self):
        self.assertEqual(first_true(lambda i: False, 5), 5)
