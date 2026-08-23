from __future__ import annotations


import unittest
from needle.gen import sum_and_count

class T(unittest.TestCase):
    def test_list(self):
        self.assertEqual(sum_and_count([1,2,3]), (6, 3))

    def test_generator(self):
        self.assertEqual(sum_and_count(i for i in range(1, 4)), (6, 3))
