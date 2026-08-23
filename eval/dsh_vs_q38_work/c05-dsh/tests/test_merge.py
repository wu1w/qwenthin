from __future__ import annotations


import unittest
from needle.merge import merge_intervals

class T(unittest.TestCase):
    def test_sorted(self):
        self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])

    # Regression: production passes unsorted input; the old contract relied on
    # callers pre-sorting, so unsorted input silently dropped intervals.
    def test_unsorted_production_case(self):
        self.assertEqual(merge_intervals([[8,10],[1,3],[2,6]]), [[1,6],[8,10]])

    def test_unsorted_no_overlap(self):
        self.assertEqual(merge_intervals([[5,6],[1,2],[3,4]]), [[1,2],[3,4],[5,6]])

    def test_unsorted_containment(self):
        self.assertEqual(merge_intervals([[4,5],[2,3],[1,10]]), [[1,10]])

    def test_unsorted_reversed(self):
        self.assertEqual(merge_intervals([[10,12],[1,3],[5,7]]), [[1,3],[5,7],[10,12]])

    def test_equal_starts(self):
        self.assertEqual(merge_intervals([[1,4],[1,2]]), [[1,4]])

    def test_touching_ends(self):
        self.assertEqual(merge_intervals([[3,5],[1,3]]), [[1,5]])

    def test_empty(self):
        self.assertEqual(merge_intervals([]), [])

    def test_single(self):
        self.assertEqual(merge_intervals([[2,4]]), [[2,4]])

    def test_does_not_mutate_input(self):
        intervals = [[8,10],[1,3],[2,6]]
        merge_intervals(intervals)
        self.assertEqual(intervals, [[8,10],[1,3],[2,6]])
