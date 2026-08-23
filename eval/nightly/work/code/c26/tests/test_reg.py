from __future__ import annotations


import unittest
from needle import reg

class T(unittest.TestCase):
    def test_one(self):
        reg.REGISTRY.clear()
        self.assertEqual(reg.parse([("a",1)]), {"a":1})

    def test_no_leak_between_parses(self):
        self.assertEqual(reg.parse([("a",1)]), {"a":1})
        self.assertEqual(reg.parse([("b",2)]), {"b":2})
