from __future__ import annotations


import struct
def put_u32(n: int) -> bytes:
    """Pack unsigned 32-bit in network byte order (big-endian)."""
    return struct.pack(">I", n)
