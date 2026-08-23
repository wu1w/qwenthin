from __future__ import annotations


import unittest
from needle.merge import merge_intervals

class T(unittest.TestCase):
    def test_sorted(self):
        self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])

    def test_unsorted_input(self):
        # Regression: production passed unsorted intervals.
        self.assertEqual(merge_intervals([[8,10],[1,3],[2,6]]), [[1,6],[8,10]])

    def test_does_not_mutate_input(self):
        intervals = [[8,10],[1,3],[2,6]]
        expected = [iv[:] for iv in intervals]
        merge_intervals(intervals)
        self.assertEqual(intervals, expected)

    def test_empty(self):
        self.assertEqual(merge_intervals([]), [])
