from __future__ import annotations


def fib(n, _cache={}):
    """nth Fibonacci. _cache is a process-lifetime memo. Do not pass it."""
    if n < 2:
        return n
    if n not in _cache:
        _cache[n] = fib(n - 1, _cache) + fib(n - 2, _cache)
    return _cache[n]
