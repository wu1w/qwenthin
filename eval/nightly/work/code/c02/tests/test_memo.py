from __future__ import annotations


import unittest
from needle.memo import fib

class T(unittest.TestCase):
    def test_val(self):
        self.assertEqual(fib(10), 55)
    def test_shared(self):
        fib(8)
        self.assertIn(8, fib.__defaults__[0])
        i = id(fib.__defaults__[0])
        fib(9)
        self.assertEqual(id(fib.__defaults__[0]), i)
