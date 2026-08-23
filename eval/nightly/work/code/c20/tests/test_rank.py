from __future__ import annotations


import unittest
from needle.rank import sort_by_score

class T(unittest.TestCase):
    def test_stable(self):
        rows = [("a",1),("b",2),("c",1)]
        out = sort_by_score(rows)
        self.assertEqual([n for n,_ in out], ["b","a","c"])
