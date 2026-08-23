from __future__ import annotations


def contains(xs, t):
    """Binary search a sorted sequence for ``t``.

    NaN is treated as "not found": comparisons with NaN are always False, so a
    normal search would neither terminate reliably nor give a meaningful answer.
    We reject a NaN target up front and guarantee the loop always makes progress
    (``lo = mid + 1``), so it can never livelock.
    """
    if t != t:  # NaN target -> "not found"
        return False

    lo, hi = 0, len(xs)
    while lo < hi:
        mid = (lo + hi) // 2
        if xs[mid] == t:
            return True
        if xs[mid] < t:
            lo = mid + 1
        else:
            hi = mid
    return False
