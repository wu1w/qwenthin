from __future__ import annotations


def week_number(dt):
    """ISO-8601 week number (1..53; weeks start Monday, week 1 has the year's first Thursday)."""
    return dt.isocalendar()[1]
