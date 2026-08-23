from __future__ import annotations


def merge_intervals(intervals):
    """Merge overlapping [s,e]. Input need not be sorted."""
    out = []
    for s, e in sorted(intervals, key=lambda p: p[0]):
        if out and s <= out[-1][1]:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return out
