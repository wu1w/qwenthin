import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  failMsg,
  nameFromEvents,
  rpc,
  sessionName,
  SLASH,
  type Permit,
  type SessionEvent,
  type SessionInfo,
  type Snap,
  type Uploaded,
} from "./api";
import { turnArtifacts } from "./artifacts";
import { MdText } from "./md";
import { Empty, Icon, Overlay, uiConfirm } from "./ui";

/** IME confirm (Enter / 选词) must not send the message. keyCode 229 is the composition sentinel. */
function imeBusy(e: { nativeEvent: { isComposing?: boolean }; isComposing?: boolean; keyCode: number }) {
  return e.isComposing === true || e.nativeEvent.isComposing === true || e.keyCode === 229;
}

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.left = "-9999px";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      return ok;
    } catch {
      return false;
    }
  }
}

function fmtTokS(n: number): string {
  if (n >= 100) return `${Math.round(n)} tok/s`;
  if (n >= 10) return `${n.toFixed(1)} tok/s`;
  return `${n.toFixed(2)} tok/s`;
}

function toolBadge(name: string) {
  const n = name.toLowerCase();
  if (n === "read" || n === "view" || n === "recall" || n === "memory_search") return "read";
  if (n === "edit") return "edit";
  if (n === "write") return "write";
  return "bash";
}

function livePromptTokens(events: SessionEvent[], snap?: Snap): number {
  let last = 0;
  for (const e of events) {
    if (e.type === "assistant" && (e.prompt_tokens || 0) > 0) {
      last = e.prompt_tokens || 0;
    }
  }
  if (last > 0) return last;
  return snap?.usage?.live_prompt_tokens || snap?.usage?.last_prompt_tokens || 0;
}

function compactCount(events: SessionEvent[], snap?: Snap): number {
  const n = events.filter((e) => e.type === "session/compact").length;
  return n || snap?.usage?.compacts || 0;
}

export type RunPhase = "idle" | "waiting" | "thinking" | "writing" | "tool" | "permit" | "stopping";

export function runPhase(opts: {
  busy: boolean;
  aborting?: boolean;
  live: { think: string; content: string };
  events: SessionEvent[];
  permit?: Permit;
}): RunPhase {
  if (opts.permit) return "permit";
  if (opts.aborting) return "stopping";
  const liveOn = !!(opts.live.think || opts.live.content);
  if (!opts.busy && !liveOn) return "idle";
  if (opts.live.content) return "writing";
  if (opts.live.think) return "thinking";
  const last = opts.events[opts.events.length - 1];
  if (last?.type === "tool") return "tool";
  if (last?.type === "assistant" && (last.tool_calls?.length ?? 0) > 0) return "tool";
  if (opts.busy) return "waiting";
  return "idle";
}

export const PHASE_LABEL: Record<RunPhase, string> = {
  idle: "空闲",
  waiting: "等待模型",
  thinking: "思考中",
  writing: "生成中",
  tool: "调用工具",
  permit: "等待审批",
  stopping: "正在停止",
};

export function fmtElapsed(s: number) {
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const r = s % 60;
  return `${m}:${r.toString().padStart(2, "0")}`;
}

export function RunChip({
  phase,
  elapsed,
  queued,
  steered,
  onClick,
}: {
  phase: RunPhase;
  elapsed: number;
  queued: number;
  steered: number;
  onClick?: () => void;
}) {
  const running = phase !== "idle";
  const bits = [PHASE_LABEL[phase]];
  if (running && elapsed > 0) bits.push(fmtElapsed(elapsed));
  if (queued > 0) bits.push(`排队 ${queued}`);
  if (steered > 0) bits.push(`转向 ${steered}`);
  return (
    <button
      type="button"
      className={`run-chip${running ? " on" : ""} phase-${phase}`}
      onClick={onClick}
      aria-label={`模型状态 ${bits.join(" · ")}`}
    >
      <span className="run-dot" />
      <span>{bits.join(" · ")}</span>
    </button>
  );
}

export function ChatPage({
  snap,
  events,
  live,
  busy,
  permit,
  elapsed,
  detailsOpen,
  onToggleDetails,
  onReload,
}: {
  snap: Snap;
  events: SessionEvent[];
  live: { think: string; content: string };
  busy: boolean;
  permit: Permit;
  elapsed: number;
  detailsOpen: boolean;
  onToggleDetails: () => void;
  onReload: () => Promise<void>;
}) {
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [histOpen, setHistOpen] = useState(false);
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [text, setText] = useState("");
  const [atts, setAtts] = useState<Uploaded[]>([]);
  const [uploading, setUploading] = useState(0);
  const [slash, setSlash] = useState<typeof SLASH>([]);
  const [slashSel, setSlashSel] = useState(0);
  const [aborting, setAborting] = useState(false);
  const [err, setErr] = useState("");
  const [slashOut, setSlashOut] = useState("");
  const fileRef = useRef<HTMLInputElement>(null);
  const logRef = useRef<HTMLDivElement>(null);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const imeLock = useRef(false);
  const [showJump, setShowJump] = useState(false);
  const [openBlocks, setOpenBlocks] = useState<Set<string>>(() => new Set());
  const [openSteps, setOpenSteps] = useState<Set<string>>(() => new Set());
  const anchorRef = useRef<{ count: number; sess?: string }>({ count: 0, sess: undefined });

  const turns = useMemo(() => buildTurns(events), [events]);
  const lastUserKey = useMemo(() => {
    for (let i = turns.length - 1; i >= 0; i--) {
      if (turns[i].user !== undefined) return turns[i].key;
    }
    return null;
  }, [turns]);
  const userTurnCount = useMemo(() => turns.filter((t) => t.user !== undefined).length, [turns]);

  const toggleBlock = useCallback(
    (k: string) =>
      setOpenBlocks((s) => {
        const n = new Set(s);
        if (n.has(k)) n.delete(k);
        else n.add(k);
        return n;
      }),
    [],
  );
  const toggleStep = useCallback(
    (k: string) =>
      setOpenSteps((s) => {
        const n = new Set(s);
        if (n.has(k)) n.delete(k);
        else n.add(k);
        return n;
      }),
    [],
  );

  const updateJump = () => {
    const el = logRef.current;
    if (!el) return;
    setShowJump(el.scrollHeight - el.scrollTop - el.clientHeight > 240);
  };

  const refreshSessions = async () => {
    try {
      const j = await rpc<{ sessions?: SessionInfo[] }>("session.list", {});
      setSessions(j.sessions || []);
    } catch {
      /* list is best-effort */
    }
  };

  useEffect(() => {
    refreshSessions();
  }, [snap.session]);

  useEffect(() => {
    if (histOpen) void refreshSessions();
    else setPicked(new Set());
  }, [histOpen]);

  useEffect(() => {
    setPicked((cur) => {
      const live = new Set(sessions.map((s) => s.id));
      const next = new Set([...cur].filter((id) => live.has(id)));
      return next.size === cur.size ? cur : next;
    });
  }, [sessions]);

  useEffect(() => {
    const el = logRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      el.style.setProperty("--thread-h", `${el.clientHeight}px`);
      updateJump();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    updateJump();
  }, [events, live, busy]);

  useEffect(() => {
    setOpenBlocks(new Set());
    setOpenSteps(new Set());
    setSlashOut("");
  }, [snap.session]);

  /**
   * 新的用户消息锚到视口顶部，回复在下方生长；流式期间不追底。
   * 只在「用户轮数增加」或「切会话」时滚动——compact / undo 触发的
   * history.replace 会减少轮数，不应劫持滚动位置。
   */
  useEffect(() => {
    const el = logRef.current;
    if (!el) return;
    const sessChanged = anchorRef.current.sess !== snap.session;
    const grew = userTurnCount > anchorRef.current.count;
    anchorRef.current = { count: userTurnCount, sess: snap.session };
    if (!sessChanged && !grew) return;
    if (!lastUserKey) return;
    const target = el.querySelector<HTMLElement>(`[data-turn="${lastUserKey}"]`);
    if (!target) return;
    const top =
      target.getBoundingClientRect().top - el.getBoundingClientRect().top + el.scrollTop - 10;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    el.scrollTo({ top: Math.max(0, top), behavior: sessChanged || reduce ? "auto" : "smooth" });
  }, [userTurnCount, lastUserKey, snap.session]);

  useEffect(() => {
    if (!busy) setAborting(false);
  }, [busy]);

  /** 命令输出追加在底部，滚过去让用户看到。 */
  useEffect(() => {
    if (!slashOut) return;
    const el = logRef.current;
    if (!el) return;
    requestAnimationFrame(() => {
      const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      el.scrollTo({ top: el.scrollHeight, behavior: reduce ? "auto" : "smooth" });
    });
  }, [slashOut]);

  /** 历史抽屉 Esc 关闭。 */
  useEffect(() => {
    if (!histOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setHistOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [histOpen]);

  /** 输入框随内容长高（上限交给 CSS max-height）。 */
  useEffect(() => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    ta.style.height = `${Math.min(180, ta.scrollHeight)}px`;
  }, [text]);

  const upload = async (files: FileList | File[]) => {
    const fd = new FormData();
    [...files].forEach((f) => fd.append("file", f));
    setUploading((n) => n + 1);
    try {
      const r = await fetch("/api/upload", { method: "POST", body: fd });
      if (!r.ok) throw new Error(await r.text());
      const j = (await r.json()) as { files: Uploaded[] };
      setAtts((xs) => [...xs, ...(j.files || [])]);
      setErr("");
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setUploading((n) => Math.max(0, n - 1));
    }
  };

  const applySlash = (cmd: string) => {
    setText(cmd + " ");
    setSlash([]);
    taRef.current?.focus();
  };

  const togglePick = (id: string, on: boolean) => {
    setPicked((cur) => {
      const next = new Set(cur);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  };

  const deleteSessions = async (ids: string[]) => {
    if (ids.length === 0) return;
    if (busy && snap.session && ids.includes(snap.session)) {
      setErr("当前会话正在回复，先停止再删。");
      return;
    }
    const one = ids.length === 1 ? sessions.find((s) => s.id === ids[0]) : undefined;
    const label =
      ids.length === 1 ? `删除会话「${one ? sessionName(one) : ids[0]}」？` : `删除 ${ids.length} 个会话？`;
    if (!(await uiConfirm(label, "会话记录与标题将一并删除，无法恢复。", { danger: true, okLabel: "删除" }))) return;
    setErr("");
    try {
      await rpc("session.delete", ids.length === 1 ? { session: ids[0] } : { sessions: ids });
      setPicked(new Set());
      await refreshSessions();
      await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const send = async () => {
    const raw = text.trim();
    if (!raw && !atts.length) return;
    setErr("");
    if (raw.startsWith("/")) {
      try {
        const j = await rpc<{ text?: string }>("slash", { text: raw });
        setText("");
        setSlash([]);
        setSlashOut((j.text || "").trim());
        await onReload();
        await refreshSessions();
      } catch (e) {
        setErr(failMsg(e));
      }
      return;
    }
    const parts: unknown[] = [];
    const notes: string[] = [];
    for (const f of atts) {
      if (f.content_part && (f.kind === "image" || f.kind === "video" || f.kind === "audio")) {
        parts.push(f.content_part);
      } else notes.push(f.path);
    }
    let prompt = raw;
    if (notes.length) prompt = `${prompt ? prompt + "\n\n" : ""}${notes.map((p) => `[attached: ${p}]`).join("\n")}`;
    try {
      await rpc("turn.start", { prompt: prompt || " ", content_parts: parts });
      setText("");
      setAtts([]);
      setSlash([]);
      setSlashOut("");
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const stop = async () => {
    setAborting(true);
    setErr("");
    try {
      await rpc("turn.abort", {});
    } catch (e) {
      setErr(failMsg(e));
      setAborting(false);
    }
  };

  const steer = async () => {
    const raw = text.trim();
    if (!raw) return;
    setErr("");
    try {
      await rpc("turn.steer", { text: raw });
      setText("");
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const queue = async () => {
    const raw = text.trim();
    if (!raw) return;
    setErr("");
    try {
      await rpc("turn.queue", { prompt: raw });
      setText("");
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const arts = useMemo(() => turnArtifacts(events, snap.workspace), [events, snap.workspace]);
  const usage = snap.usage;
  const win = snap.window || 0;
  const used = livePromptTokens(events, snap);
  const rawPct = win ? Math.round((used / win) * 100) : 0;
  const pct = Math.min(100, Math.max(0, rawPct));
  const compacts = compactCount(events, snap);
  const hit =
    usage?.cached_reported && usage.hit_pct != null ? `${Number(usage.hit_pct).toFixed(1)}%` : "n/a";
  const queued = snap.queued ?? 0;
  const steered = snap.steered ?? 0;
  const policy = snap.busy || "interrupt";
  const phase = runPhase({ busy, aborting, live, events, permit });
  const waitPrefix =
    phase === "waiting" ? usage?.live_prompt_tokens || usage?.last_prompt_tokens || 0 : 0;
  const waitPrefixBit = waitPrefix > 0 ? ` · ${waitPrefix} tokens` : "";
  const callLabel =
    phase === "stopping" || phase === "permit"
      ? PHASE_LABEL[phase]
      : waitPrefix > 0
        ? `正在调用模型 · ${waitPrefix.toLocaleString()} tokens`
        : "正在调用模型";

  /** 把流式中的思考/正文合并进最后一轮：思考进轨迹块，正文在下方流式生长。 */
  const withLive = (blocks: Block[]): Block[] => {
    let out = blocks;
    if (live.think) {
      const step: Step = { kind: "think", text: live.think, live: !live.content };
      const last = out[out.length - 1];
      if (last && last.kind === "activity") {
        out = [...out.slice(0, -1), { kind: "activity", steps: [...last.steps, step] }];
      } else {
        out = [...out, { kind: "activity", steps: [step] }];
      }
    }
    if (live.content) {
      out = [...out, { kind: "text", text: live.content, live: true }];
    } else if (busy && !live.think) {
      const last = out[out.length - 1];
      if (!last || last.kind !== "activity") out = [...out, { kind: "activity", steps: [] }];
    }
    return out;
  };
  const draft = text.trim();
  const heading = nameFromEvents(events, snap.title);
  const approvals = snap.approvals || "ask";

  const newChat = async () => {
    setErr("");
    try {
      await rpc("session.new", {});
      await refreshSessions();
      await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  /** 审批模式就地轮换（ask → auto → yolo）。走 /api/config，会同步 PermitHub 并写盘。 */
  const cycleApprovals = async () => {
    const order = ["ask", "auto", "yolo"];
    const next = order[(order.indexOf(approvals) + 1) % order.length];
    setErr("");
    try {
      await api("/config", { method: "POST", body: JSON.stringify({ approvals: next }) });
      if (!busy) await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const togglePlan = async () => {
    setErr("");
    try {
      await rpc("slash", { text: snap.plan_mode ? "/plan off" : "/plan on" });
      if (!busy) await onReload();
    } catch (e) {
      setErr(failMsg(e));
    }
  };

  const sendLabel = !busy ? "发送" : policy === "queue" ? "排队" : policy === "steer" ? "转向" : "打断";
  const placeholder = !busy
    ? "给 Qwenthin 发消息…  / 唤起命令，粘贴图片走上传"
    : policy === "queue"
      ? "本轮结束后会跑这段话…"
      : policy === "steer"
        ? "下一次工具结果后注入这段引导…"
        : "发送将打断当前轮次并改跑这段话…";
  const policyHint =
    policy === "queue"
      ? "忙碌策略 queue：Enter 排到本轮之后"
      : policy === "steer"
        ? "忙碌策略 steer：Enter 在下一次工具后注入"
        : "忙碌策略 interrupt：Enter 打断本轮";

  return (
    <div className="page chat-page">
      <div className="chat-col">
        <div className="chat-top">
          <div className="chat-who">
            <strong className="ellipsis">{heading}</strong>
            {snap.session ? <span className="sid">{snap.session}</span> : null}
          </div>
          <span className="chip mono hide-narrow" title="会话模式，/mode 切换">{snap.mode || "agent"}</span>
          <span className="spacer" />
          <button className="btn ghost small" title="浏览与恢复历史会话" onClick={() => setHistOpen(true)}>
            <Icon name="clock" />
            历史
          </button>
          <button className="btn primary small" onClick={newChat}>
            <Icon name="plus" />
            新建聊天
          </button>
          <button
            type="button"
            className={`icon-btn${detailsOpen ? " on" : ""}`}
            title={detailsOpen ? "收起会话状态面板" : "展开会话状态面板"}
            aria-label="会话状态面板"
            aria-pressed={detailsOpen}
            onClick={onToggleDetails}
          >
            <Icon name="panel" />
          </button>
        </div>
        <div className="thread" ref={logRef} onScroll={updateJump}>
          <div className="thread-inner">
            {turns.length === 0 && !busy && !live.content && !live.think ? (
              <Empty title="开始对话" body="在下方输入。Enter 发送，Shift+Enter 换行，/ 唤起命令。" />
            ) : null}
            {turns.map((t, ti) => {
              const isLast = ti === turns.length - 1;
              return (
                <TurnView
                  key={t.key}
                  turnKey={t.key}
                  user={t.user}
                  blocks={isLast ? withLive(t.blocks) : t.blocks}
                  active={busy && isLast}
                  decodeTokS={busy && isLast ? null : t.decodeTokS}
                  callLabel={isLast ? callLabel : ""}
                  elapsed={isLast ? elapsed : 0}
                  openBlocks={openBlocks}
                  openSteps={openSteps}
                  onToggleBlock={toggleBlock}
                  onToggleStep={toggleStep}
                />
              );
            })}
            {slashOut ? (
              <div className="msg system">
                <div className="meta">
                  <span>命令</span>
                </div>
                <div className="bubble">{slashOut}</div>
              </div>
            ) : null}
          </div>
        </div>
        <div className="jump-anchor">
          <button
            type="button"
            className={`jump-bottom${showJump ? " on" : ""}`}
            aria-label="滚到最新"
            title="滚到最新"
            onClick={() => {
              const el = logRef.current;
              if (!el) return;
              const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
              el.scrollTo({ top: el.scrollHeight, behavior: reduce ? "auto" : "smooth" });
            }}
          >
            <Icon name="chev-d" />
          </button>
        </div>
        <div className="composer-wrap">
          <div className="composer-inner">
            {err ? <div className="err" style={{ margin: "0 0 8px" }}>{err}</div> : null}
            {busy ? (
              <div className="runbar" role="status">
                <span className={`run-dot lg phase-${phase}`} />
                <div className="run-copy">
                  <b>{PHASE_LABEL[phase]}</b>
                  <span>
                    {snap.model || "model"}
                    {elapsed > 0 ? ` · ${fmtElapsed(elapsed)}` : ""}
                    {waitPrefixBit}
                    {policy !== "interrupt" ? ` · busy ${policy}` : ""}
                  </span>
                </div>
                {queued > 0 ? (
                  <span className="run-pill on" title={(snap.queue_preview || []).join("\n")}>
                    排队 {queued}
                  </span>
                ) : (
                  <span className="run-pill idle">排队 0</span>
                )}
                {steered > 0 ? (
                  <span className="run-pill on" title={(snap.steer_preview || []).join("\n")}>
                    转向 {steered}
                  </span>
                ) : (
                  <span className="run-pill idle">转向 0</span>
                )}
                <div className="run-actions">
                  <button type="button" className="btn danger small" onClick={stop} disabled={aborting} title="中止本轮">
                    <Icon name="stop" />
                    停止
                  </button>
                  <button
                    type="button"
                    className="btn ghost small"
                    onClick={steer}
                    disabled={!draft || aborting}
                    title="下一次工具结果后注入输入框内容"
                  >
                    <Icon name="steer" />
                    转向
                  </button>
                  <button
                    type="button"
                    className="btn ghost small"
                    onClick={queue}
                    disabled={!draft || aborting}
                    title="本轮结束后再跑输入框内容"
                  >
                    <Icon name="queue" />
                    排队
                  </button>
                </div>
              </div>
            ) : null}
            {slash.length > 0 ? (
              <div className="slash-pop" role="listbox">
                {slash.map(([c, d], i) => (
                  <button
                    key={c}
                    type="button"
                    className={`slash-item${i === slashSel ? " sel" : ""}`}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      applySlash(c);
                    }}
                  >
                    <span className="cmd">{c}</span>
                    <span className="desc">{d}</span>
                  </button>
                ))}
              </div>
            ) : null}
            <div className="composer">
              {atts.length > 0 ? (
                <div className="toolbar" style={{ marginBottom: 6 }}>
                  {atts.map((f, i) => (
                    <span className="chip" key={f.path}>
                      {f.name}
                      <button
                        type="button"
                        className="cbtn"
                        aria-label={`移除 ${f.name}`}
                        onClick={() => setAtts(atts.filter((_, j) => j !== i))}
                      >
                        ×
                      </button>
                    </span>
                  ))}
                </div>
              ) : null}
              <textarea
                ref={taRef}
                rows={1}
                value={text}
                placeholder={placeholder}
                onChange={(e) => {
                  const v = e.target.value;
                  setText(v);
                  if (v.startsWith("/")) {
                    const q = v.slice(1).toLowerCase();
                    const rows = SLASH.filter(([c, d]) => (c + d).toLowerCase().includes(q));
                    setSlash(rows);
                    setSlashSel(0);
                  } else setSlash([]);
                }}
                onKeyDown={(e) => {
                  if (imeBusy(e) || imeLock.current) return;
                  if (slash.length && e.key === "Escape") {
                    e.preventDefault();
                    setSlash([]);
                    return;
                  }
                  if (slash.length && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
                    e.preventDefault();
                    setSlashSel((s) =>
                      e.key === "ArrowDown" ? Math.min(slash.length - 1, s + 1) : Math.max(0, s - 1),
                    );
                    return;
                  }
                  if (slash.length && e.key === "Tab") {
                    e.preventDefault();
                    applySlash(slash[slashSel][0]);
                    return;
                  }
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    if (slash.length) {
                      // 敲全命令（含带参数）直接回车执行；半截命令先补全。
                      const cmd = slash[slashSel][0];
                      const typed = text.trim();
                      if (typed === cmd || typed.startsWith(`${cmd} `)) {
                        setSlash([]);
                        send();
                      } else applySlash(cmd);
                    } else send();
                  }
                }}
                onCompositionStart={() => {
                  imeLock.current = true;
                }}
                onCompositionEnd={() => {
                  // Some engines fire compositionend, then a leftover Enter in the same frame.
                  imeLock.current = true;
                  requestAnimationFrame(() => {
                    imeLock.current = false;
                  });
                }}
                onPaste={(e) => {
                  const files = [...(e.clipboardData?.files || [])];
                  if (files.length) {
                    e.preventDefault();
                    upload(files);
                  }
                }}
                onDragOver={(e) => e.preventDefault()}
                onDrop={(e) => {
                  e.preventDefault();
                  if (e.dataTransfer.files.length) upload(e.dataTransfer.files);
                }}
              />
              <div className="composer-bar">
                <input
                  ref={fileRef}
                  type="file"
                  multiple
                  hidden
                  onChange={(e) => e.target.files && upload(e.target.files)}
                />
                <button type="button" className="cbtn" title="上传附件（也可直接粘贴/拖入）" aria-label="上传附件" onClick={() => fileRef.current?.click()}>
                  <Icon name="clip" />
                </button>
                <button
                  type="button"
                  className="cbtn"
                  title="斜杠命令"
                  aria-label="斜杠命令"
                  onClick={() => {
                    setText("/");
                    setSlash(SLASH);
                    setSlashSel(0);
                    taRef.current?.focus();
                  }}
                >
                  <Icon name="command" />
                </button>
                <span className="bar-sep" aria-hidden />
                <button
                  type="button"
                  className={`pill-btn ap-${approvals}`}
                  title={`审批模式：ask 逐一审批写改 · auto 放行 write/edit · yolo 全放行。点击切换（当前 ${approvals}）`}
                  onClick={cycleApprovals}
                >
                  <Icon name="lock" />
                  {approvals}
                </button>
                <button
                  type="button"
                  className={`pill-btn${snap.plan_mode ? " on" : ""}`}
                  title={snap.plan_mode ? "计划模式开启中：只读不改。点击关闭" : "开启计划模式：先勘察再动手，写改类工具被挡"}
                  aria-pressed={!!snap.plan_mode}
                  onClick={togglePlan}
                >
                  <Icon name="list" />
                  plan
                </button>
                {uploading > 0 ? (
                  <span className="upl-chip">
                    <span className="act-spin" aria-hidden />
                    上传中…
                  </span>
                ) : null}
                <span className="spacer" style={{ flex: 1 }} />
                <span className="composer-hint">
                  {busy ? `${policyHint} · 也可点停止 / 转向 / 排队` : "Enter 发送 · Shift+Enter 换行"}
                </span>
                <button
                  type="button"
                  className="send-btn"
                  onClick={send}
                  disabled={uploading > 0 || (busy ? !draft && atts.length === 0 : false)}
                  title={uploading > 0 ? "附件上传中…" : undefined}
                >
                  {sendLabel} <Icon name="arrow-up" />
                </button>
              </div>
            </div>
          </div>
        </div>
        {histOpen ? (
          <>
            <div className="drawer-mask" onClick={() => setHistOpen(false)} />
            <aside className="drawer" aria-label="聊天历史">
              <header>
                <label className="tick" title="全选">
                  <input
                    type="checkbox"
                    checked={sessions.length > 0 && sessions.every((s) => picked.has(s.id))}
                    disabled={sessions.length === 0}
                    onChange={(e) => setPicked(e.target.checked ? new Set(sessions.map((s) => s.id)) : new Set())}
                    aria-label="全选会话"
                  />
                </label>
                <b>聊天历史</b>
                <span className="spacer" style={{ flex: 1 }} />
                {picked.size > 0 ? (
                  <button type="button" className="btn danger small" onClick={() => void deleteSessions([...picked])}>
                    删除 {picked.size}
                  </button>
                ) : null}
                <button className="btn ghost small" onClick={() => setHistOpen(false)}>
                  关闭
                </button>
              </header>
              <div style={{ overflow: "auto", flex: 1 }}>
                {sessions.length === 0 ? <Empty title="没有会话" body="点新建聊天开始。" /> : null}
                {sessions.map((s) => (
                  <div key={s.id} className={`session-row${s.id === snap.session ? " on" : ""}`}>
                    <label className="tick">
                      <input
                        type="checkbox"
                        checked={picked.has(s.id)}
                        onChange={(e) => togglePick(s.id, e.target.checked)}
                        aria-label={`选择 ${sessionName(s)}`}
                      />
                    </label>
                    <button
                      type="button"
                      className="session-item"
                      onClick={async () => {
                        setErr("");
                        try {
                          await rpc("session.resume", { session: s.id });
                          setHistOpen(false);
                          await refreshSessions();
                          await onReload();
                        } catch (e) {
                          setErr(failMsg(e));
                        }
                      }}
                    >
                      <div className="t">{sessionName(s)}</div>
                      {s.id ? <div className="sid">{s.id}</div> : null}
                      <div className="m">
                        {s.mode || "agent"} · {s.channel || "console"} · {s.events ?? 0} events
                      </div>
                    </button>
                    <button
                      type="button"
                      className="btn ghost small session-del"
                      title="删除"
                      aria-label={`删除 ${sessionName(s)}`}
                      onClick={() => void deleteSessions([s.id])}
                    >
                      <Icon name="trash" />
                    </button>
                  </div>
                ))}
              </div>
            </aside>
          </>
        ) : null}
      </div>
      <aside className={`details${detailsOpen ? "" : " closed"}`}>
        <div className="dt-head">
          会话状态
          <button type="button" className="icon-btn dt-close" title="收起面板" aria-label="收起面板" onClick={onToggleDetails}>
            <Icon name="x" />
          </button>
        </div>
        <div className="dt-scroll">
          <div className="dt-block">
            <div className="cap">当前会话</div>
            <div className="kv"><span>模式</span><b>{snap.mode || "—"}</b></div>
            <div className="kv"><span>审批</span><b>{snap.approvals || "—"}</b></div>
            <div className="kv"><span>计划模式</span><b>{snap.plan_mode ? "on" : "off"}</b></div>
            <div className="kv"><span>忙碌策略</span><b>{snap.busy || "—"}</b></div>
            <div className="kv"><span>运行</span><b>{busy ? PHASE_LABEL[phase] : "空闲"}</b></div>
            <div className="kv"><span>排队</span><b>{queued}</b></div>
            <div className="kv"><span>转向</span><b>{steered}</b></div>
            <div className="kv"><span>低精度</span><b>{snap.low_precision ? "on" : "off"}</b></div>
          </div>
          {(snap.queue_preview?.length || snap.steer_preview?.length) ? (
            <div className="dt-block">
              <div className="cap">待处理</div>
              {(snap.queue_preview || []).map((t, i) => (
                <div className="sub" key={`q${i}`} style={{ marginBottom: 6 }}>排队 · {t}</div>
              ))}
              {(snap.steer_preview || []).map((t, i) => (
                <div className="sub" key={`s${i}`} style={{ marginBottom: 6 }}>转向 · {t}</div>
              ))}
            </div>
          ) : null}
          <div className="dt-block">
            <div className="cap">上下文窗口</div>
            <div className="gauge">
              <div className="g-label">
                <span>当前前缀 / 窗口</span>
                <b>
                  {used.toLocaleString()} / {win ? win.toLocaleString() : "—"} ({rawPct}%)
                </b>
              </div>
              <div className="g-track">
                <div className="g-fill" style={{ width: `${pct}%` }} />
              </div>
            </div>
            <div className="sub" style={{ marginTop: 8 }}>
              最近一次模型调用的 prompt。compact 之后是压缩后的活窗口，不是历史上每跳相加。
              {compacts ? ` 已 compact ${compacts} 次。` : ""}
            </div>
          </div>
          <div className="dt-block">
            <div className="cap">累计用量</div>
            <div className="kv"><span>prompt</span><b>{(usage?.prompt_tokens ?? 0).toLocaleString()}</b></div>
            <div className="kv"><span>completion</span><b>{(usage?.completion_tokens ?? 0).toLocaleString()}</b></div>
            <div className="kv"><span>cached</span><b>{usage?.cached_reported ? (usage.cached_tokens ?? 0).toLocaleString() : "n/a"}</b></div>
            <div className="kv"><span>前缀命中率</span><b>{hit}</b></div>
            <div className="kv"><span>assistant 步数</span><b>{usage?.assistant_steps ?? 0}</b></div>
            <div className="kv"><span>compact</span><b>{compacts}</b></div>
          </div>
          <div className="dt-block">
            <div className="cap">本轮产物</div>
            {arts.length === 0 ? <div className="sub">本轮还没有写入的交付文件。</div> : null}
            {arts.map((p) => (
              <a key={p} className="artifact" href={`/api/files?path=${encodeURIComponent(p)}`} target="_blank" rel="noreferrer">
                <div className="a-ico"><Icon name="file" /></div>
                <div>
                  <div className="a-name" style={{ fontFamily: "var(--mono)", fontSize: 12 }}>{p.split("/").pop()}</div>
                  <div className="sub">{p}</div>
                </div>
              </a>
            ))}
          </div>
        </div>
      </aside>
    </div>
  );
}

/* ── 轮次分组：把会话事件折成「用户消息 + 轨迹块 + 正文」 ────────── */

const HIDE_OPEN = "<tool_response>";
const HIDE_CLOSE = "</tool_response>";

/** Harness 注入的隐藏注记（守卫、提示、转向）以 tool_response 包裹存进 user 事件。 */
function hiddenNote(text: string): string | null {
  const t = text.trim();
  if (!t.startsWith(HIDE_OPEN) || !t.endsWith(HIDE_CLOSE)) return null;
  return t.slice(HIDE_OPEN.length, Math.max(HIDE_OPEN.length, t.length - HIDE_CLOSE.length)).trim();
}

function firstLine(s: string): string {
  for (const line of s.split("\n")) {
    const t = line.trim();
    if (t) return t;
  }
  return "";
}

function clipEnd(s: string, n: number): string {
  const cs = [...s];
  return cs.length <= n ? s : `${cs.slice(0, n - 1).join("")}…`;
}

/** 直播思考里“正在说的那句话”：取末行的尾段。 */
function thinkTail(s: string): string {
  const t = s.trimEnd();
  const nl = t.lastIndexOf("\n");
  const line = (nl >= 0 ? t.slice(nl + 1) : t).trim();
  const cs = [...line];
  return cs.length <= 64 ? line : `…${cs.slice(-63).join("")}`;
}

type ToolStep = {
  kind: "tool";
  id: string;
  name: string;
  args: string;
  output?: string;
  done: boolean;
};
type Step = ToolStep | { kind: "think"; text: string; live?: boolean } | { kind: "note"; text: string };
type ActivityBlockData = { kind: "activity"; steps: Step[] };
type Block = ActivityBlockData | { kind: "text"; text: string; live?: boolean } | { kind: "sys"; text: string };
type TurnGroup = {
  key: string;
  user?: string;
  blocks: Block[];
  /** 有引擎 timings 的 hop 加权平均 decode tok/s。 */
  decodeTokS: number | null;
};

/** djb2：给轮次生成内容稳定的 key，history.replace / compact 后不漂移。 */
function hashText(s: string): string {
  let h = 5381;
  for (let i = 0; i < s.length; i++) h = ((h << 5) + h + s.charCodeAt(i)) >>> 0;
  return h.toString(36);
}

function buildTurns(events: SessionEvent[]): TurnGroup[] {
  const turns: TurnGroup[] = [];
  const seen = new Map<string, number>();
  let cur: TurnGroup | undefined;
  let act: ActivityBlockData | undefined;
  let ratedTok = 0;
  let decodeSec = 0;

  const turn = (): TurnGroup => {
    if (!cur) {
      cur = { key: "t-head", blocks: [], decodeTokS: null };
      turns.push(cur);
    }
    return cur;
  };
  const finishRates = () => {
    if (!cur) return;
    cur.decodeTokS = decodeSec > 0 ? ratedTok / decodeSec : null;
  };
  const noteDecode = (e: SessionEvent) => {
    const c = e.completion_tokens || 0;
    const r = e.decode_tok_s;
    if (c > 0 && typeof r === "number" && r > 0 && Number.isFinite(r)) {
      ratedTok += c;
      decodeSec += c / r;
    }
  };
  const activity = (): ActivityBlockData => {
    if (!act) {
      act = { kind: "activity", steps: [] };
      turn().blocks.push(act);
    }
    return act;
  };

  events.forEach((e, i) => {
    switch (e.type) {
      case "user": {
        const note = hiddenNote(e.text || "");
        if (note !== null) {
          if (note) activity().steps.push({ kind: "note", text: note });
          return;
        }
        finishRates();
        act = undefined;
        ratedTok = 0;
        decodeSec = 0;
        const hv = hashText(e.text || "");
        const n = (seen.get(hv) || 0) + 1;
        seen.set(hv, n);
        cur = { key: `u${hv}.${n}`, user: e.text || "", blocks: [], decodeTokS: null };
        turns.push(cur);
        return;
      }
      case "assistant": {
        noteDecode(e);
        if (e.reasoning) activity().steps.push({ kind: "think", text: e.reasoning });
        if (e.content) {
          act = undefined;
          turn().blocks.push({ kind: "text", text: e.content });
        }
        (e.tool_calls || []).forEach((c, j) => {
          activity().steps.push({
            kind: "tool",
            id: c.id || `${i}.${j}`,
            name: c.function?.name || "tool",
            args: c.function?.arguments || "",
            done: false,
          });
        });
        return;
      }
      case "tool": {
        const id = e.tool_call_id;
        let hit: ToolStep | undefined;
        const blocks = turn().blocks;
        outer: for (let b = blocks.length - 1; b >= 0; b--) {
          const blk = blocks[b];
          if (blk.kind !== "activity") continue;
          for (let s = blk.steps.length - 1; s >= 0; s--) {
            const st = blk.steps[s];
            if (st.kind === "tool" && !st.done && (!id || st.id === id)) {
              hit = st;
              break outer;
            }
          }
        }
        if (!hit) {
          hit = { kind: "tool", id: id || `r${i}`, name: e.name || "tool", args: "", done: false };
          activity().steps.push(hit);
        }
        hit.done = true;
        hit.output = e.output || "";
        return;
      }
      case "stop": {
        act = undefined;
        // 常规收束不值得占一行，只显示有信息量的停机原因（中止、守卫、错误）。
        const reason = (e.reason || "").trim();
        if (reason && reason !== "stop" && reason !== "done" && reason !== "end_turn") {
          turn().blocks.push({ kind: "sys", text: reason });
        }
        return;
      }
      case "session/compact":
        act = undefined;
        turn().blocks.push({ kind: "sys", text: "上下文已压缩，早期轨迹已归档。" });
        return;
      default:
        return;
    }
  });
  finishRates();
  return turns;
}

/** 单个轮次。memo 后流式增量只触发最后一轮重渲，长会话打字不再整屏刷新。 */
const TurnView = memo(function TurnView({
  turnKey,
  user,
  blocks,
  active,
  decodeTokS,
  callLabel,
  elapsed,
  openBlocks,
  openSteps,
  onToggleBlock,
  onToggleStep,
}: {
  turnKey: string;
  user?: string;
  blocks: Block[];
  active: boolean;
  decodeTokS: number | null;
  callLabel: string;
  elapsed: number;
  openBlocks: Set<string>;
  openSteps: Set<string>;
  onToggleBlock: (k: string) => void;
  onToggleStep: (k: string) => void;
}) {
  const answer = blocks
    .filter((b): b is { kind: "text"; text: string; live?: boolean } => b.kind === "text")
    .map((b) => b.text)
    .filter(Boolean)
    .join("\n\n");
  return (
    <section className="turn" data-turn={turnKey}>
      {user !== undefined ? (
        <div className="msg user">
          <div className="bubble">{user}</div>
        </div>
      ) : null}
      {blocks.map((b, bi) => {
        const bk = `${turnKey}:${bi}`;
        if (b.kind === "text") return <MdText key={bk} text={b.text} live={b.live} />;
        if (b.kind === "sys") {
          return (
            <div key={bk} className="turn-sys">
              {b.text}
            </div>
          );
        }
        const running = active && bi === blocks.length - 1;
        return (
          <ActivityBlock
            key={bk}
            bk={bk}
            steps={b.steps}
            running={running}
            callLabel={callLabel}
            elapsed={elapsed}
            open={openBlocks.has(bk)}
            openSteps={openSteps}
            onToggle={onToggleBlock}
            onToggleStep={onToggleStep}
          />
        );
      })}
      <TurnFoot answer={answer} decodeTokS={decodeTokS} />
    </section>
  );
});

function TurnFoot({ answer, decodeTokS }: { answer: string; decodeTokS: number | null }) {
  const [copied, setCopied] = useState(false);
  if (!answer && decodeTokS == null) return null;
  return (
    <div className="turn-foot">
      {answer ? (
        <button
          type="button"
          className="copy-ans"
          title="复制回答全文"
          aria-label="复制回答全文"
          onClick={() => {
            void copyText(answer).then((ok) => {
              if (!ok) return;
              setCopied(true);
              window.setTimeout(() => setCopied(false), 1400);
            });
          }}
        >
          <Icon name={copied ? "check" : "copy"} />
        </button>
      ) : null}
      {decodeTokS != null ? (
        <span className="toks" title="本轮解码平均速度（completion tokens / decode 时间）">
          {fmtTokS(decodeTokS)}
        </span>
      ) : null}
    </div>
  );
}

function actSummary(steps: Step[]): string {
  let think = 0;
  let tool = 0;
  let note = 0;
  for (const s of steps) {
    if (s.kind === "think") think++;
    else if (s.kind === "tool") tool++;
    else note++;
  }
  const bits: string[] = [];
  if (think > 0) bits.push(think === 1 ? "思考" : `思考 ${think} 段`);
  if (tool > 0) bits.push(`工具 ${tool} 次`);
  if (bits.length === 0) return note > 0 ? "系统注记" : "轨迹";
  return bits.join(" · ");
}

function argPreview(name: string, raw: string): string {
  let a: Record<string, unknown> = {};
  try {
    a = JSON.parse(raw || "{}") as Record<string, unknown>;
  } catch {
    return clipEnd(firstLine(raw), 64);
  }
  const s = (k: string) => (typeof a[k] === "string" ? (a[k] as string) : "");
  switch (name) {
    case "read":
    case "view":
    case "write":
    case "edit":
      return s("path");
    case "bash":
      return clipEnd(firstLine(s("command")), 64);
    case "run_code":
      return clipEnd(firstLine(s("code")), 64);
    case "web":
      return clipEnd(s("query") || s("url"), 64);
    case "search":
    case "memory_search":
    case "recall":
      return clipEnd(s("query"), 64);
    case "skill":
      return s("name");
    case "mcp":
      return [s("server"), s("method")].filter(Boolean).join(" · ");
    default: {
      const v = Object.values(a).find((x) => typeof x === "string") as string | undefined;
      return clipEnd(firstLine(v || ""), 64);
    }
  }
}

function toolIcon(name: string): string {
  const n = name.toLowerCase();
  if (n === "read" || n === "view") return "book";
  if (n === "edit" || n === "write") return "edit";
  if (n === "bash" || n === "run_code") return "terminal";
  if (n === "web") return "globe";
  if (n === "mcp") return "plug";
  if (n === "search" || n === "memory_search" || n === "recall") return "search";
  if (n === "skill") return "spark";
  return "wrench";
}

function stepStatus(s: ToolStep): "run" | "ok" | "err" | "warn" {
  if (!s.done) return "run";
  const out = (s.output || "").trimStart();
  if (out === "tool task aborted") return "warn";
  if (/^(error|错误)/i.test(out)) return "err";
  return "ok";
}

function fmtArgs(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

function StepRow({
  step,
  sk,
  open,
  onToggle,
}: {
  step: Step;
  sk: string;
  open: boolean;
  onToggle: (k: string) => void;
}) {
  if (step.kind === "think") {
    return (
      <div className="step think">
        <button type="button" className="step-head" onClick={() => onToggle(sk)} aria-expanded={open}>
          <Icon name="spark" />
          <span className="step-name">思考</span>
          <span className="step-prev">
            {step.live && !open ? thinkTail(step.text) : clipEnd(firstLine(step.text), 80)}
          </span>
          {step.live ? <span className="st-dot run" /> : null}
        </button>
        {open ? <div className="step-full think-full">{step.text}</div> : null}
      </div>
    );
  }
  if (step.kind === "note") {
    return (
      <div className="step note">
        <button type="button" className="step-head" onClick={() => onToggle(sk)} aria-expanded={open}>
          <Icon name="shield" />
          <span className="step-name">注记</span>
          <span className="step-prev">{clipEnd(firstLine(step.text), 80)}</span>
        </button>
        {open ? <div className="step-full think-full">{step.text}</div> : null}
      </div>
    );
  }
  const st = stepStatus(step);
  return (
    <div className="step tool">
      <button type="button" className="step-head" onClick={() => onToggle(sk)} aria-expanded={open}>
        <Icon name={toolIcon(step.name)} />
        <span className="step-name mono">{step.name}</span>
        <span className="step-prev">{argPreview(step.name, step.args)}</span>
        <span className={`st-dot ${st}`} />
      </button>
      {open ? (
        <div className="step-full">
          {step.args ? <pre className="pre step-pre">{fmtArgs(step.args)}</pre> : null}
          {step.done ? (
            step.output ? (
              <pre className="pre step-pre">{step.output}</pre>
            ) : (
              <div className="sub">（无输出）</div>
            )
          ) : (
            <div className="sub">运行中…</div>
          )}
        </div>
      ) : null}
    </div>
  );
}

function ActivityBlock({
  bk,
  steps,
  running,
  callLabel,
  elapsed,
  open,
  openSteps,
  onToggle,
  onToggleStep,
}: {
  bk: string;
  steps: Step[];
  running: boolean;
  callLabel: string;
  elapsed: number;
  open: boolean;
  openSteps: Set<string>;
  onToggle: (k: string) => void;
  onToggleStep: (k: string) => void;
}) {
  const last = steps[steps.length - 1];
  let label: string;
  let preview = "";
  if (running) {
    if (last?.kind === "think" && last.live) {
      label = "思考中";
      preview = thinkTail(last.text);
    } else if (last?.kind === "tool" && !last.done) {
      label = `运行 ${last.name}`;
      preview = argPreview(last.name, last.args);
    } else {
      label = callLabel;
    }
  } else {
    label = actSummary(steps);
  }
  const canOpen = steps.length > 0;
  const expanded = open && canOpen;
  return (
    <div className={`activity${expanded ? " open" : ""}${running ? " running" : ""}`}>
      <button
        type="button"
        className="act-head"
        onClick={() => canOpen && onToggle(bk)}
        aria-expanded={expanded}
        disabled={!canOpen}
      >
        {canOpen ? <Icon name="chev-r" className="ico act-chev" /> : null}
        {running ? <span className="act-spin" aria-hidden /> : null}
        <span className={`act-label${running ? " shimmer" : ""}`}>{label}</span>
        {preview && !expanded ? <span className="act-prev">{preview}</span> : null}
        {running && elapsed > 0 ? <span className="act-time">{fmtElapsed(elapsed)}</span> : null}
      </button>
      {expanded ? (
        <div className="act-trace">
          {steps.map((s, si) => {
            const sk = `${bk}:${si}`;
            return <StepRow key={sk} step={s} sk={sk} open={openSteps.has(sk)} onToggle={onToggleStep} />;
          })}
        </div>
      ) : null}
    </div>
  );
}

export function PermitModal({
  permit,
  onClose,
}: {
  permit: NonNullable<Permit>;
  onClose: () => void;
}) {
  const go = async (d: string) => {
    await api("/permit", { method: "POST", body: JSON.stringify({ id: permit.id, decision: d }) });
    onClose();
  };
  // 审批是不可逆裁决：点遮罩 / Esc 一律不动作，只有三个按钮能裁决。
  return (
    <Overlay onClose={() => {}}>
      <div className="modal" role="dialog" aria-labelledby="permit-title">
        <h2 id="permit-title">
          <Icon name="shield" />
          工具调用审批
          <span className={`tc-badge ${toolBadge(permit.tool)}`} style={{ marginLeft: "auto" }}>
            {permit.tool}
          </span>
        </h2>
        <div className="m-sub">permit.ask · id #{permit.id}</div>
        <pre className="pre" style={{ marginBottom: 12, maxHeight: 150 }}>{permit.preview}</pre>
        <div className="sub" style={{ marginBottom: 14 }}>
          allow 放行这一次 · always 本进程记住该工具 · deny 拒绝。点空白处不会裁决；中止轮次视为 deny。
        </div>
        <div className="m-actions">
          <button className="btn danger" onClick={() => go("deny")}>拒绝</button>
          <button className="btn ink" onClick={() => go("always")}>始终允许</button>
          <button className="btn primary" onClick={() => go("allow")}>允许</button>
        </div>
      </div>
    </Overlay>
  );
}

export function fmtAgo(ts?: number | null) {
  if (!ts) return "—";
  const s = Math.max(0, Math.floor(Date.now() / 1000 - ts));
  if (s < 60) return `${s} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return new Date(ts * 1000).toLocaleString("zh-CN");
}
