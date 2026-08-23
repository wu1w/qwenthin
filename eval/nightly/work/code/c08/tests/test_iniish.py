from __future__ import annotations


import unittest
from needle.iniish import parse_map


class T(unittest.TestCase):
    def test_bool_true(self):
        self.assertEqual(parse_map("enabled: true"), {"enabled": True})

    def test_bool_false(self):
        self.assertEqual(parse_map("enabled: false"), {"enabled": False})

    def test_bool_on_off(self):
        self.assertEqual(
            parse_map("a: on\nb: off"),
            {"a": True, "b": False},
        )

    def test_country_no_is_not_false(self):
        # Regression: Norway's ISO code must not be coerced to False.
        self.assertEqual(parse_map("country: NO"), {"country": "NO"})

    def test_country_no_case_insensitive(self):
        self.assertEqual(parse_map("country: no"), {"country": "no"})

    def test_yes_stays_string(self):
        # YAML 1.1 words are no longer type-guessed.
        self.assertEqual(parse_map("confirm: yes"), {"confirm": "yes"})

    def test_mixed_routing_block(self):
        cfg = parse_map(
            "country: NO\nenabled: true\nflag: off\nname: Norway"
        )
        self.assertEqual(
            cfg,
            {"country": "NO", "enabled": True, "flag": False, "name": "Norway"},
        )

    def test_empty_and_comments(self):
        self.assertEqual(
            parse_map("# header\n\nkey: value"),
            {"key": "value"},
        )


if __name__ == "__main__":
    unittest.main()
