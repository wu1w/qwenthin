#!/usr/bin/env python3
"""Count JSONL stop reasons under ~/.q38-agent/sessions.

Repeat tags (budget:repeat*) are fire counts, not false-positive rates.
Legacy rows are reason=budget:repeat; new hops split stutter vs dump.

  python3 scripts/stop_reasons.py
  python3 scripts/stop_reasons.py /path/to/sessions
"""

from __future__ import annotations

import json
import os
import sys
from collections import Counter
from pathlib import Path


REPEAT_PREFIX = "budget:repeat"


def sessions_dir(argv: list[str]) -> Path:
    if len(argv) > 1:
        return Path(argv[1]).expanduser()
    return Path.home() / ".q38-agent" / "sessions"


def classify(reason: str) -> str:
    if reason == f"{REPEAT_PREFIX}:stutter":
        return "stutter"
    if reason == f"{REPEAT_PREFIX}:dump":
        return "dump"
    if reason == REPEAT_PREFIX:
        return "legacy"
    if reason.startswith(REPEAT_PREFIX):
        return "other-repeat"
    return ""


def scan(dir_path: Path) -> tuple[Counter[str], Counter[str], int, int, int]:
    reasons: Counter[str] = Counter()
    repeat: Counter[str] = Counter()
    files = 0
    bad = 0
    sessions_with_repeat = 0
    if not dir_path.is_dir():
        return reasons, repeat, files, bad, sessions_with_repeat
    for path in sorted(dir_path.glob("*.jsonl")):
        files += 1
        hit = False
        with path.open(encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    bad += 1
                    continue
                if row.get("type") != "stop":
                    continue
                reason = row.get("reason") or "stop"
                reasons[reason] += 1
                kind = classify(reason)
                if kind:
                    repeat[kind] += 1
                    hit = True
        if hit:
            sessions_with_repeat += 1
    return reasons, repeat, files, bad, sessions_with_repeat


def main() -> int:
    dir_path = sessions_dir(sys.argv)
    reasons, repeat, files, bad, sessions_hit = scan(dir_path)
    stops = sum(reasons.values())
    fires = sum(repeat.values())
    print(f"dir={dir_path}")
    print(f"jsonl={files}  stops={stops}  bad_lines={bad}")
    print(f"repeat_fires={fires}  sessions_with_repeat={sessions_hit}")
    if stops:
        print(f"repeat/stops={fires / stops:.3f}")
    print()
    print("repeat breakdown (fires, not precision):")
    for key in ("stutter", "dump", "legacy", "other-repeat"):
        print(f"  {key:14} {repeat[key]}")
    print()
    print("all stop reasons:")
    if not reasons:
        print("  (none)")
        return 0
    width = max(len(k) for k in reasons)
    for reason, n in reasons.most_common():
        print(f"  {reason:<{width}}  {n}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
