from __future__ import annotations


import unittest
from needle.win import window_max

class T(unittest.TestCase):
    def test_k_too_big(self):
        self.assertEqual(window_max([1, 2], 5), [])

    def test_k_equals_n(self):
        self.assertEqual(window_max([3, 1, 4, 1, 5], 5), [5])

    def test_window_count(self):
        # n-k+1 windows
        self.assertEqual(len(window_max([1, 2, 3, 4, 5], 3)), 3)
