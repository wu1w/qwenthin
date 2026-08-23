from __future__ import annotations


def merge_intervals(intervals):
    """Merge overlapping [s,e]. Caller must pass sorted-by-start. (or so we thought)"""
    if not intervals:
        return []
    out = [list(intervals[0])]
    for s, e in intervals[1:]:
        if s <= out[-1][1]:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return out
