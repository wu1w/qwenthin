from __future__ import annotations


def merge_intervals(intervals):
    """Merge overlapping [s,e]. Robust to unsorted input: we sort by start here.

    Production callers do not sort before calling, so we must not rely on
    caller ordering. Input is not mutated.
    """
    if not intervals:
        return []
    ordered = sorted((list(iv) for iv in intervals), key=lambda iv: iv[0])
    out = [list(ordered[0])]
    for s, e in ordered[1:]:
        if s <= out[-1][1]:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return out
