from __future__ import annotations


import os

def safe_join(root, user_path):
    """Join *root* and *user_path*, rejecting any path traversal that would
    land outside *root*.  Raises ValueError on escape."""
    root = os.path.abspath(root)
    # Strip a leading '/' so absolute user paths are treated as relative.
    if user_path.startswith("/"):
        user_path = user_path[1:]
    joined = os.path.normpath(os.path.join(root, user_path))
    # Verify the normalised path is still inside root (or IS root).
    if joined != root and not joined.startswith(root + os.sep):
        raise ValueError(
            f"path traversal detected: {user_path!r} escapes root {root!r}"
        )
    return joined
