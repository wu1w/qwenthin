from __future__ import annotations


import unittest
from needle.path import shortest_path

class T(unittest.TestCase):
    def test_ok(self):
        g = {"a": [("b", 1.0), ("c", 4.0)], "b": [("c", 1.0)]}
        self.assertEqual(shortest_path(g, "a", "c"), 2.0)
    def test_neg(self):
        # 负权边支持：a -> b 直连 -1，走直连
        g = {"a": [("b", -1.0), ("c", 4.0)], "b": [("c", 1.0)]}
        self.assertEqual(shortest_path(g, "a", "c"), 0.0)
        # 负权边使原本更短的路径变长
        self.assertEqual(shortest_path({"a": [("b", -1.0)]}, "a", "b"), -1.0)
    def test_neg_cycle(self):
        g = {"a": [("b", 1.0)], "b": [("a", -3.0)], "c": [("a", 0.0)]}
        with self.assertRaises(ValueError):
            shortest_path(g, "a", "b")
    def test_unreachable(self):
        g = {"a": [("b", 1.0)], "x": [("y", -2.0)]}
        self.assertIsNone(shortest_path(g, "a", "y"))
