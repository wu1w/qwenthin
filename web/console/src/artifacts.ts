import type { SessionEvent } from "./api";

const WRITE_TOOLS = new Set(["write", "edit"]);

const JUNK_DIR = [
  "/library/python/",
  "/site-packages/",
  "/dist-packages/",
  "/__pycache__/",
  "/node_modules/",
  "/.git/",
  "/target/debug/",
  "/target/release/",
  "/usr/lib/",
  "/opt/homebrew/lib/",
  "/.q38-sdk/",
  "/.q38/",
];

const SCRATCH_NAMES = new Set([
  "analysis",
  "analysis.md",
  "scratch.md",
  "dump.md",
  "tmp.md",
  "probe.txt",
]);

function norm(p: string) {
  return p.replace(/\\/g, "/").replace(/\/+/g, "/").trim();
}

function basename(p: string) {
  const n = norm(p).replace(/\/+$/, "");
  const i = n.lastIndexOf("/");
  return (i >= 0 ? n.slice(i + 1) : n).toLowerCase();
}

function parseArgs(raw: unknown): Record<string, unknown> {
  if (!raw) return {};
  if (typeof raw === "object" && !Array.isArray(raw)) {
    return raw as Record<string, unknown>;
  }
  if (typeof raw !== "string") return {};
  try {
    const v = JSON.parse(raw);
    if (v && typeof v === "object" && !Array.isArray(v)) return v as Record<string, unknown>;
  } catch {
    /* XML / truncated args */
  }
  return {};
}

function argPath(args: Record<string, unknown>): string | null {
  for (const k of ["path", "file_path", "new_path"]) {
    const v = args[k];
    if (typeof v === "string" && v.trim()) return v.trim();
  }
  return null;
}

function isJunkPath(p: string) {
  const n = `/${norm(p).toLowerCase()}/`;
  if (JUNK_DIR.some((d) => n.includes(d))) return true;
  const base = basename(p);
  if (base.endsWith(".log") || base === ".ds_store") return true;
  // unique_tmp is `{stem}.q38tmp.{pid}.{nanos}.{uuid}`, not `*.q38tmp`.
  if (base.includes(".q38tmp.") || base.endsWith(".q38tmp")) return true;
  if (base.startsWith(".q38")) return true;
  if (base.endsWith(".tmp") || base.endsWith(".swp") || base.endsWith("~")) return true;
  return p === "/dev/null" || base === "dev/null";
}

function isScratchName(p: string) {
  const base = basename(p);
  if (SCRATCH_NAMES.has(base)) return true;
  const n = norm(p).toLowerCase();
  return /(^|\/)notes\/harness_/.test(n) || /(^|\/)reports\/harness_/.test(n);
}

function inWorkspace(p: string, workspace?: string) {
  const n = norm(p);
  if (!n || n === "/dev/null") return false;
  const abs = n.startsWith("/") || /^[a-zA-Z]:\//.test(n);
  if (!abs) return !n.startsWith("../");
  if (!workspace) return true;
  const w = norm(workspace).replace(/\/$/, "");
  return n === w || n.startsWith(`${w}/`);
}

export function isDeliverablePath(p: string, workspace?: string) {
  const n = norm(p).replace(/^['"`]|['"`]$/g, "");
  if (!n || n.length > 512) return false;
  if (isJunkPath(n) || isScratchName(n)) return false;
  return inWorkspace(n, workspace);
}

function addPath(out: Set<string>, raw: string | null | undefined, workspace?: string) {
  if (!raw) return;
  const p = norm(raw.replace(/^['"`]|['"`]$/g, "").replace(/\.$/, ""));
  if (isDeliverablePath(p, workspace)) out.add(p);
}

function bashRedirects(cmd: string): string[] {
  const found: string[] = [];
  const re = /(?:^|[^\d])(?:>>?|tee(?:\s+-a)?)\s+['"]?([^\s'";|&]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(cmd))) {
    const p = m[1];
    if (p && p !== "/dev/null") found.push(p);
  }
  return found;
}

function announcedPaths(text: string): string[] {
  const found: string[] = [];
  const patterns = [
    /Wrote \d+ bytes to ([^\n]+?)\.?\s*$/gm,
    /Successfully replaced text in ([^\n]+?)\.?\s*$/gm,
    /(?:已保存|已写入|saved to|saved:|wrote to|written to)\s*[:：]?\s*`?([^\s`\n]+)/gi,
  ];
  for (const re of patterns) {
    let m: RegExpExecArray | null;
    const copy = new RegExp(re.source, re.flags);
    while ((m = copy.exec(text))) found.push(m[1]);
  }
  return found;
}

/** Last live user message — skip hidden `<tool_response>` / steer notes. */
export function lastLiveUserIndex(events: SessionEvent[]): number {
  let start = 0;
  for (let i = 0; i < events.length; i++) {
    const e = events[i];
    if (e.type !== "user") continue;
    const t = String(e.text || e.content || "").trim();
    if (!t || t.startsWith("<tool_response>") || t.startsWith("Steer:")) continue;
    start = i;
  }
  return start;
}

/** Write/edit/save paths for the current user turn — not read/find/stdlib hits. */
export function turnArtifacts(events: SessionEvent[], workspace?: string): string[] {
  const out = new Set<string>();
  const slice = events.slice(lastLiveUserIndex(events));
  for (const e of slice) {
    if (e.type === "assistant") {
      for (const c of e.tool_calls || []) {
        const name = (c.function?.name || "").toLowerCase();
        const args = parseArgs(c.function?.arguments);
        if (WRITE_TOOLS.has(name)) addPath(out, argPath(args), workspace);
        if (name === "bash" && typeof args.command === "string") {
          bashRedirects(args.command).forEach((p) => addPath(out, p, workspace));
        }
      }
    }
    if (e.type === "tool") {
      const name = (e.name || "").toLowerCase();
      const output = String(e.output || "");
      if (WRITE_TOOLS.has(name) || name === "bash") {
        announcedPaths(output).forEach((p) => addPath(out, p, workspace));
      }
    }
  }
  return [...out];
}
