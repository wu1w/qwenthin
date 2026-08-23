from __future__ import annotations


def parse_map(text: str) -> dict:
    """Tiny YAML subset.

    Only canonical booleans (true/false, on/off) become bools. We deliberately
    do NOT treat the YAML 1.1 words "yes"/"no" as booleans, because values such
    as the ISO country code "NO" (Norway) would otherwise be silently coerced to
    False.
    """
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or ":" not in line:
            continue
        k, v = line.split(":", 1)
        v = v.strip()
        low = v.lower()
        if low in ("true", "on"):
            out[k.strip()] = True
        elif low in ("false", "off"):
            out[k.strip()] = False
        else:
            out[k.strip()] = v
    return out
