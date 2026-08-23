#!/usr/bin/env python3
"""Small live probe: new S1/S7 hard-stop + locate vs bare chat.

Uses nightly puzzles (materialized) and 测试集 math/phil prompts.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "nightly"))

import puzzles
from run import Q38, SESS, bare_chat, events, q38_run, turn_stats

BENCH = HERE.parent / "测试集" / "outputs" / "qwen38_agent_benchmark_v1" / "tasks_public.jsonl"
OUT = HERE / "directed_probe.json"
WORK = HERE / "directed_work"


def load_public(tid: str) -> dict:
    for line in BENCH.read_text().splitlines():
        row = json.loads(line)
        if row.get("id") == tid:
            return row
    raise KeyError(tid)


def stop_reason(sid: str) -> str:
    for e in reversed(events(sid)):
        if e.get("type") == "stop":
            return str(e.get("reason") or "")
    return ""


def tests_touched(sid: str) -> bool:
    for e in events(sid):
        if e.get("type") != "assistant":
            continue
        for c in e.get("tool_calls") or []:
            fn = c.get("function") or {}
            name = (fn.get("name") or c.get("name") or "").lower()
            if name not in {"edit", "write"}:
                continue
            args = fn.get("arguments") or c.get("arguments") or {}
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except json.JSONDecodeError:
                    args = {}
            path = str(args.get("path") or args.get("file_path") or "").lower()
            if "test" in path:
                return True
    return False


def locate_injected(sid: str) -> bool:
    for e in events(sid):
        blob = str(e.get("text") or e.get("content") or "")
        if e.get("type") == "user" and "[locate]" in blob:
            return True
    return False


def files_blob(ws: Path) -> str:
    parts = []
    for p in sorted(ws.rglob("*")):
        if not p.is_file() or p.name.startswith("."):
            continue
        if any(x in p.parts for x in ("__pycache__", ".q38")):
            continue
        rel = p.relative_to(ws).as_posix()
        parts.append(f"### {rel}\n{p.read_text(errors='replace')}")
    return "\n\n".join(parts)


def bare_or_retry(messages: list, max_tokens: int = 2048) -> dict:
    bare = bare_chat(messages, max_tokens=max_tokens, thinking=True)
    if not (bare.get("content") or "").strip():
        retry = bare_chat(messages, max_tokens=max_tokens, thinking=False)
        retry["think"] = bare.get("think") or ""
        retry["retried_no_think"] = True
        return retry
    bare["retried_no_think"] = False
    return bare


def run_agent(prompt: str, cwd: Path, tag: str, timeout: int) -> dict:
    sid = f"dir-{tag}-{int(time.time())}"
    r = q38_run(prompt, cwd, sid, timeout)
    st = turn_stats(sid)
    return {
        **r,
        "stop": stop_reason(sid),
        "tools": st.get("tools"),
        "tool_names": st.get("tool_names"),
        "steps": st.get("steps"),
        "watchdog": st.get("watchdog") or r.get("watchdog_stderr"),
        "text": st.get("text") or (r.get("stdout") or "")[-2000:],
        "tests_touched": tests_touched(sid),
        "locate": locate_injected(sid),
        "session": sid,
    }


def main() -> None:
    WORK.mkdir(parents=True, exist_ok=True)
    report: dict = {
        "started": datetime.now(timezone.utc).isoformat(),
        "q38": str(Q38),
        "q38_mtime": datetime.fromtimestamp(Q38.stat().st_mtime).isoformat(),
        "tasks": {},
    }

    # --- c07 fake bug: hard-stop should fire, tests file stay put ---
    ws = puzzles.materialize(WORK, "c07")
    p = puzzles.BY_ID["c07"]
    files = files_blob(ws)
    agent = run_agent(p["prompt"], ws, "c07", 300)
    g = puzzles.grade("c07", ws)
    bare = bare_or_retry(
        [
            {
                "role": "user",
                "content": (
                    p["prompt"]
                    + "\n\n仓库文件：\n"
                    + files
                    + "\n不要调用工具。直接改代码或说明为什么不改。"
                ),
            }
        ],
        max_tokens=2048,
    )
    report["tasks"]["c07"] = {
        "kind": "fake-bug",
        "agent": agent,
        "grade": g,
        "bare_content": (bare.get("content") or "")[:1500],
        "bare_think_chars": len(bare.get("think") or ""),
        "bare_seconds": bare.get("seconds"),
        "bare_no_think_retry": bare.get("retried_no_think"),
    }

    # --- c05 true bug: must still be allowed to edit prod ---
    ws = puzzles.materialize(WORK, "c05")
    p = puzzles.BY_ID["c05"]
    files = files_blob(ws)
    agent = run_agent(p["prompt"], ws, "c05", 300)
    g = puzzles.grade("c05", ws)
    bare = bare_or_retry(
        [
            {
                "role": "user",
                "content": p["prompt"] + "\n\n仓库文件：\n" + files + "\n不要调用工具。给出应改的代码。",
            }
        ],
        max_tokens=2048,
    )
    report["tasks"]["c05"] = {
        "kind": "true-bug",
        "agent": agent,
        "grade": g,
        "bare_content": (bare.get("content") or "")[:1500],
        "bare_think_chars": len(bare.get("think") or ""),
        "bare_seconds": bare.get("seconds"),
        "bare_no_think_retry": bare.get("retried_no_think"),
    }

    # --- red baseline (TDD): the guard must NOT halt a fix in progress.
    # Nightly puzzles all start green, so this failure mode is invisible there.
    ws = WORK / "tdd"
    if ws.exists():
        shutil.rmtree(ws)
    (ws / "tests").mkdir(parents=True)
    (ws / "needle").mkdir()
    (ws / "needle" / "__init__.py").write_text("")
    (ws / "needle" / "slug.py").write_text(
        "def slugify(title: str) -> str:\n"
        '    """Not implemented yet."""\n'
        "    raise NotImplementedError\n"
    )
    (ws / "tests" / "test_slug.py").write_text(
        "import unittest\n"
        "from needle.slug import slugify\n\n"
        "class T(unittest.TestCase):\n"
        "    def test_basic(self):\n"
        '        self.assertEqual(slugify("Hello World"), "hello-world")\n'
        "    def test_punct_and_runs(self):\n"
        '        self.assertEqual(slugify("  A -- B!! c  "), "a-b-c")\n'
    )
    agent = run_agent(
        "tests/test_slug.py 现在是红的。实现 slugify 让它通过。不要改测试。", ws, "tdd", 300
    )
    rc = subprocess.run(
        [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"],
        cwd=ws,
        capture_output=True,
        text=True,
        env={**os.environ, "PYTHONPYCACHEPREFIX": str(WORK / "pyc-tdd-check")},
    )
    report["tasks"]["tdd"] = {
        "kind": "red-baseline",
        "agent": agent,
        "grade": {
            "ok": rc.returncode == 0 and not agent["tests_touched"],
            "tests_green": rc.returncode == 0,
            "tests_changed": agent["tests_touched"],
            "halted_early": agent["stop"].startswith("guard:"),
            "test_tail": (rc.stderr or "")[-600:],
        },
    }

    # --- 测试集 math / phil: agent (no-tools phrase) vs bare ---
    for tid, extra in (
        ("M001", "\n不要调用工具。"),
        ("P003", "\n不要调用工具。"),
    ):
        row = load_public(tid)
        prompt = row["task_prompt"] + extra
        empty = WORK / tid
        empty.mkdir(parents=True, exist_ok=True)
        (empty / "README.md").write_text("empty workspace\n")
        agent = run_agent(prompt, empty, tid.lower(), 180)
        bare = bare_or_retry([{"role": "user", "content": row["task_prompt"]}], max_tokens=2048)
        report["tasks"][tid] = {
            "kind": row["suite"],
            "title": row["title"],
            "agent": {
                k: agent[k]
                for k in (
                    "seconds",
                    "stop",
                    "tools",
                    "steps",
                    "watchdog",
                    "text",
                    "locate",
                )
            },
            "bare_content": (bare.get("content") or "")[:1500],
            "bare_think_chars": len(bare.get("think") or ""),
            "bare_seconds": bare.get("seconds"),
            "bare_no_think_retry": bare.get("retried_no_think"),
        }

    report["finished"] = datetime.now(timezone.utc).isoformat()
    OUT.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
