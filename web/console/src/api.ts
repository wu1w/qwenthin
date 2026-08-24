export async function api<T = unknown>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers);
  if (init?.body && !(init.body instanceof FormData) && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }
  const r = await fetch(`/api${path}`, { ...init, headers });
  if (!r.ok) throw new Error(await r.text());
  const ct = r.headers.get("content-type") || "";
  if (ct.includes("application/json")) return r.json() as Promise<T>;
  return undefined as T;
}

export async function rpc<T = Record<string, unknown>>(method: string, params?: unknown): Promise<T> {
  const j = await api<T & { ok?: boolean; error?: string }>("/rpc", {
    method: "POST",
    body: JSON.stringify({ method, params }),
  });
  if (j && typeof j === "object" && "ok" in j && (j as { ok?: boolean }).ok === false) {
    throw new Error((j as { error?: string }).error || method);
  }
  return j as T;
}

export function failMsg(e: unknown): string {
  const raw = e instanceof Error ? e.message : String(e);
  // 后端错误响应统一为 {"error": "..."}，解出来给人看；非 JSON 原样返回。
  try {
    const j = JSON.parse(raw) as { error?: unknown };
    if (j && typeof j.error === "string" && j.error) return j.error;
  } catch {
    /* not json */
  }
  return raw;
}

export type Usage = {
  prompt_tokens?: number;
  completion_tokens?: number;
  cached_tokens?: number;
  cache_prompt_tokens?: number;
  cached_reported?: boolean;
  hit_pct?: number | null;
  hit_rate?: number | null;
  first_hop_hit_rate?: number | null;
  first_hop_prompt_tokens?: number;
  first_hop_cached_tokens?: number;
  stuck_first_hops?: number;
  assistant_steps?: number;
  prefix_note?: string;
  last_prompt_tokens?: number;
  live_prompt_tokens?: number;
  compacts?: number;
  window?: number;
  session?: string;
};

export type Permit = { id: number; tool: string; preview: string } | null;

export type Clarify = {
  id: number;
  title: string;
  prompt: string;
  options: Array<{ id: string; label: string }>;
} | null;

export type Snap = {
  ok?: boolean;
  session?: string;
  title?: string;
  workspace?: string;
  model?: string;
  mode?: string;
  channel?: string;
  plan_mode?: boolean;
  clarify_mode?: boolean;
  approvals?: string;
  agent_scope?: "workspace" | "global";
  low_precision?: boolean;
  busy?: string;
  queued?: number;
  steered?: number;
  queue_preview?: string[];
  steer_preview?: string[];
  turn_in_flight?: boolean;
  window?: number;
  usage?: Usage;
  permit?: Permit;
  clarify?: Clarify;
  jobs?: number;
};

export type SessionEvent = {
  type: string;
  text?: string;
  content?: string;
  reasoning?: string;
  name?: string;
  output?: string;
  channel?: string;
  delta?: boolean;
  reset?: boolean;
  prompt_tokens?: number;
  completion_tokens?: number;
  cached_tokens?: number | null;
  decode_tok_s?: number | null;
  reason?: string;
  session_id?: string;
  tool_call_id?: string;
  blob?: string | null;
  original_chars?: number | null;
  tool_calls?: Array<{
    id?: string;
    function?: { name?: string; arguments?: string };
  }>;
  [k: string]: unknown;
};

export type Uploaded = {
  name: string;
  path: string;
  url: string;
  mime: string;
  kind: string;
  content_part: Record<string, unknown>;
};

export type SessionInfo = {
  id: string;
  title?: string;
  preview?: string;
  mode?: string;
  channel?: string;
  events?: number;
  mtime?: number;
};

export type ChannelField = {
  key: string;
  label: string;
  secret: boolean;
  hint?: string;
};

export type ChannelKind = {
  id: string;
  name: string;
  blurb: string;
  mark: string;
  color: string;
  qr: boolean;
  once: boolean;
  in_process: boolean;
  fields: ChannelField[];
};

export type ChannelEp = {
  id: string;
  kind: string;
  enabled: boolean;
  bind?: string;
  reply_url?: string;
  require_mention?: boolean;
  dm_policy?: string;
  group_policy?: string;
  secret_set?: boolean;
  bot_token_set?: boolean;
  bot_token?: string;
  creds_set?: string[];
  allow_from?: string[];
  deny_from?: string[];
  extra?: Record<string, unknown>;
  secret?: string;
  runtime?: { state?: string; detail?: string | null };
  _local?: boolean;
  _origId?: string;
};

export type CronJob = {
  id: string;
  name: string;
  interval_s: number;
  prompt: string;
  enabled: boolean;
  last_run?: number | null;
};

export type Heartbeat = {
  enabled: boolean;
  interval_s: number;
  prompt: string;
  last_run?: number | null;
};

export function connectEvents(
  onMsg: (msg: { method: string; params: unknown }) => void,
  onStatus?: (up: boolean) => void,
) {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  let ws: WebSocket | undefined;
  let closed = false;
  const open = () => {
    if (closed) return;
    ws = new WebSocket(`${proto}://${location.host}/api/events`);
    ws.onopen = () => onStatus?.(true);
    ws.onmessage = (ev) => {
      try {
        onMsg(JSON.parse(String(ev.data)));
      } catch {
        /* ignore */
      }
    };
    ws.onclose = () => {
      if (closed) return;
      onStatus?.(false);
      setTimeout(open, 1500);
    };
  };
  open();
  return () => {
    closed = true;
    ws?.close();
  };
}

export const NEW_CHAT = "新的聊天";

export function titleFromText(text: string): string {
  const lines = text.split(/\n/);
  const parts: string[] = [];
  let attached = "";
  for (const line of lines) {
    const l = line.trim();
    if (!l) continue;
    if (l.startsWith("[attached:")) {
      if (!attached) attached = l.replace(/^\[attached:\s*/, "").replace(/\]\s*$/, "").trim();
      continue;
    }
    parts.push(l);
  }
  let t = parts.join(" ").trim();
  if (t.startsWith("[heartbeat]")) t = t.slice("[heartbeat]".length).trim();
  else if (t.startsWith("[cron:")) {
    const i = t.indexOf("]");
    t = i >= 0 ? t.slice(i + 1).trim() : t;
  }
  if (t.startsWith("/")) return "";
  if (t) return clipChars(t, 32);
  if (attached) {
    const name = attached.split(/[/\\]/).pop() || attached;
    return clipChars(name, 32);
  }
  return "";
}

export function sessionName(s: { title?: string; preview?: string; text?: string }): string {
  const stored = (s.title || "").trim();
  if (stored) return stored;
  const raw = s.preview || s.text || "";
  return titleFromText(raw) || NEW_CHAT;
}

export function nameFromEvents(events: Array<{ type?: string; text?: string }>, stored?: string): string {
  if (stored && stored.trim()) return stored.trim();
  for (const e of events) {
    if (e.type !== "user" || !e.text) continue;
    const t = titleFromText(e.text);
    if (t) return t;
  }
  return NEW_CHAT;
}

function clipChars(s: string, n: number): string {
  const t = s.replace(/\s+/g, " ").trim();
  const chars = [...t];
  if (chars.length <= n) return t;
  return chars.slice(0, Math.max(0, n - 1)).join("") + "…";
}

export const SLASH: Array<[string, string]> = [
  ["/help", "命令列表"],
  ["/status", "会话状态 recap"],
  ["/context", "token 估算：system / tools[] / 对话"],
  ["/new", "新会话（可带标题）"],
  ["/resume", "恢复会话"],
  ["/sessions", "会话目录检索"],
  ["/compress", "抽取式压缩"],
  ["/undo", "撤销上一轮"],
  ["/retry", "重试上一轮"],
  ["/stop", "中止轮次"],
  ["/queue", "忙碌时排队到下轮"],
  ["/steer", "下一个工具结果后注入引导"],
  ["/think", "low | medium | xhigh | off"],
  ["/fast", "关思考"],
  ["/mode", "chat | agent | think | code"],
  ["/plan", "on / off / go"],
  ["/clarify", "on / off"],
  ["/approvals", "ask | auto | yolo"],
  ["/lossy", "低精度模式开关"],
  ["/model", "查看/切换模型"],
  ["/tools", "当前工具清单"],
  ["/skills", "技能目录"],
  ["/mcp", "已挂载 MCP 目录（带名则调用）"],
  ["/reload", "重读 config.toml（下一轮）"],
  ["/usage", "token 与缓存 recap"],
  ["/diff", "查看改动"],
  ["/cron", "定时任务（.q38/cron.json）"],
  ["/config", "配置投影"],
  ["/setup", "连接向导：探测端点并写配置"],
  ["/busy", "忙碌策略 interrupt | queue | steer"],
  ["/history", "最近轮次一览"],
  ["/clear", "清空当前对话"],
  ["/version", "版本信息"],
];
