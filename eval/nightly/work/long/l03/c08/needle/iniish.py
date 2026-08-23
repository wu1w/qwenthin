from __future__ import annotations


def parse_map(text: str) -> dict:
    """Tiny YAML subset. Only the literals `true`/`false` become bools;
    everything else (including NO, yes, on, off) stays a string."""
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or ":" not in line:
            continue
        k, v = line.split(":", 1)
        v = v.strip()
        low = v.lower()
        if low == "true":
            out[k.strip()] = True
        elif low == "false":
            out[k.strip()] = False
        else:
            out[k.strip()] = v
    return out
