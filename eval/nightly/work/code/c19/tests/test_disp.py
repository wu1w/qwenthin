from __future__ import annotations


import unittest
from needle.disp import display_name

class T(unittest.TestCase):
    def test_none(self):
        self.assertEqual(display_name({}), "guest")
        self.assertEqual(display_name({"name": None}), "guest")
        self.assertEqual(display_name({"name": "Ada"}), "Ada")

    def test_empty_string_is_name(self):
        self.assertEqual(display_name({"name": ""}), "")
