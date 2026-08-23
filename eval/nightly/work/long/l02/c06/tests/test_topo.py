from __future__ import annotations


import unittest
from needle.topo import topo_sort

class T(unittest.TestCase):
    def test_chain(self):
        self.assertEqual(topo_sort({"a":["b"],"b":["c"],"c":[]}), ["a","b","c"])

    def test_diamond_dag(self):
        # a -> b, a -> c, b -> d, c -> d.  One of the valid orderings.
        order = topo_sort({"a": ["b", "c"], "b": ["d"], "c": ["d"], "d": []})
        self.assertEqual(order, ["a", "c", "b", "d"])

    def test_dag_with_isolated_node(self):
        # Node 'z' only appears as a dependency, no outgoing edges listed.
        order = topo_sort({"a": ["b"], "b": ["z"]})
        self.assertEqual(order, ["a", "b", "z"])

    def test_self_loop_raises(self):
        with self.assertRaises(ValueError):
            topo_sort({"a": ["a"]})

    def test_two_cycle_raises(self):
        with self.assertRaises(ValueError):
            topo_sort({"a": ["b"], "b": ["a"]})

    def test_larger_cycle_raises(self):
        with self.assertRaises(ValueError):
            topo_sort({"a": ["b"], "b": ["c"], "c": ["a"]})

    def test_cycle_does_not_hit_recursion_limit(self):
        # Long chain that loops back to the start: 0 -> 1 -> ... -> 5000 -> 0.
        n = 5000
        graph = {str(i): [str((i + 1) % n)] for i in range(n)}
        with self.assertRaises(ValueError):
            topo_sort(graph)
