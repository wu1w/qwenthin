from __future__ import annotations

import unicodedata


# 规范化必须对称：register 与 exists 走同一个 _norm。
# 两处各修各的（只 strip、或只 NFC）都会留下失配路径：
#   - 只修 strip：NFD 的 "Cafe\u0301" 仍对不上 NFC 的 "Café"
#   - 只修 NFC：带首尾空格的名字 register 进去了、exists 查不到
# 所以 _norm = NFC + strip，两个入口都过它。

def _norm(name):
    return unicodedata.normalize("NFC", name).strip()

def register(store, name):
    store[_norm(name)] = True

def exists(store, name):
    return _norm(name) in store
