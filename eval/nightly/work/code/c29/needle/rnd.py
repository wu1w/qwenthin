from __future__ import annotations

import math


def round_half(x: float) -> int:
    """小学四舍五入：.5 一律向远离零方向取整（1.5->2, 2.5->3, -2.5->-3）。"""
    return math.floor(x + 0.5) if x >= 0 else math.ceil(x - 0.5)
