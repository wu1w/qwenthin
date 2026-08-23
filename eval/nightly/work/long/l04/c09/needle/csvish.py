from __future__ import annotations


def split_csv(line: str) -> list[str]:
    """Split a CSV line, respecting double-quoted fields."""
    fields: list[str] = []
    cur: list[str] = []
    in_quotes = False
    i = 0
    while i < len(line):
        ch = line[i]
        if in_quotes:
            if ch == '"':
                if i + 1 < len(line) and line[i + 1] == '"':
                    cur.append('"')
                    i += 1
                else:
                    in_quotes = False
            else:
                cur.append(ch)
        else:
            if ch == '"':
                in_quotes = True
            elif ch == ",":
                fields.append("".join(cur))
                cur = []
            else:
                cur.append(ch)
        i += 1
    fields.append("".join(cur))
    return fields
