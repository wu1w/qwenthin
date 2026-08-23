from __future__ import annotations


def fit(s: str, n: int) -> str:
    """Truncate to n user-visible code points (Python len)."""
    return s[:n]
