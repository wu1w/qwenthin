from __future__ import annotations


import unittest
from needle.topo import topo_sort


class T(unittest.TestCase):
    def test_chain(self):
        self.assertEqual(topo_sort({"a":["b"],"b":["c"],"c":[]}), ["a","b","c"])

    def test_empty(self):
        self.assertEqual(topo_sort({}), [])

    def test_node_without_key(self):
        # a node only referenced by an edge (no key) is still ordered
        self.assertEqual(topo_sort({"a":["b","c"],"b":[]}), ["a","c","b"])

    def test_diamond(self):
        # a -> (b,c) -> d
        self.assertEqual(
            topo_sort({"a":["b","c"],"b":["d"],"c":["d"],"d":[]}),
            ["a","c","b","d"],
        )

    def test_dag_stays_a_valid_topo_order(self):
        graph = {"a":["b","c"],"b":["d"],"c":["d"],"d":[]}
        out = topo_sort(graph)
        pos = {v: i for i, v in enumerate(out)}
        for u, vs in graph.items():
            for v in vs:
                self.assertLess(pos[u], pos[v])

    def test_deep_dag_no_recursion_error(self):
        # A long chain used to hit the interpreter recursion limit.
        n = 50000
        graph = {i: [i + 1] for i in range(n)}
        graph[n] = []
        out = topo_sort(graph)
        self.assertEqual(out, list(range(n + 1)))

    def test_two_node_cycle_raises(self):
        with self.assertRaises(ValueError):
            topo_sort({"a":["b"],"b":["a"]})

    def test_self_loop_raises(self):
        with self.assertRaises(ValueError):
            topo_sort({"a":["a"]})

    def test_longer_cycle_raises(self):
        with self.assertRaises(ValueError):
            topo_sort({"a":["b"],"b":["c"],"c":["d"],"d":["a"]})


if __name__ == "__main__":
    unittest.main()
