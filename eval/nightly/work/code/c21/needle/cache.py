from __future__ import annotations


_STORE = {}


def _norm(key):
    """Return a hashable representation of ``key``.

    Hashable keys (str, int, tuple of hashables, ...) are used as-is so that
    distinct values like ``(1,)``, ``[1]`` and ``"1"`` no longer collide.
    Unhashable keys (lists, dicts, sets, ...) fall back to a canonical form
    that is stable for equal structures without conflating different types.
    """
    try:
        hash(key)
        return key
    except TypeError:
        return _canonical(key)


def _canonical(obj):
    if isinstance(obj, (list, tuple)):
        # Tag the container type so [1] and (1,) stay distinct.
        return ("__seq__", type(obj).__name__, tuple(_canonical(v) for v in obj))
    if isinstance(obj, dict):
        return ("__dict__", tuple(sorted((_canonical(k), _canonical(v)) for k, v in obj.items())))
    if isinstance(obj, (set, frozenset)):
        return ("__set__", type(obj).__name__, tuple(sorted(_canonical(v) for v in obj)))
    # Last resort: fall back to repr. Better than str() because it preserves
    # enough structure to distinguish most distinct values.
    return ("__repr__", repr(obj))


def cache_put(key, val):
    _STORE[_norm(key)] = val


def cache_get(key, default=None):
    return _STORE.get(_norm(key), default)
