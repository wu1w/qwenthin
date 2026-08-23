from __future__ import annotations


import unittest
from needle import cache

class T(unittest.TestCase):
    def test_put(self):
        cache.cache_put("a", 1)
        self.assertEqual(cache.cache_get("a"), 1)

    def test_distinct_keys_do_not_collide(self):
        # (1,), [1], "1" and 1 all used to collapse to the same string.
        cache.cache_put((1,), "tuple")
        cache.cache_put([1], "list")
        cache.cache_put("1", "str")
        cache.cache_put(1, "int")
        self.assertEqual(cache.cache_get((1,)), "tuple")
        self.assertEqual(cache.cache_get([1]), "list")
        self.assertEqual(cache.cache_get("1"), "str")
        self.assertEqual(cache.cache_get(1), "int")

    def test_unhashable_key_roundtrip(self):
        cache.cache_put([2, 3], "x")
        self.assertEqual(cache.cache_get([2, 3]), "x")
        # A structurally similar but differently-typed key stays separate.
        self.assertIsNone(cache.cache_get((2, 3)))
        self.assertEqual(cache.cache_get([2, 3], "dflt"), "x")

    def test_missing_returns_default(self):
        self.assertEqual(cache.cache_get("nope", "dflt"), "dflt")
        self.assertIsNone(cache.cache_get("nope"))
