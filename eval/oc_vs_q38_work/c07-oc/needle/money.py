from __future__ import annotations


from decimal import Decimal, ROUND_HALF_UP

def cents(amount: str) -> int:
    """Parse dollars to integer cents, .5 rounds away from zero."""
    q = Decimal(amount).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)
    return int(q * 100)
