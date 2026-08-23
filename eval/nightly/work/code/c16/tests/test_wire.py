from __future__ import annotations


import unittest, struct
from needle.wire import put_u32

class T(unittest.TestCase):
    def test_small(self):
        self.assertEqual(len(put_u32(1)), 4)

    def test_big_endian(self):
        self.assertEqual(put_u32(1), b"\x00\x00\x00\x01")
        self.assertEqual(put_u32(0x01020304), b"\x01\x02\x03\x04")
