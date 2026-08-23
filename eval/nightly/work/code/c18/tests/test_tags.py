from __future__ import annotations


import unittest
from needle import tags

class T(unittest.TestCase):
    def test_flyweight(self):
        self.assertIs(tags.tag(), tags.EMPTY)
        self.assertIs(tags.tag(), tags.tag())
