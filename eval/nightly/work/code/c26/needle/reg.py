from __future__ import annotations


REGISTRY = {}

def parse(pairs):
    reg = {}
    for k, v in pairs:
        reg[k] = v
    return reg
