from __future__ import annotations


def merge_intervals(intervals):
    """Merge overlapping [s,e]. Accepts unsorted input: sorts by start internally.

    The documented contract used to require callers to pre-sort, but production
    callers pass unsorted lists, which silently dropped intervals. The function
    now sorts itself, so both sorted and unsorted input give correct results.
    """
    if not intervals:
        return []
    out = []
    for s, e in sorted(intervals):
        if out and s <= out[-1][1]:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return out
