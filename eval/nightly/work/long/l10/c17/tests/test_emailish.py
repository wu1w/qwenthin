from __future__ import annotations


import unittest
from needle.emailish import is_email

class T(unittest.TestCase):
    def test_ok(self):
        self.assertTrue(is_email("a@b.com"))
        self.assertFalse(is_email("nope"))
