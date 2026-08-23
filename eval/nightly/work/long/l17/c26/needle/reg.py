from __future__ import annotations


REGISTRY = {}

def parse(pairs):
    REGISTRY.clear()
    for k, v in pairs:
        REGISTRY[k] = v
    return dict(REGISTRY)
