import unittest
from needle.slug import slugify

class T(unittest.TestCase):
    def test_basic(self):
        self.assertEqual(slugify("Hello World"), "hello-world")
    def test_punct_and_runs(self):
        self.assertEqual(slugify("  A -- B!! c  "), "a-b-c")
