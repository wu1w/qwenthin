from __future__ import annotations


import re
_EMAIL = re.compile(r"^[a-zA-Z0-9]+@[a-zA-Z0-9]+\.[a-z]{2,}$")

def is_email(s: str) -> bool:
    return _EMAIL.match(s) is not None
