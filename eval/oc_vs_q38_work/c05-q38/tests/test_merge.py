from __future__ import annotations


import unittest
from needle.merge import merge_intervals

class T(unittest.TestCase):
    def test_sorted(self):
        self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])

    def test_unsorted_regression(self):
        # 生产事故输入：未排序时首元素不能锚定结果
        self.assertEqual(
            merge_intervals([[8, 10], [1, 3], [2, 6]]), [[1, 6], [8, 10]]
        )

    def test_unsorted_first_not_anchor(self):
        # 最大起点在最前，全部重叠
        self.assertEqual(merge_intervals([[5, 6], [1, 10], [2, 3]]), [[1, 10]])

    def test_unsorted_touching(self):
        # 端点相接也合并
        self.assertEqual(merge_intervals([[4, 7], [1, 4]]), [[1, 7]])

    def test_input_not_mutated(self):
        data = [[8, 10], [1, 3], [2, 6]]
        merge_intervals(data)
        self.assertEqual(data, [[8, 10], [1, 3], [2, 6]])

    def test_empty(self):
        self.assertEqual(merge_intervals([]), [])
