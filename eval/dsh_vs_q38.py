#!/usr/bin/env python3
"""Same weights: official DeepSeek Harness (dsh headless) vs q38 --print.

Coding = nightly puzzles. Math/phil = 测试集 v1.
dsh is launched from source (tsx) with DSH_HOME=eval/dsh_home.
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
OUT = HERE / "dsh_vs_q38.json"
WORK = HERE / "dsh_vs_q38_work"
DSH_REPO = Path("/Users/william/deepseek-harness")
DSH_HOME = HERE / "dsh_home"
DSH_BIN = DSH_REPO / "apps/cli/src/bin.ts"
TSX = DSH_REPO / "node_modules/tsx/dist/esm/index.mjs"
NODE = shutil.which("node") or "node"

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


def cwd_slug(cwd: Path) -> str:
    return "--" + re.sub(r"[^A-Za-z0-9]+", "-", str(cwd.resolve())).strip("-") + "--"


def newest_session(cwd: Path) -> Path | None:
    root = DSH_HOME / "sessions" / cwd_slug(cwd)
    if not root.exists():
        # pnpm --dir used to pin sessions to the dsh repo; also search all.
        cands = list((DSH_HOME / "sessions").glob("**/session.jsonl*"))
    else:
        cands = list(root.glob("**/session.jsonl*"))
    if not cands:
        return None
    return max(cands, key=lambda p: p.stat().st_mtime)


def parse_dsh_session(path: Path | None) -> dict:
    if path is None or not path.exists():
        return {"tools": [], "tool_count": 0, "text": "", "reasoning_chars": 0, "session": None}
    raw = path.read_bytes()
    if path.suffix == ".zstd" or path.name.endswith(".jsonl.zstd"):
        try:
            import zstandard as zstd  # type: ignore

            raw = zstd.ZstdDecompressor().decompress(raw)
        except Exception:
            return {
                "tools": [],
                "tool_count": 0,
                "text": "",
                "reasoning_chars": 0,
                "session": str(path),
                "parse_error": "zstd",
            }
    tools: list[str] = []
    texts: list[str] = []
    reasoning = 0
    types: list[str] = []
    turn_end = ""
    for line in raw.decode("utf-8", "replace").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = str(ev.get("type") or "")
        types.append(t)
        data = ev.get("data") if isinstance(ev.get("data"), dict) else ev
        if t == "tool/call":
            name = data.get("name") or (data.get("data") or {}).get("name")
            if name:
                tools.append(str(name))
        if t == "assistant/message":
            msg = data.get("message") or {}
            for block in msg.get("content") or []:
                if not isinstance(block, dict):
                    continue
                bt = block.get("type")
                if bt == "text" and block.get("text"):
                    texts.append(str(block["text"]))
                elif bt == "reasoning" and block.get("text"):
                    reasoning += len(str(block["text"]))
        if t == "turn/end":
            reason = data.get("reason") or {}
            if isinstance(reason, dict):
                turn_end = str(reason.get("kind") or reason)
            else:
                turn_end = str(reason)
    return {
        "tools": tools,
        "tool_count": len(tools),
        "text": next((x for x in reversed(texts) if x.strip()), texts[-1] if texts else ""),
        "reasoning_chars": reasoning,
        "session": str(path),
        "turn_end": turn_end,
        "event_types": sorted(set(types)),
    }


def run_dsh(prompt: str, cwd: Path, timeout: int) -> dict:
    cwd.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["DSH_HOME"] = str(DSH_HOME)
    env["DSH_PERMISSION_MODE"] = "danger-full-access"
    env["DSH_TELEMETRY_DISABLED"] = "1"
    env["TSX_TSCONFIG_PATH"] = str(DSH_REPO / "tsconfig.json")
    t0 = time.time()
    cmd = [
        NODE,
        "--import",
        str(TSX),
        str(DSH_BIN),
        "--profile",
        "headless",
        prompt,
    ]
    try:
        r = subprocess.run(
            cmd,
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
    parsed = parse_dsh_session(newest_session(cwd))
    text = (parsed.get("text") or "").strip() or (out or "").strip()
    return {
        "seconds": round(time.time() - t0, 1),
        "rc": rc,
        "timed_out": timed,
        "text": text,
        "stdout": (out or "")[-1500:],
        "stderr_tail": (err or "")[-2000:],
        "tools": parsed.get("tools") or [],
        "tool_count": parsed.get("tool_count") or 0,
        "reasoning_chars": parsed.get("reasoning_chars") or 0,
        "turn_end": parsed.get("turn_end") or "",
        "session": parsed.get("session"),
        "event_types": parsed.get("event_types") or [],
    }


def run_q38(prompt: str, cwd: Path, tag: str, timeout: int) -> dict:
    cwd.mkdir(parents=True, exist_ok=True)
    sid = f"dshcmp-{tag}-{int(time.time())}"
    r = q38_run(prompt, cwd, sid, timeout)
    st = turn_stats(sid)
    return {
        "seconds": r.get("seconds"),
        "rc": r.get("rc"),
        "timed_out": r.get("timed_out"),
        "text": st.get("text") or (r.get("stdout") or ""),
        "tools": st.get("tool_names") or [],
        "tool_count": st.get("tools") or 0,
        "think_chars": st.get("think_chars") or 0,
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
    shutil.copytree(src, dst, ignore=shutil.ignore_patterns(".q38", "__pycache__", ".pyc", ".dsh"))
    return dst


def main() -> None:
    if not TSX.exists():
        raise SystemExit(f"tsx missing at {TSX}; pnpm install in {DSH_REPO}")
    if not DSH_BIN.exists():
        raise SystemExit(f"dsh bin missing: {DSH_BIN}")
    pub = load_jsonl(BENCH / "tasks_public.jsonl")
    priv = load_jsonl(BENCH / "evaluator_private.jsonl")
    WORK.mkdir(parents=True, exist_ok=True)
    report: dict = {
        "started": datetime.now(timezone.utc).isoformat(),
        "q38": str(Q38),
        "dsh_repo": str(DSH_REPO),
        "dsh_home": str(DSH_HOME),
        "model": "Qwen3.8-27B-UD-Q8",
        "note": "Official dsh headless (full tool zoo, danger-full-access) vs q38 --print. Same endpoint/weights. Coding = nightly puzzles. Math/phil = 测试集 v1.",
        "tasks": {},
    }

    for pid in CODE_IDS:
        src = puzzles.materialize(WORK / "src", pid)
        p = puzzles.BY_ID[pid]
        prompt = p["prompt"]
        qws = clone_tree(src, WORK / f"{pid}-q38")
        dws = clone_tree(src, WORK / f"{pid}-dsh")
        q = run_q38(prompt, qws, pid, 300)
        qg = puzzles.grade(pid, qws)
        d = run_dsh(prompt, dws, 300)
        dg = puzzles.grade(pid, dws)
        report["tasks"][pid] = {
            "kind": p["kind"],
            "title": p["title"],
            "q38": {**q, "grade": qg, "tail": (q["text"] or "")[-400:]},
            "dsh": {**d, "grade": dg, "tail": (d["text"] or "")[-400:]},
        }
        print(
            f"{pid} q38 ok={qg.get('ok')} stop={q.get('stop')} tools={q.get('tool_count')} | "
            f"dsh ok={dg.get('ok')} tools={d.get('tool_count')} {d.get('seconds')}s names={d.get('tools')}",
            flush=True,
        )

    for tid in MATH_IDS + PHIL_IDS:
        row = pub[tid]
        extra = "\n不要调用工具。" if tid.startswith("M") or tid.startswith("P") else ""
        qws = WORK / f"{tid}-q38"
        dws = WORK / f"{tid}-dsh"
        for w in (qws, dws):
            w.mkdir(parents=True, exist_ok=True)
            (w / "README.md").write_text("empty workspace\n")
        q = run_q38(row["task_prompt"] + extra, qws, tid.lower(), 180)
        d = run_dsh(row["task_prompt"] + extra, dws, 180)
        expect = (priv.get(tid) or {}).get("answer")
        item = {
            "kind": row["suite"],
            "title": row["title"],
            "expect": expect,
            "q38": {
                **{k: q[k] for k in ("seconds", "tool_count", "tools", "watchdog", "stop", "think_chars")},
                "ok": bool(expect) and has_answer(q["text"], expect) if expect else None,
                "tail": (q["text"] or "")[-400:],
            },
            "dsh": {
                **{k: d[k] for k in ("seconds", "tool_count", "tools", "reasoning_chars", "turn_end", "rc")},
                "ok": bool(expect) and has_answer(d["text"], expect) if expect else None,
                "tail": (d["text"] or "")[-400:],
            },
        }
        report["tasks"][tid] = item
        print(
            f"{tid} expect={expect!r} q38={item['q38']['ok']} t={q['seconds']} | "
            f"dsh={item['dsh']['ok']} t={d['seconds']} tools={d['tool_count']}",
            flush=True,
        )

    report["finished"] = datetime.now(timezone.utc).isoformat()
    OUT.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print("wrote", OUT)


if __name__ == "__main__":
    main()
