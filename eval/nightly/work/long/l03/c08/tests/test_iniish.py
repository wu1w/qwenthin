from __future__ import annotations


import unittest
from needle.iniish import parse_map

class T(unittest.TestCase):
    def test_bool(self):
        self.assertEqual(parse_map("enabled: true"), {"enabled": True})

    def test_country_no_stays_string(self):
        self.assertEqual(parse_map("country: NO"), {"country": "NO"})

    def test_false_keyword(self):
        self.assertEqual(parse_map("flag: false"), {"flag": False})

    def test_yes_on_off_not_bool(self):
        self.assertEqual(parse_map("a: yes"), {"a": "yes"})
        self.assertEqual(parse_map("b: on"), {"b": "on"})
        self.assertEqual(parse_map("c: off"), {"c": "off"})
