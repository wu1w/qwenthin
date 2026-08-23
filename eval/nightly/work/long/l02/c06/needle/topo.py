from __future__ import annotations


def topo_sort(graph):
    """Iterative DFS reverse postorder.

    A node is marked *in-progress* (gray) before its neighbours are explored
    and moved to *seen* (black) afterwards.  Revisiting a gray node means the
    graph has a cycle, which is reported instead of recursing until the stack
    overflows.  The traversal order matches the original recursive
    implementation exactly, so acyclic graphs produce identical output.
    """
    nodes = set(graph)
    for vs in graph.values():
        nodes.update(vs)
    seen = set()
    on_path = set()
    out = []

    for start in sorted(nodes):
        if start in seen:
            continue
        on_path.add(start)
        stack = [(start, iter(graph.get(start, [])))]
        while stack:
            u, it = stack[-1]
            descended = False
            for v in it:
                if v in on_path:
                    raise ValueError(f"cycle detected at node {v!r}")
                if v not in seen:
                    on_path.add(v)
                    stack.append((v, iter(graph.get(v, []))))
                    descended = True
                    break
            if not descended:
                on_path.discard(u)
                seen.add(u)
                out.append(u)
                stack.pop()

    out.reverse()
    return out
