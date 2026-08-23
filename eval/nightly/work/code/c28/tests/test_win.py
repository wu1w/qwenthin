from __future__ import annotations


import unittest
from needle.win import window_max

class T(unittest.TestCase):
    def test_k_too_big(self):
        self.assertEqual(window_max([1, 2], 5), [])
