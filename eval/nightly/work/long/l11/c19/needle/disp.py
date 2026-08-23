from __future__ import annotations


def display_name(user: dict) -> str:
    # 空串是合法名，保留原样；只有 None（含缺省）回落到 guest。
    name = user.get("name")
    return name if name is not None else "guest"
