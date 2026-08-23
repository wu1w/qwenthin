from __future__ import annotations


_STORE = {}

def cache_put(key, val):
    _STORE[key] = val

def cache_get(key, default=None):
    return _STORE.get(key, default)
