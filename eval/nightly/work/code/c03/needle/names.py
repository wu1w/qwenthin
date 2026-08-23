from __future__ import annotations

import unicodedata


def _canon(name: str) -> str:
    # 视觉上相同的名字（NFC/NFD 混用、首尾空白）归一为同一 key。
    return unicodedata.normalize("NFC", name).strip()


def register(store, name):
    store[_canon(name)] = True


def exists(store, name):
    return _canon(name) in store
