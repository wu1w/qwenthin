from __future__ import annotations


import unittest
from needle.names import exists, register

class T(unittest.TestCase):
    def test_plain(self):
        s = {}
        register(s, "Ada")
        self.assertTrue(exists(s, "Ada"))

    def test_strip_symmetric(self):
        s = {}
        register(s, " Ada ")
        self.assertTrue(exists(s, "Ada"))
        self.assertTrue(exists(s, "Ada  "))
        register(s, "Bob ")
        self.assertTrue(exists(s, " Bob"))

    def test_nfc_symmetric(self):
        s = {}
        register(s, "Café")          # NFC
        self.assertTrue(exists(s, "Cafe\u0301"))  # NFD
        s2 = {}
        register(s2, "Cafe\u0301")   # NFD
        self.assertTrue(exists(s2, "Café"))       # NFC

    def test_strip_and_nfc_combined(self):
        s = {}
        register(s, "  Café  ")      # 带空格 + NFC
        self.assertTrue(exists(s, "Cafe\u0301"))  # NFD，无空格
