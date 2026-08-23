from __future__ import annotations


import math
import unittest
from needle.bsearch import contains


class T(unittest.TestCase):
    def test_found(self):
        self.assertTrue(contains([1, 2, 3, 4], 1))
        self.assertTrue(contains([1, 2, 3, 4], 3))

    def test_not_found(self):
        self.assertFalse(contains([1, 2, 3, 4], 0))
        self.assertFalse(contains([1, 2, 3, 4], 5))
        self.assertFalse(contains([], 1))

    def test_livelock_regression(self):
        # Previously `lo = mid` (no +1) spun forever on these misses.
        self.assertFalse(contains([1, 5], 3))
        self.assertFalse(contains([7], 5))
        self.assertFalse(contains([8, 9], 1))

    def test_nan_target_is_not_found(self):
        # NaN must be "not found" and must terminate.
        self.assertFalse(contains([1.0, 2.0, float("nan"), 4.0], float("nan")))
        self.assertFalse(contains([float("nan"), 1.0, 2.0], float("nan")))
        self.assertFalse(contains([], float("nan")))

    def test_terminates_even_on_odd_input(self):
        # The loop must terminate (never livelock) and return a bool on any
        # input, including a NaN element (which makes the array unsorted, so we
        # only assert termination + a definite result, not a specific value).
        xs = [1.0, 2.0, float("nan"), 4.0, 5.0]
        for t in (3.0, 4.0, float("nan")):
            self.assertIsInstance(contains(xs, t), bool)

    def test_terminates_on_large_sorted(self):
        xs = list(range(1000))
        self.assertTrue(contains(xs, 999))
        self.assertFalse(contains(xs, 500.5))


if __name__ == "__main__":
    unittest.main()
