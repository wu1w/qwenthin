from __future__ import annotations


def sum_and_count(xs):
    """Return (sum, count). xs is any iterable, consumed only once."""
    total = 0
    count = 0
    for x in xs:
        total += x
        count += 1
    return (total, count)
