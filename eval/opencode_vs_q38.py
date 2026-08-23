#!/usr/bin/env python3
"""Same weights, two harnesses: q38 (qwenthin) vs OpenCode.

Coding uses nightly puzzles (the only materializable trees).
Math/philosophy use 测试集/outputs/qwen38_agent_benchmark_v1 (exact answers).
Supplement coding fixtures are recipes, not trees — they are listed, not executed.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "nightly"))

import puzzles  # noqa: E402
from run import Q38, events, q38_run, turn_stats  # noqa: E402

BENCH = HERE.parent / "测试集" / "outputs" / "qwen38_agent_benchmark_v1"
OUT = HERE / "opencode_vs_q38.json"
WORK = HERE / "oc_vs_q38_work"
MODEL = "ixiaotao/Qwen3.8-27B-UD-Q8"
OPENCODE = shutil.which("opencode") or "opencode"

MATH_IDS = ["M001", "M002", "M003", "M004", "M007", "M014"]
CODE_IDS = ["c05", "c07"]
PHIL_IDS = ["P003"]


def load_jsonl(path: Path) -> dict:
    return {
        json.loads(l)["id"]: json.loads(l)
        for l in path.read_text().splitlines()
        if l.strip()
    }


def norm(s: str) -> str:
    s = s.replace("−", "-").replace("＝", "=").replace("，", ",")
    s = re.sub(r"[\s$\\{}()*`]", "", s)
    return s.lower()


def has_answer(text: str, expect: str) -> bool:
    return norm(expect) in norm(text)


def parse_opencode(stdout: str) -> dict:
    texts: list[str] = []
    tools: list[str] = []
    tokens: dict = {}
    types: list[str] = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = str(ev.get("type") or "")
        types.append(t)
        part = ev.get("part") or {}
        pt = str(part.get("type") or "")
        if t == "text" or pt == "text":
            blob = part.get("text") or ev.get("text") or ""
            if blob:
                texts.append(blob)
        name = (
            part.get("tool")
            or part.get("name")
            or (part.get("call") or {}).get("name")
            or (part.get("state") or {}).get("tool")
        )
        if name and ("tool" in t.lower() or "tool" in pt.lower() or t in {"tool_use", "tool-call"}):
            tools.append(str(name))
        if t in {"step_finish", "step-finish"} or pt in {"step-finish", "step_finish"}:
            tokens = part.get("tokens") or ev.get("tokens") or tokens
    return {
        "text": "".join(texts),
        "tools": tools,
        "tool_count": len(tools),
        "tokens": tokens,
        "event_types": sorted(set(types)),
    }


def run_opencode(prompt: str, cwd: Path, timeout: int) -> dict:
    cwd.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["OPENCODE_DISABLE_AUTOUPDATE"] = "1"
    t0 = time.time()
    try:
        r = subprocess.run(
            [
                OPENCODE,
                "run",
                "--dir",
                str(cwd),
                "--model",
                MODEL,
                "--format",
                "json",
                "--auto",
                "--",
                prompt,
            ],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
        )
        out, err, rc, timed = r.stdout or "", r.stderr or "", r.returncode, False
    except subprocess.TimeoutExpired as e:
        dec = lambda b: b.decode("utf-8", "replace") if isinstance(b, bytes) else (b or "")
        out, err, rc, timed = dec(e.stdout), dec(e.stderr), -1, True
    parsed = parse_opencode(out)
    return {
        "seconds": round(time.time() - t0, 1),
        "rc": rc,
        "timed_out": timed,
        "text": parsed["text"],
        "tools": parsed["tools"],
        "tool_count": parsed["tool_count"],
        "tokens": parsed["tokens"],
        "event_types": parsed["event_types"],
        "stderr_tail": (err or "")[-1500:],
        "stdout_chars": len(out),
    }


def run_q38(prompt: str, cwd: Path, tag: str, timeout: int) -> dict:
    cwd.mkdir(parents=True, exist_ok=True)
    sid = f"occmp-{tag}-{int(time.time())}"
    r = q38_run(prompt, cwd, sid, timeout)
    st = turn_stats(sid)
    return {
        "seconds": r.get("seconds"),
        "rc": r.get("rc"),
        "timed_out": r.get("timed_out"),
        "text": st.get("text") or (r.get("stdout") or ""),
        "tools": st.get("tool_names") or [],
        "tool_count": st.get("tools") or 0,
        "watchdog": st.get("watchdog") or r.get("watchdog_stderr"),
        "stop": next(
            (e.get("reason") for e in reversed(events(sid)) if e.get("type") == "stop"),
            "",
        ),
        "session": sid,
    }


def clone_tree(src: Path, dst: Path) -> Path:
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst, ignore=shutil.ignore_patterns(".q38", "__pycache__", ".pyc"))
    return dst


def main() -> None:
    if not Path(OPENCODE).exists() and not shutil.which("opencode"):
        raise SystemExit("opencode not on PATH")
    pub = load_jsonl(BENCH / "tasks_public.jsonl")
    priv = load_jsonl(BENCH / "evaluator_private.jsonl")
    WORK.mkdir(parents=True, exist_ok=True)
    report: dict = {
        "started": datetime.now(timezone.utc).isoformat(),
        "q38": str(Q38),
        "opencode": subprocess.check_output([OPENCODE, "--version"], text=True).strip(),
        "model": MODEL,
        "note": "Same endpoint/weights. Coding = nightly puzzles (materializable). Math/phil = 测试集 v1. Supplement coding fixtures are recipes and were not executed.",
        "tasks": {},
    }

    for pid in CODE_IDS:
        src = puzzles.materialize(WORK / "src", pid)
        p = puzzles.BY_ID[pid]
        prompt = p["prompt"]
        qws = clone_tree(src, WORK / f"{pid}-q38")
        ows = clone_tree(src, WORK / f"{pid}-oc")
        q = run_q38(prompt, qws, pid, 300)
        qg = puzzles.grade(pid, qws)
        o = run_opencode(prompt, ows, 300)
        og = puzzles.grade(pid, ows)
        report["tasks"][pid] = {
            "kind": p["kind"],
            "title": p["title"],
            "q38": {**q, "grade": qg, "tail": (q["text"] or "")[-400:]},
            "opencode": {**o, "grade": og, "tail": (o["text"] or "")[-400:]},
        }
        print(
            f"{pid} q38 ok={qg.get('ok')} stop={q.get('stop')} tools={q.get('tool_count')} | "
            f"oc ok={og.get('ok')} tools={o.get('tool_count')} {o.get('seconds')}s",
            flush=True,
        )

    for tid in MATH_IDS + PHIL_IDS:
        row = pub[tid]
        prompt = row["task_prompt"]
        extra = "\n不要调用工具。" if tid.startswith("M") or tid.startswith("P") else ""
        qws = WORK / f"{tid}-q38"
        ows = WORK / f"{tid}-oc"
        for w in (qws, ows):
            w.mkdir(parents=True, exist_ok=True)
            (w / "README.md").write_text("empty workspace\n")
        q = run_q38(row["task_prompt"] + extra, qws, tid.lower(), 180)
        o = run_opencode(row["task_prompt"] + extra, ows, 180)
        expect = (priv.get(tid) or {}).get("answer")
        item = {
            "kind": row["suite"],
            "title": row["title"],
            "expect": expect,
            "q38": {
                **{k: q[k] for k in ("seconds", "tool_count", "tools", "watchdog", "stop")},
                "ok": bool(expect) and has_answer(q["text"], expect) if expect else None,
                "tail": (q["text"] or "")[-400:],
            },
            "opencode": {
                **{k: o[k] for k in ("seconds", "tool_count", "tools", "tokens")},
                "ok": bool(expect) and has_answer(o["text"], expect) if expect else None,
                "tail": (o["text"] or "")[-400:],
            },
        }
        report["tasks"][tid] = item
        print(
            f"{tid} expect={expect!r} q38={item['q38']['ok']} t={q['seconds']} | "
            f"oc={item['opencode']['ok']} t={o['seconds']} tools={o['tool_count']}",
            flush=True,
        )

    report["finished"] = datetime.now(timezone.utc).isoformat()
    OUT.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print("wrote", OUT)


if __name__ == "__main__":
    main()
