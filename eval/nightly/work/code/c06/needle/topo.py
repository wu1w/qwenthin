from __future__ import annotations


def topo_sort(graph):
    """Iterative DFS reverse postorder.

    Uses an explicit stack of (node, iterator) so deep DAGs no longer hit
    the interpreter recursion limit, and a `state` set of nodes currently on
    the stack so that cycles raise ValueError instead of recursing forever.
    """
    nodes = set(graph)
    for vs in graph.values():
        nodes.update(vs)

    WHITE, GRAY, BLACK = 0, 1, 2
    state = {n: WHITE for n in nodes}
    out = []

    for start in sorted(nodes):
        if state[start] != WHITE:
            continue
        state[start] = GRAY
        stack = [(start, iter(graph.get(start, [])))]
        while stack:
            u, it = stack[-1]
            descended = False
            for v in it:
                if state[v] == GRAY:
                    raise ValueError(
                        "cycle detected in graph (node %r revisited on stack)" % (v,))
                if state[v] == WHITE:
                    state[v] = GRAY
                    stack.append((v, iter(graph.get(v, []))))
                    descended = True
                    break
            if not descended:
                state[u] = BLACK
                out.append(u)
                stack.pop()

    out.reverse()
    return out
