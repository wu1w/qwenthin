from __future__ import annotations


import unittest
from needle.names import exists, register

class T(unittest.TestCase):
    def test_plain(self):
        s = {}
        register(s, "Ada")
        self.assertTrue(exists(s, "Ada"))

    def test_nfc_nfd_roundtrip(self):
        # NFC: é 是单码点 U+00E9；NFD: e + 组合重音 U+0301
        nfc = "Caf\u00e9"
        nfd = "Cafe\u0301"
        s = {}
        register(s, nfc)
        self.assertTrue(exists(s, nfd))
        s2 = {}
        register(s2, nfd)
        self.assertTrue(exists(s2, nfc))

    def test_different_names_do_not_collide(self):
        s = {}
        register(s, "Caf\u00e9")
        self.assertFalse(exists(s, "Cafe"))

    def test_strip_symmetric(self):
        # register 和 exists 行为一致：首尾空白不影响匹配
        s = {}
        register(s, "  Ada ")
        self.assertTrue(exists(s, "Ada"))
        s2 = {}
        register(s2, "Ada")
        self.assertTrue(exists(s2, "  Ada  "))

    def test_combined(self):
        s = {}
        register(s, "  Café ")
        self.assertTrue(exists(s, "Cafe\u0301"))
