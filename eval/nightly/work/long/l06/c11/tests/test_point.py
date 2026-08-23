from __future__ import annotations


import unittest
from needle.point import Point

class T(unittest.TestCase):
    def test_eq(self):
        self.assertEqual(Point(1,2), Point(1,2))

    def test_hash_and_dict_key(self):
        d = {Point(1, 2): "a"}
        d[Point(1, 2)] = "b"  # 相等点共用键
        self.assertEqual(len(d), 1)
        self.assertEqual(d[Point(1, 2)], "b")

