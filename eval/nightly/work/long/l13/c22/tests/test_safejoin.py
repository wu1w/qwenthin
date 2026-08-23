from __future__ import annotations


import unittest, os
from needle.safejoin import safe_join

class T(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(safe_join("/data", "a/b"), os.path.join("/data","a","b"))
