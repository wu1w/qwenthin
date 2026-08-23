from __future__ import annotations


import unittest
from needle.gen import sum_and_count

class T(unittest.TestCase):
    def test_list(self):
        self.assertEqual(sum_and_count([1,2,3]), (6, 3))
