from __future__ import annotations


import unittest
from needle import cache

class T(unittest.TestCase):
    def test_put(self):
        cache.cache_put("a", 1)
        self.assertEqual(cache.cache_get("a"), 1)

    def test_int_and_str_keys_do_not_collide(self):
        cache.cache_put(1, "int")
        cache.cache_put("1", "str")
        self.assertEqual(cache.cache_get(1), "int")
        self.assertEqual(cache.cache_get("1"), "str")
