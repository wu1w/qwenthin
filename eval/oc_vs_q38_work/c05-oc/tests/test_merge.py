from __future__ import annotations


import unittest
from needle.merge import merge_intervals

class T(unittest.TestCase):
    def test_sorted(self):
        self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])

    def test_unsorted(self):
        self.assertEqual(merge_intervals([[8,10],[1,3],[2,6]]), [[1,6],[8,10]])

    def test_unsorted_containment(self):
        self.assertEqual(merge_intervals([[2,9],[1,3],[4,5]]), [[1,9]])

    def test_unsorted_touching(self):
        self.assertEqual(merge_intervals([[3,4],[1,3]]), [[1,4]])

    def test_unsorted_no_overlap(self):
        self.assertEqual(merge_intervals([[5,6],[1,2],[3,4]]), [[1,2],[3,4],[5,6]])

    def test_empty_and_single(self):
        self.assertEqual(merge_intervals([]), [])
        self.assertEqual(merge_intervals([[4,4]]), [[4,4]])

    def test_does_not_mutate_input(self):
        intervals = [[8,10],[1,3],[2,6]]
        merge_intervals(intervals)
        self.assertEqual(intervals, [[8,10],[1,3],[2,6]])
