from __future__ import annotations


import unittest
from needle.addm import add_money

class T(unittest.TestCase):
    def test_int(self):
        self.assertEqual(add_money("1", "2"), "3.00")

    def test_float_trap(self):
        # 浮点经典坑：'0.1' + '0.2' 必须精确等于 '0.3'
        self.assertEqual(add_money("0.1", "0.2"), "0.3")

    def test_decimal_precision(self):
        self.assertEqual(add_money("1.5", "2.25"), "3.75")

    def test_negative(self):
        self.assertEqual(add_money("-0.5", "0.3"), "-0.2")

    def test_sub_cent(self):
        # 允许超过分位的精度，不截断
        self.assertEqual(add_money("0.001", "0.002"), "0.003")

    def test_invalid(self):
        with self.assertRaises(ValueError):
            add_money("abc", "1")


if __name__ == "__main__":
    unittest.main()
