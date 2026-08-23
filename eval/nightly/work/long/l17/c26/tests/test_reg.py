from __future__ import annotations


import unittest
from needle import reg

class T(unittest.TestCase):
    def test_one(self):
        reg.REGISTRY.clear()
        self.assertEqual(reg.parse([("a",1)]), {"a":1})

    def test_no_carryover(self):
        reg.REGISTRY.clear()
        self.assertEqual(reg.parse([("a",1),("b",2)]), {"a":1,"b":2})
        self.assertEqual(reg.parse([("c",3)]), {"c":3})
        self.assertEqual(reg.REGISTRY, {"c":3})
