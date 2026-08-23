from __future__ import annotations


def shortest_path(graph, src, dst):
    """Shortest path with possibly negative edge weights (Bellman-Ford).

    Returns the minimum distance from ``src`` to ``dst``, or ``None`` if
    ``dst`` is unreachable. Raises ``ValueError`` if a negative-weight cycle
    can reach ``dst`` (distance undefined) — including a negative self-loop.
    """
    nodes = set(graph.keys())
    for edges in graph.values():
        for v, _w in edges:
            nodes.add(v)
    nodes.add(src)

    INF = float("inf")
    dist = {u: INF for u in nodes}
    dist[src] = 0.0

    # Relax |V| - 1 times (no-op when the graph is empty).
    for _ in range(max(len(nodes) - 1, 0)):
        changed = False
        for u, edges in graph.items():
            du = dist[u]
            if du == INF:
                continue
            for v, w in edges:
                nd = du + w
                if nd < dist[v]:
                    dist[v] = nd
                    changed = True
        if not changed:
            break

    # One more pass detects a negative cycle reachable from src.
    for u, edges in graph.items():
        du = dist[u]
        if du == INF:
            continue
        for v, w in edges:
            if du + w < dist[v]:
                raise ValueError("negative cycle on path to destination")

    d = dist[dst]
    return None if d == INF else d
