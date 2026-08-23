from __future__ import annotations


def hours_between(a, b):
    """Absolute hours. Mixing a tz-aware and a naive datetime raises TypeError."""
    a_aware = getattr(a, "tzinfo", None) is not None
    b_aware = getattr(b, "tzinfo", None) is not None
    if a_aware != b_aware:
        raise TypeError(
            "can't subtract offset-naive and offset-aware datetimes"
        )
    return abs((a - b).total_seconds()) / 3600.0
