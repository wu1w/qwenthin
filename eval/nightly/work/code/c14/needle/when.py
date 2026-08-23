from __future__ import annotations


def hours_between(a, b):
    """Absolute hours between two timezone-aware datetimes.

    Both arguments must carry tzinfo; mixing or passing naive datetimes
    raises TypeError instead of silently comparing local wall time.
    """
    if a.tzinfo is None or a.tzinfo.utcoffset(a) is None:
        raise TypeError("hours_between: a must be timezone-aware (has tzinfo)")
    if b.tzinfo is None or b.tzinfo.utcoffset(b) is None:
        raise TypeError("hours_between: b must be timezone-aware (has tzinfo)")
    return abs((a - b).total_seconds()) / 3600.0
