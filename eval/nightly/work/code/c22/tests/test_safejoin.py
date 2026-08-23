from __future__ import annotations

import unittest, os
from needle.safejoin import safe_join, PathTraversalError


class T(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(safe_join("/data", "a/b"), os.path.join("/data", "a", "b"))

    def test_dotsegments_collapse_in_root(self):
        self.assertEqual(safe_join("/data", "a/./b/../c"), os.path.join("/data", "a", "c"))

    def test_reported_escape_blocked(self):
        with self.assertRaises(PathTraversalError):
            safe_join("/data", "a/../../etc/passwd")

    def test_direct_parent_escape(self):
        with self.assertRaises(PathTraversalError):
            safe_join("/data", "../../etc")

    def test_absolute_user_path(self):
        with self.assertRaises(PathTraversalError):
            safe_join("/data", "/etc/passwd")

    def test_bare_dotdot(self):
        with self.assertRaises(PathTraversalError):
            safe_join("/data", "..")

    def test_prefix_siblink_not_allowed(self):
        # /data-evil 不能被 /data 前缀误判为内部
        with self.assertRaises(PathTraversalError):
            safe_join("/data", "../data-evil/x")

    def test_root_itself_allowed(self):
        self.assertEqual(safe_join("/data", "."), os.path.abspath("/data"))
        self.assertEqual(safe_join("/data", ""), os.path.abspath("/data"))


if __name__ == "__main__":
    unittest.main()
