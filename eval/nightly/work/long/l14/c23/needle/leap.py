from __future__ import annotations


def is_leap(year: int) -> bool:
    # 格里高利历：能被 4 整除的年份是闰年，
    # 但能被 100 整除的年份不是，除非它也能被 400 整除。
    # 因此 1900 非闰年，2000 是闰年。
    if year % 400 == 0:
        return True
    if year % 100 == 0:
        return False
    return year % 4 == 0
