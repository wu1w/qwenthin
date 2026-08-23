from __future__ import annotations


from datetime import datetime

def parse_ts_v1(s: str) -> datetime:
    """已废弃，保留兼容。Z -> +00:00，返回 UTC aware datetime。"""
    s = s.replace("Z", "+00:00")
    return datetime.fromisoformat(s)

def parse_ts_v2(s: str) -> datetime:
    # Z 等价于 +00:00，不能剥掉：剥掉会变 naive，丢失时区。
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)

def parse_ts(s: str) -> datetime:
    return parse_ts_v2(s)
