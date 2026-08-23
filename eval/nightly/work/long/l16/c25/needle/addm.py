from __future__ import annotations

from decimal import Decimal


def add_money(a: str, b: str) -> str:
    result = Decimal(a) + Decimal(b)
    s = str(result)
    if "." not in s:
        s += ".0"
    return s
