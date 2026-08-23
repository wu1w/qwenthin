#!/usr/bin/env python3
"""Does the no-tool think clip cost accuracy?

Three arms on 测试集 math items, which have exact answers in the private file:
  bare    — raw chat, thinking on (reference ceiling)
  agent   — q38 --print, prompt carries 不要调用工具 (clip_no_tool_think -> 256)
  locked  — q38 --print --think, same prompt (user_locked bypasses the clip)
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "nightly"))

from run import Q38, bare_chat, events, turn_stats  # noqa: E402

BENCH = HERE.parent / "测试集" / "outputs" / "qwen38_agent_benchmark_v1"
OUT = HERE / "think_clip_probe.json"
WORK = HERE / "clip_work"

IDS = ["M001", "M002", "M003", "M004", "M007", "M012", "M014", "M020"]
NO_TOOLS = "\n不要调用工具。"


def load(name: str) -> dict:
    return {
        json.loads(l)["id"]: json.loads(l)
        for l in (BENCH / name).read_text().splitlines()
        if l.strip()
    }


def norm(s: str) -> str:
    s = s.replace("−", "-").replace("＝", "=").replace("，", ",")
    s = re.sub(r"[\s$\\{}()*`]", "", s)
    return s.lower()


def has_answer(text: str, expect: str) -> bool:
    return norm(expect) in norm(text)


def tail_hit(text: str, expect: str, window: int = 400) -> bool:
    return has_answer(text[-window:], expect)


def wobbles(text: str) -> bool:
    """Visible self-correction: the model published a wrong answer first."""
    return any(k in text for k in ("等等", "修正", "更正", "让我重新", "有误", "wait"))


def run_q38(prompt: str, cwd: Path, session: str, extra: list[str], timeout: int) -> dict:
    cwd.mkdir(parents=True, exist_ok=True)
    (cwd / "README.md").write_text("empty workspace\n")
    t0 = time.time()
    try:
        r = subprocess.run(
            [str(Q38), "--print", "--session", session, *extra, prompt],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=os.environ.copy(),
        )
        out, err, rc, timed = r.stdout or "", r.stderr or "", r.returncode, False
    except subprocess.TimeoutExpired as e:
        dec = lambda b: b.decode("utf-8", "replace") if isinstance(b, bytes) else (b or "")
        out, err, rc, timed = dec(e.stdout), dec(e.stderr), -1, True
    st = turn_stats(session)
    think = "\n".join(
        e.get("reasoning") or "" for e in events(session) if e.get("type") == "assistant"
    )
    return {
        "seconds": round(time.time() - t0, 1),
        "rc": rc,
        "timed_out": timed,
        "text": st.get("text") or out,
        "tools": st.get("tools"),
        "think_chars": len(think),
        "watchdog": bool(st.get("watchdog")) or "[watchdog]" in err,
    }


ARMS = {"agent": [], "locked": ["--think=medium"]}


def grade(text: str, expect: str) -> dict:
    return {
        "ok": has_answer(text, expect),
        "tail_ok": tail_hit(text, expect),
        "wobble": wobbles(text),
    }


def main() -> None:
    only = [a for a in sys.argv[1:] if a in ("bare", *ARMS)]
    pub, priv = load("tasks_public.jsonl"), load("evaluator_private.jsonl")
    report = json.loads(OUT.read_text()) if only and OUT.exists() else {
        "arms": ["bare", *ARMS],
        "items": {},
    }
    report.setdefault("started", datetime.now(timezone.utc).isoformat())
    stamp = int(time.time())
    for tid in IDS:
        task, expect = pub[tid]["task_prompt"], priv[tid]["answer"]
        row = report["items"].setdefault(
            tid, {"title": pub[tid]["title"], "expect": expect, "arms": {}}
        )

        if not only or "bare" in only:
            b = bare_chat([{"role": "user", "content": task}], max_tokens=3072)
            if not (b.get("content") or "").strip():
                b2 = bare_chat(
                    [{"role": "user", "content": task}], max_tokens=3072, thinking=False
                )
                b2["think"] = b.get("think") or ""
                b = b2
            text = b.get("content") or ""
            row["arms"]["bare"] = {
                **grade(text, expect),
                "think_chars": len(b.get("think") or ""),
                "seconds": b.get("seconds"),
                "tail": text[-300:],
            }

        for arm, extra in ARMS.items():
            if only and arm not in only:
                continue
            r = run_q38(
                task + NO_TOOLS,
                WORK / f"{tid}-{arm}",
                f"clip-{tid}-{arm}-{stamp}",
                extra,
                240,
            )
            row["arms"][arm] = {
                **grade(r["text"], expect),
                "think_chars": r["think_chars"],
                "watchdog": r["watchdog"],
                "tools": r["tools"],
                "rc": r["rc"],
                "seconds": r["seconds"],
                "tail": r["text"][-300:],
            }

        done = {a: row["arms"][a]["tail_ok"] for a in ("bare", *ARMS) if a in row["arms"]}
        print(f"{tid} {expect!r} -> {done}", flush=True)

    report["tally"] = {
        arm: {
            "tail_ok": sum(1 for r in report["items"].values() if r["arms"][arm]["tail_ok"]),
            "anywhere": sum(1 for r in report["items"].values() if r["arms"][arm]["ok"]),
            "wobble": sum(1 for r in report["items"].values() if r["arms"][arm]["wobble"]),
            "think_chars": sum(r["arms"][arm]["think_chars"] for r in report["items"].values()),
        }
        for arm in ("bare", *ARMS)
        if all(arm in r["arms"] for r in report["items"].values())
    }
    report["n"] = len(IDS)
    report["finished"] = datetime.now(timezone.utc).isoformat()
    OUT.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps(report["tally"], ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
