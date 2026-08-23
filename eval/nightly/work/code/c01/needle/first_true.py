from __future__ import annotations


def first_true(pred, n: int) -> int:
    """Smallest i in [0, n) with pred(i). pred is monotonic False* True*. Else n.

    hi = mid is required. mid itself may be the answer.
    """
    lo, hi = 0, n
    while lo < hi:
        mid = (lo + hi) // 2
        if pred(mid):
            hi = mid
        else:
            lo = mid + 1
    return lo
