from __future__ import annotations

from decimal import Decimal, InvalidOperation


def add_money(a: str, b: str) -> str:
    """精确到分相加，避免浮点误差（如 '0.1'+'0.2'=='0.3'）。"""
    try:
        result = Decimal(a) + Decimal(b)
    except InvalidOperation as exc:
        raise ValueError(f"不是合法金额: {a!r}, {b!r}") from exc
    # 规范化：去掉末尾多余的 0，但保留至少一位小数（精确到分）
    if result == result.to_integral_value():
        return f"{result:.2f}"
    text = format(result, "f")
    if "." in text:
        int_part, frac = text.split(".", 1)
        frac = frac.rstrip("0") or "0"
        return f"{int_part}.{frac}"
    return f"{text}.0"
