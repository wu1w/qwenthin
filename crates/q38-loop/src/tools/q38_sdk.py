# Inlined by run_code. Harness sets `_Q38_ROOT` first.
import os
import signal
import subprocess
from pathlib import Path


def _root():
    return Path(_Q38_ROOT)


def _lexical_normalize(path):
    p = Path(path)
    if p.anchor:
        out = Path(p.anchor)
        rest = p.parts[1:]
    else:
        out = Path()
        rest = p.parts
    for part in rest:
        if part == "..":
            if p.anchor:
                if out != Path(p.anchor):
                    out = out.parent
            elif out.parts:
                out = out.parent
        elif part != ".":
            out = out / part
    return out


def _is_within(path, root):
    path = Path(path)
    root = Path(root)
    return path == root or root in path.parents


def _resolve(path):
    if not path:
        raise ValueError("Error: No `path` provided.")
    root = _root()
    raw = Path(path)
    joined = raw if raw.is_absolute() else root / raw
    normalized = _lexical_normalize(joined)
    # Follow existing symlink ancestors (strict=False) so an in-repo
    # symlink cannot smuggle a read/write past the workspace root.
    try:
        real = Path(normalized).resolve()
        root_real = Path(root).resolve()
    except OSError as e:
        raise ValueError(f"Error: cannot resolve `{path}`: {e}") from e
    if not _is_within(real, root_real):
        raise ValueError(f"Error: path `{path}` is outside the workspace.")
    return real


def read(path, offset=None, limit=None):
    text = _resolve(path).read_text()
    if offset is None and limit is None:
        return text
    lines = text.split("\n")
    start = max(int(offset or 1), 1)
    if start > len(lines):
        raise ValueError(
            f"Error: start_line {start} exceeds file length ({len(lines)} lines)."
        )
    chunk = lines[start - 1 :]
    if limit is not None:
        chunk = chunk[: int(limit)]
    return "\n".join(chunk)


def write(path, content):
    p = _resolve(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)
    return f"Wrote {len(content)} bytes to {path}."


def _normalize_newlines_with_boundaries(text):
    out = []
    boundaries = [0]
    i = 0
    while i < len(text):
        if text[i] == "\r":
            out.append("\n")
            i += 2 if i + 1 < len(text) and text[i + 1] == "\n" else 1
        else:
            out.append(text[i])
            i += 1
        boundaries.append(i)
    return "".join(out), boundaries


def _newline_style(text):
    crlf = text.count("\r\n")
    lf = text.count("\n") - crlf
    cr = text.count("\r") - crlf
    count, style = max((crlf, "\r\n"), (lf, "\n"), (cr, "\r"))
    return style if count else None


def edit(path, old, new):
    p = _resolve(path)
    with p.open("r", encoding="utf-8", newline="") as handle:
        text = handle.read()
    normalized = "\r" in old or "\n" in old
    if normalized:
        normalized_text, boundaries = _normalize_newlines_with_boundaries(text)
        normalized_old, _ = _normalize_newlines_with_boundaries(old)
        n = normalized_text.count(normalized_old)
    else:
        n = text.count(old)
    if n == 0:
        raise ValueError(f"Error: The text to replace was not found in {path}.")
    if n > 1:
        raise ValueError(
            f"Error: `old_string` matched {n} times in {path}; provide a longer, more unique `old_string` so the edit targets exactly one location."
        )
    if normalized:
        start = normalized_text.index(normalized_old)
        end = start + len(normalized_old)
        original_start, original_end = boundaries[start], boundaries[end]
        fragment = text[original_start:original_end]
        style = _newline_style(fragment) or _newline_style(text) or "\n"
        normalized_new, _ = _normalize_newlines_with_boundaries(new)
        replacement = normalized_new.replace("\n", style)
        updated = text[:original_start] + replacement + text[original_end:]
        preserved = fragment != old or replacement != new
    else:
        updated = text.replace(old, new, 1)
        preserved = False
    with p.open("w", encoding="utf-8", newline="") as handle:
        handle.write(updated)
    suffix = " (preserved file line endings)" if preserved else ""
    return f"Successfully replaced text in {path}{suffix}."


def bash(command):
    timeout = float(os.environ.get("Q38_BASH_TIMEOUT", "60"))
    extra = {}
    if os.name != "nt":
        extra["start_new_session"] = True
    p = subprocess.Popen(
        command,
        shell=True,
        cwd=str(_root()),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        **extra,
    )
    try:
        stdout, stderr = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        _kill_bash(p)
        stdout, stderr = p.communicate()
        out = stdout or ""
        if stderr:
            out = out + ("\n[stderr]\n" if out else "") + stderr
        raise RuntimeError(f"Command timed out after {timeout} seconds.\n{out}")
    out = stdout or ""
    if stderr:
        out = out + ("\n[stderr]\n" if out else "") + stderr
    if p.returncode != 0:
        raise RuntimeError(f"Command failed with exit code {p.returncode}.\n{out}")
    return out


def _kill_bash(p):
    if os.name != "nt":
        try:
            os.killpg(p.pid, signal.SIGKILL)
            return
        except OSError:
            pass
    p.kill()
