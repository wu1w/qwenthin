from __future__ import annotations


from decimal import Decimal, ROUND_HALF_EVEN

def cents(amount: str) -> int:
    """Parse dollars to integer cents, banker's rounding."""
    q = Decimal(amount).quantize(Decimal("0.01"), rounding=ROUND_HALF_EVEN)
    return int(q * 100)
