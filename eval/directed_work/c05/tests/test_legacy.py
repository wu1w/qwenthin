from __future__ import annotations


import unittest
from needle.legacy import legacy_scale

class TestLegacy(unittest.TestCase):
    def test_scale(self):
        self.assertEqual(legacy_scale([1, 2, 3]), [1, 2, 3])
