from __future__ import annotations


import unittest
from needle.merge import merge_intervals

class T(unittest.TestCase):
    def test_sorted(self):
        self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])

    def test_unsorted_regression(self):
        # Production incident: unsorted input was merged incorrectly.
        self.assertEqual(
            merge_intervals([[8,10],[1,3],[2,6]]),
            [[1,6],[8,10]],
        )

    def test_unsorted_disjoint(self):
        self.assertEqual(
            merge_intervals([[5,7],[1,2]]),
            [[1,2],[5,7]],
        )

    def test_empty(self):
        self.assertEqual(merge_intervals([]), [])
