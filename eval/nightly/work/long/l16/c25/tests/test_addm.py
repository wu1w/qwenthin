from __future__ import annotations


import unittest
from needle.addm import add_money

class T(unittest.TestCase):
    def test_int(self):
        self.assertEqual(add_money("1", "2"), "3.0")
