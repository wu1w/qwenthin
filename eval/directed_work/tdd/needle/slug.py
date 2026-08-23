import re


def slugify(title: str) -> str:
    """Convert a title to a URL-friendly slug."""
    s = title.lower()
    s = re.sub(r"[^a-z0-9]+", "-", s)
    return s.strip("-")
