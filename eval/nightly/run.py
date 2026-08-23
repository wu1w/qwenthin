#!/usr/bin/env python3
"""Overnight eval: 30 code + 20 philosophy + 20 compact-stress longs + bare probes.

Resume-safe. State: eval/nightly/runs/state.json
Live log: eval/nightly/runs/live.log
"""
from __future__ import annotations

import json
import os
import re
import shutil
import sys
import time
import traceback
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from subprocess import run, TimeoutExpired

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import philosophy as phil
import puzzles
from longs import LONGS


def q38_binary() -> Path:
    override = os.environ.get("Q38_BIN")
    if override:
        return Path(override).expanduser()
    installed = shutil.which("q38")
    if installed:
        return Path(installed)
    suffix = ".exe" if os.name == "nt" else ""
    return HERE.parents[1] / "target" / "release" / f"q38{suffix}"


Q38 = q38_binary()
SESS = Path.home() / ".q38-agent" / "sessions"
CFG = Path.home() / ".q38-agent" / "config.toml"
RUNS = HERE / "runs"
WORK = HERE / "work"
STATE_PATH = RUNS / "state.json"
HOP_TIMEOUT = 240
CODE_TIMEOUT = 420
PHIL_TIMEOUT = 180
FINALE_TIMEOUT = 540


def log(msg: str) -> None:
    RUNS.mkdir(parents=True, exist_ok=True)
    line = f"{datetime.now().strftime('%H:%M:%S')} {msg}"
    print(line, flush=True)
    with (RUNS / "live.log").open("a") as f:
        f.write(line + "\n")


def load_state() -> dict:
    if STATE_PATH.exists():
        return json.loads(STATE_PATH.read_text())
    return {
        "started": datetime.now(timezone.utc).isoformat(),
        "phil": {},
        "code": {},
        "long": {},
        "bare": {},
        "errors": [],
    }


def save_state(st: dict) -> None:
    RUNS.mkdir(parents=True, exist_ok=True)
    tmp = STATE_PATH.with_suffix(".tmp")
    tmp.write_text(json.dumps(st, ensure_ascii=False, indent=2))
    tmp.replace(STATE_PATH)


def api_key() -> str:
    m = re.search(r'api_key\s*=\s*"([^"]+)"', CFG.read_text())
    if not m:
        raise RuntimeError("no api_key")
    return m.group(1)


def session_path(sid: str) -> Path:
    return SESS / f"{sid}.jsonl"


def events(sid: str) -> list[dict]:
    p = session_path(sid)
    if not p.exists():
        return []
    out = []
    for line in p.read_text(errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


def compact_n(sid: str) -> int:
    return sum(1 for e in events(sid) if e.get("type") == "session/compact")


def turn_stats(sid: str, since: int = 0) -> dict:
    ev = events(sid)[since:]
    tools = 0
    think = 0
    prompt = 0
    comp = 0
    cached = 0
    watchdog = False
    contents = []
    names = []
    for e in ev:
        t = e.get("type")
        if t == "assistant":
            think += len(e.get("reasoning") or "")
            prompt += int(e.get("prompt_tokens") or 0)
            comp += int(e.get("completion_tokens") or 0)
            cached += int(e.get("cached_tokens") or 0)
            contents.append(e.get("content") or "")
            for c in e.get("tool_calls") or []:
                tools += 1
                fn = c.get("function") or {}
                names.append(fn.get("name") or c.get("name") or "?")
        if t == "policy" and e.get("reason") == "watchdog":
            watchdog = True
        if t == "stop" and "watchdog" in str(e.get("reason", "")).lower():
            watchdog = True
    text = next((c for c in reversed(contents) if c.strip()), "")
    return {
        "tools": tools,
        "tool_names": names,
        "think_chars": think,
        "prompt_tokens": prompt,
        "completion_tokens": comp,
        "cached_tokens": cached,
        "watchdog": watchdog,
        "text": text,
        "n_events": len(ev),
        "steps": sum(1 for e in ev if e.get("type") == "assistant"),
    }


def q38_run(
    prompt: str,
    cwd: Path,
    session: str,
    timeout: int,
    window: str | None = None,
) -> dict:
    env = os.environ.copy()
    if window:
        env["Q38_WORKING_WINDOW"] = window
    cwd.mkdir(parents=True, exist_ok=True)
    t0 = time.time()
    try:
        r = run(
            [str(Q38), "--print", "--session", session, prompt],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
        )
        err = r.stderr or ""
        out = r.stdout or ""
        rc = r.returncode
        timed = False
    except TimeoutExpired as e:
        err = (e.stderr or b"").decode("utf-8", "replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
        out = (e.stdout or b"").decode("utf-8", "replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        rc = -1
        timed = True
    dt = time.time() - t0
    wd = "[watchdog]" in err or "budget:think" in err
    return {
        "seconds": round(dt, 1),
        "rc": rc,
        "timed_out": timed,
        "stdout": out[-8000:],
        "stderr_tail": err[-4000:],
        "watchdog_stderr": wd,
        "session": session,
    }


def bare_chat(messages: list[dict], max_tokens: int = 2048, thinking: bool = True) -> dict:
    key = api_key()
    body = {
        "model": "Qwen3.8-27B-UD-Q8",
        "messages": messages,
        "temperature": 1.0,
        "top_p": 0.95,
        "top_k": 20,
        "min_p": 0.0,
        "max_tokens": max_tokens,
        "chat_template_kwargs": {
            "enable_thinking": thinking,
            "reasoning_effort": "medium",
            "preserve_thinking": True,
        },
    }
    req = urllib.request.Request(
        "https://llm.ixiaotao.com/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {key}"},
    )
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=300) as r:
        resp = json.load(r)
    msg = resp["choices"][0]["message"]
    return {
        "seconds": round(time.time() - t0, 1),
        "think": msg.get("reasoning_content") or "",
        "content": msg.get("content") or "",
        "usage": resp.get("usage") or {},
    }


def run_philosophy(st: dict) -> None:
    ws = WORK / "phil"
    ws.mkdir(parents=True, exist_ok=True)
    (ws / "README.md").write_text("empty workspace for chat-like turns\n")
    for item in phil.PHILOSOPHY:
        pid = item["id"]
        if pid in st["phil"]:
            continue
        sid = f"nphil-{pid}"
        log(f"PHIL {pid} {item['title']}")
        n0 = len(events(sid))
        raw = q38_run(item["prompt"], ws, sid, PHIL_TIMEOUT)
        stats = turn_stats(sid, n0)
        rumin = phil.rumination(  # think is in jsonl, not stdout
            _last_reasoning(sid),
            stats["text"] or raw["stdout"],
        )
        wd = stats["watchdog"] or raw["watchdog_stderr"]
        ok, notes = phil.philosophy_ok(rumin, wd, stats["tools"], stats["steps"])
        st["phil"][pid] = {
            "title": item["title"],
            "ok": ok,
            "notes": notes,
            "rumin": rumin,
            "stats": {k: stats[k] for k in stats if k != "text"},
            "answer": (stats["text"] or raw["stdout"])[:1500],
            "raw": {k: raw[k] for k in ("seconds", "rc", "timed_out", "watchdog_stderr")},
        }
        save_state(st)


def _last_reasoning(sid: str) -> str:
    parts = []
    for e in events(sid):
        if e.get("type") == "assistant":
            parts.append(e.get("reasoning") or "")
    return "\n".join(parts)


def run_code(st: dict) -> None:
    for p in puzzles.PUZZLES:
        pid = p["id"]
        if pid in st["code"]:
            continue
        log(f"CODE {pid} {p['kind']} {p['title']}")
        ws = puzzles.materialize(WORK / "code", pid)
        sid = f"ncode-{pid}"
        n0 = len(events(sid))
        raw = q38_run(p["prompt"], ws, sid, CODE_TIMEOUT)
        stats = turn_stats(sid, n0)
        g = puzzles.grade(pid, ws)
        st["code"][pid] = {
            "title": p["title"],
            "kind": p["kind"],
            "grade": g,
            "stats": {k: stats[k] for k in stats if k != "text"},
            "answer": (stats["text"] or raw["stdout"])[:1200],
            "raw": {k: raw[k] for k in ("seconds", "rc", "timed_out", "watchdog_stderr")},
            "ok": g["ok"] and not raw["timed_out"],
        }
        save_state(st)


def run_long(st: dict) -> None:
    for spec in LONGS:
        lid = spec["id"]
        if lid in st["long"] and st["long"][lid].get("done"):
            continue
        pid = spec["finale_puzzle"]
        log(f"LONG {lid} domains={spec['domains']} finale={pid}")
        ws_path = WORK / "long" / lid
        prior = st["long"].get(lid, {})
        expected_ws = ws_path / pid
        ws = (
            expected_ws
            if prior.get("hops") and expected_ws.is_dir()
            else puzzles.materialize(ws_path, pid)
        )
        sid = f"nlong-{lid}"
        hops_run = list(prior.get("hops") or [])
        for hop_i, hop in enumerate(spec["hops"][len(hops_run):], start=len(hops_run)):
            log(f"  hop {hop_i} compact={compact_n(sid)} domain={hop['domain']}")
            n0 = len(events(sid))
            raw = q38_run(hop["prompt"], ws, sid, HOP_TIMEOUT)
            stats = turn_stats(sid, n0)
            answer = stats["text"] or raw["stdout"]
            recall_ok = all(term.lower() in answer.lower() for term in hop.get("expect", []))
            tool_ok = not hop.get("no_tools") or stats["tools"] == 0
            hops_run.append(
                {
                    "i": hop_i,
                    "domain": hop["domain"],
                    "compact_after": compact_n(sid),
                    "seconds": raw["seconds"],
                    "rc": raw["rc"],
                    "tools": stats["tools"],
                    "steps": stats["steps"],
                    "prompt_tokens": stats["prompt_tokens"],
                    "cached_tokens": stats["cached_tokens"],
                    "recall_ok": recall_ok,
                    "tool_ok": tool_ok,
                    "answer": answer[:600],
                    "timed_out": raw["timed_out"],
                }
            )
            save_state({**st, "long": {**st["long"], lid: {"hops": hops_run, "done": False}}})
            st["long"][lid] = {"hops": hops_run, "done": False}

        cn = compact_n(sid)
        log(f"  finale compact={cn} hops={len(hops_run)}")
        n0 = len(events(sid))
        raw = q38_run(spec["finale_prompt"], ws, sid, FINALE_TIMEOUT)
        stats = turn_stats(sid, n0)
        g = puzzles.grade(pid, ws)
        # constraint: tests/ expected-value edits on pre-existing tests
        tchg = g.get("tests_changed")
        trajectory_ok = all(not h["timed_out"] and h.get("rc") == 0 for h in hops_run)
        attention_ok = all(h.get("recall_ok", True) and h.get("tool_ok", True) for h in hops_run)
        st["long"][lid] = {
            "done": True,
            "domains": spec["domains"],
            "finale": pid,
            "compact": compact_n(sid),
            "compact_observed": compact_n(sid) > 0,
            "hops": hops_run,
            "grade": g,
            "tests_changed": tchg,
            "stats": {k: stats[k] for k in stats if k != "text"},
            "answer": (stats["text"] or raw["stdout"])[:1500],
            "raw": {k: raw[k] for k in ("seconds", "rc", "timed_out", "watchdog_stderr")},
            "trajectory_ok": trajectory_ok,
            "attention_ok": attention_ok,
            "peak_prompt_tokens": max((h.get("prompt_tokens", 0) for h in hops_run), default=0),
            "ok": g["ok"] and trajectory_ok and attention_ok and raw["rc"] == 0 and not raw["timed_out"],
        }
        save_state(st)


def run_bare(st: dict) -> None:
    """Bare model: all philosophy + false-code excerpts + 5 finale cold starts."""
    sys_p = "工作区助手。路径相对。只落盘交付。矛盾先说。\n"
    for item in phil.PHILOSOPHY:
        bid = f"b-{item['id']}"
        if bid in st["bare"]:
            continue
        log(f"BARE {item['id']}")
        try:
            r = bare_chat(
                [
                    {"role": "system", "content": sys_p},
                    {"role": "user", "content": item["prompt"]},
                ]
            )
            rumin = phil.rumination(r["think"], r["content"])
            ok, notes = phil.philosophy_ok(rumin, False, 0, 1)
            st["bare"][bid] = {
                "ok": ok,
                "notes": notes,
                "rumin": rumin,
                "seconds": r["seconds"],
                "answer": r["content"][:1500],
                "think_head": r["think"][:800],
            }
        except Exception as e:
            st["bare"][bid] = {"ok": False, "error": str(e)}
            st["errors"].append(f"bare {bid}: {e}")
        save_state(st)

    # false-bug code, file excerpt, plan-only like last eval
    fakes = [p for p in puzzles.PUZZLES if p["kind"] == "fake"][:6]
    for p in fakes:
        bid = f"b-{p['id']}"
        if bid in st["bare"]:
            continue
        log(f"BARE code {p['id']}")
        tgt = p["target"]
        body = p["files"].get(tgt, "")[:2500]
        user = (
            p["prompt"]
            + "\n\n你已经读过：\n"
            + tgt
            + ":\n```python\n"
            + body
            + "\n```\n先说你的行动计划（两三句），暂时不要写代码。"
        )
        try:
            r = bare_chat(
                [
                    {"role": "system", "content": sys_p + "Workspace:\n    /tmp/needle\n"},
                    {"role": "user", "content": user},
                ]
            )
            pushback = any(
                k in (r["think"] + r["content"])
                for k in ("矛盾", "设计", "故意", "不是 bug", "不要改", "规格", "测试即", "WONTFIX", "银行家", "IEEE")
            )
            st["bare"][bid] = {
                "seconds": r["seconds"],
                "pushback": pushback,
                "answer": r["content"][:1200],
                "think_head": r["think"][:800],
                "rumin": phil.rumination(r["think"], r["content"]),
            }
        except Exception as e:
            st["bare"][bid] = {"ok": False, "error": str(e)}
        save_state(st)

    # cold-start finales vs harness-after-compact
    for spec in LONGS[:5]:
        bid = f"b-cold-{spec['id']}"
        if bid in st["bare"]:
            continue
        p = puzzles.BY_ID[spec["finale_puzzle"]]
        log(f"BARE cold {spec['id']} {p['id']}")
        tgt = p["target"]
        user = spec["finale_prompt"] + "\n\n" + tgt + ":\n```python\n" + p["files"].get(tgt, "")[:2000] + "\n```"
        try:
            r = bare_chat(
                [
                    {"role": "system", "content": sys_p},
                    {"role": "user", "content": user},
                ]
            )
            st["bare"][bid] = {
                "seconds": r["seconds"],
                "answer": r["content"][:1500],
                "think_head": r["think"][:600],
            }
        except Exception as e:
            st["bare"][bid] = {"error": str(e)}
        save_state(st)


def summarize(st: dict) -> dict:
    def rate(d, key="ok"):
        vals = [v.get(key) for v in d.values() if isinstance(v, dict) and key in v]
        if not vals:
            return None
        return round(sum(bool(x) for x in vals) / len(vals), 3)

    return {
        "phil_n": len(st["phil"]),
        "phil_ok": rate(st["phil"]),
        "code_n": len(st["code"]),
        "code_ok": rate(st["code"]),
        "long_n": sum(1 for v in st["long"].values() if v.get("done")),
        "long_ok": rate({k: v for k, v in st["long"].items() if v.get("done")}),
        "bare_n": len(st["bare"]),
    }


def main() -> int:
    RUNS.mkdir(parents=True, exist_ok=True)
    WORK.mkdir(parents=True, exist_ok=True)
    if not Q38.exists():
        log(f"missing binary {Q38}")
        return 1
    only = set(sys.argv[1:])
    st = load_state()
    log(f"resume summary={summarize(st)} only={only or 'all'}")
    try:
        if not only or "phil" in only:
            run_philosophy(st)
        if not only or "code" in only:
            run_code(st)
        if not only or "long" in only:
            run_long(st)
        if not only or "bare" in only:
            run_bare(st)
    except KeyboardInterrupt:
        log("interrupted")
        save_state(st)
        return 130
    except Exception:
        st["errors"].append(traceback.format_exc())
        save_state(st)
        log("CRASH\n" + traceback.format_exc())
        return 1
    save_state(st)
    (RUNS / "summary.json").write_text(json.dumps(summarize(st), indent=2) + "\n")
    log(f"done {summarize(st)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
