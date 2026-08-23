from __future__ import annotations


def fit(s: str, n: int) -> str:
    """Truncate to n Python code points."""
    if n < 0:
        raise ValueError("n must be non-negative")
    return s[:n]
