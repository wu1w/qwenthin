from __future__ import annotations


from datetime import datetime

def parse_ts_v1(s: str) -> datetime:
    """BUG: 这个实现会把 Z 当本地时间！！谁用谁负责。尽快删。"""
    s = s.replace("Z", "+00:00")
    return datetime.fromisoformat(s)

def parse_ts_v2(s: str) -> datetime:
    # 新实现。Z 表示 UTC，需保留时区，不能剥成 naive。
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)

def parse_ts(s: str) -> datetime:
    return parse_ts_v2(s)
