from __future__ import annotations


def merge_intervals(intervals):
    """Merge overlapping [s,e]. Input need not be pre-sorted."""
    if not intervals:
        return []
    ordered = sorted(intervals, key=lambda iv: (iv[0], iv[1]))
    out = [list(ordered[0])]
    for s, e in ordered[1:]:
        if s <= out[-1][1]:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return out
