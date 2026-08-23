from __future__ import annotations


import unittest, struct
from needle.wire import put_u32

class T(unittest.TestCase):
    def test_small(self):
        self.assertEqual(len(put_u32(1)), 4)
