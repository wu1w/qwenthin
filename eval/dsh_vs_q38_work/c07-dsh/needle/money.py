from __future__ import annotations


from decimal import Decimal, ROUND_HALF_UP

def cents(amount: str) -> int:
    """Parse dollars to integer cents, half-up rounding (.5 一律远离 0)."""
    q = Decimal(amount).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)
    return int(q * 100)
