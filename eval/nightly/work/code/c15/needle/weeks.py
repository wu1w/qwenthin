from __future__ import annotations


def week_number(dt):
    """ISO-8601 week number: the week containing the first Thursday is week 1."""
    return dt.isocalendar()[1]
