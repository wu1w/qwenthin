from __future__ import annotations


def is_leap(year: int) -> bool:
    # 格里高利：被 4 整除为闰年，但世纪年须被 400 整除才是。
    return year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)
