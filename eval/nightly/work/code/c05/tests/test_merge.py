from __future__ import annotations


import unittest
from needle.merge import merge_intervals

class T(unittest.TestCase):
    def test_sorted(self):
        self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])

    def test_unsorted_regression(self):
        # Production input order: not sorted by start. This was failing in prod.
        self.assertEqual(merge_intervals([[8,10],[1,3],[2,6]]), [[1,6],[8,10]])

    def test_unsorted_gap_and_containment(self):
        self.assertEqual(
            merge_intervals([[10,12],[1,3],[2,6],[1,5]]),
            [[1,6], [10,12]],
        )

    def test_adjacent_merge(self):
        self.assertEqual(merge_intervals([[1,2],[2,3]]), [[1,3]])

    def test_single_and_empty(self):
        self.assertEqual(merge_intervals([[5,5]]), [[5,5]])
        self.assertEqual(merge_intervals([]), [])

    def test_does_not_mutate_input(self):
        data = [[8,10],[1,3],[2,6]]
        snapshot = [list(iv) for iv in data]
        merge_intervals(data)
        self.assertEqual(data, snapshot)
