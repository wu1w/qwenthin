from __future__ import annotations

import os

__all__ = ["safe_join", "PathTraversalError"]


class PathTraversalError(ValueError):
    """user_path 解析后逃逸出 root。"""


def safe_join(root, user_path):
    """把 user_path 拼到 root 下，保证结果一定在 root 内，否则抛 PathTraversalError。

    - 先 abspath（含 normpath）归一化，再比较，杜绝 a/../../ 这类回退；
    - 绝对路径、`..`、`.`、空段都会被折进根内或拒绝；
    - 用 commonpath 做前缀判定，避免 "/data-evil" 被 "/data" 前缀误放行。
    """
    base = os.path.abspath(root)
    candidate = os.path.abspath(os.path.join(base, user_path))
    if candidate != base and os.path.commonpath([base, candidate]) != base:
        raise PathTraversalError(
            "path escapes root: %r not under %r" % (user_path, base)
        )
    return candidate
