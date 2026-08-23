from __future__ import annotations


def window_max(xs, k):
    n = len(xs)
    out = []
    for i in range(n - k + 1):
        out.append(max(xs[i:i+k]))
    return out
