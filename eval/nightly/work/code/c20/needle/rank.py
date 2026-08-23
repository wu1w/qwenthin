from __future__ import annotations


def sort_by_score(rows):
    """rows: list of (name, score). Stable by original order on ties."""
    return sorted(rows, key=lambda r: -r[1])
