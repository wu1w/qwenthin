from __future__ import annotations


def display_name(user: dict) -> str:
    # 只有 None（缺省或显式 None）才回落 guest；空串是合法匿名名，需保留。
    name = user.get("name")
    return "guest" if name is None else name
