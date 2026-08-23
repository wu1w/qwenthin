import { useEffect, useRef, useState } from "react";
import {
  api,
  failMsg,
  rpc,
  type CronJob,
  type Heartbeat,
  type Permit,
  type SessionInfo,
  type Snap,
  type Usage,
  sessionName,
} from "./api";
import { fmtAgo } from "./Chat";
import { Empty, Icon, Overlay, PageHead, Seg, Switch, uiAlert, uiConfirm, uiPrompt } from "./ui";

const WINDOW_PRESETS: Array<{ id: string; n: number; label: string }> = [
  { id: "8192", n: 8192, label: "8k" },
  { id: "32768", n: 32768, label: "32k" },
  { id: "131072", n: 131072, label: "128k" },
  { id: "262144", n: 262144, label: "262k" },
];

function parseTok(s: string): number | null {
  const t = s.trim().toLowerCase().replace(/,/g, "").replace(/_/g, "");
  const m = t.match(/^(\d+(?:\.\d+)?)([km])?$/);
  if (!m) return null;
  let n = Number(m[1]);
  if (!Number.isFinite(n)) return null;
  if (m[2] === "k") n *= 1024;
  if (m[2] === "m") n *= 1024 * 1024;
  n = Math.round(n);
  return n > 0 ? n : null;
}

function parseSteps(s: string): number | null {
  const t = s.trim();
  if (!/^\d+$/.test(t)) return null;
  const n = Number(t);
  if (!Number.isInteger(n) || n < 1 || n > 10_000) return null;
  return n;
}

export function InboxPage({ permit, onPermit }: { permit: Permit; onPermit: (p: Permit) => void }) {
  const cur = permit;
  const go = async (d: string) => {
    if (!cur) return;
    await api("/permit", { method: "POST", body: JSON.stringify({ id: cur.id, decision: d }) });
    onPermit(null);
  };
  return (
    <div className="page">
      <PageHead title="收件箱" hint="审批队列 · PermitHub FIFO" />
      <div className="page-body">
        <div className="split list-detail">
          <div className="card" style={{ padding: "6px 0" }}>
            <div style={{ padding: "8px 14px 10px" }}>
              <h2 style={{ margin: 0 }}>
                <Icon name="shield" />
                待决队列
              </h2>
              <div className="sub">一次只裁决队首。定时/心跳结果出现在聊天里。</div>
            </div>
            {cur ? (
              <div className="row on">
                <span className={`tc-badge ${cur.tool}`}>{cur.tool}</span>
                <div className="grow ellipsis">
                  <b className="mono" style={{ fontSize: 11.5 }}>
                    #{cur.id}
                  </b>
                  <div className="sub">{cur.preview.slice(0, 80)}</div>
                </div>
              </div>
            ) : (
              <Empty title="没有待审批项" body="ask 模式下 write / edit / bash / run_code / mcp 会排到这里。" />
            )}
          </div>
          <div>
            {cur ? (
              <div className="card">
                <h2>
                  <Icon name="terminal" />#{cur.id} · {cur.tool}
                </h2>
                <div className="sub">裁决前该调用阻塞在 oneshot 上</div>
                <pre className="pre" style={{ margin: "12px 0" }}>
                  {cur.preview}
                </pre>
                <div style={{ display: "flex", gap: 8 }}>
                  <button className="btn primary" onClick={() => go("allow")}>
                    允许
                  </button>
                  <button className="btn ink" onClick={() => go("always")}>
                    始终允许
                  </button>
                  <button className="btn danger" onClick={() => go("deny")}>
                    拒绝
                  </button>
                </div>
              </div>
            ) : (
              <div className="card">
                <div className="sub">
                  审批模式在「安全」页切换。聊天弹窗与本页同步，操作同一队首。
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

export { ChannelsPage } from "./channels";

export function SessionsPage({
  current,
  onOpen,
  active = true,
}: {
  current?: string;
  onOpen: () => void;
  active?: boolean;
}) {
  const [rows, setRows] = useState<SessionInfo[]>([]);
  const [q, setQ] = useState("");
  const [channel, setChannel] = useState("all");
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const load = () => rpc<{ sessions?: SessionInfo[] }>("session.list", {}).then((j) => setRows(j.sessions || []));
  useEffect(() => {
    if (active) load();
    else setPicked(new Set());
  }, [current, active]);
  const channels = [...new Set(rows.map((s) => s.channel || "console"))];
  const shown = rows.filter((s) => {
    const blob = `${s.id} ${s.title} ${s.preview} ${s.channel}`.toLowerCase();
    if (q && !blob.includes(q.toLowerCase())) return false;
    if (channel !== "all" && (s.channel || "console") !== channel) return false;
    return true;
  });
  const shownIds = shown.map((s) => s.id);
  const allOn = shownIds.length > 0 && shownIds.every((id) => picked.has(id));
  const togglePick = (id: string, on: boolean) => {
    setPicked((cur) => {
      const next = new Set(cur);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  };
  const deletePicked = async (ids: string[]) => {
    if (ids.length === 0) return;
    const hit = ids.length === 1 ? shown.find((s) => s.id === ids[0]) || rows.find((s) => s.id === ids[0]) : undefined;
    const label =
      ids.length === 1 ? `删除会话「${hit ? sessionName(hit) : ids[0]}」？` : `删除 ${ids.length} 个会话？`;
    if (!(await uiConfirm(label, "会话记录与标题将一并删除，无法恢复。", { danger: true, okLabel: "删除" }))) return;
    await rpc("session.delete", ids.length === 1 ? { session: ids[0] } : { sessions: ids });
    setPicked(new Set());
    await load();
    if (current && ids.includes(current)) onOpen();
  };
  return (
    <div className="page">
      <PageHead title="会话" hint="所有频道" />
      <div className="page-body">
        <div className="toolbar">
          <input className="input inline" placeholder="按标题 / id / 预览筛选" value={q} onChange={(e) => setQ(e.target.value)} />
          <select className="input" style={{ width: 160 }} value={channel} onChange={(e) => setChannel(e.target.value)}>
            <option value="all">全部频道</option>
            {channels.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
          <span className="spacer" />
          <button
            type="button"
            className="btn danger small"
            disabled={picked.size === 0}
            onClick={() => void deletePicked([...picked])}
          >
            删除已选{picked.size > 0 ? ` ${picked.size}` : ""}
          </button>
        </div>
        <div className="card table-wrap" style={{ padding: 0 }}>
          {shown.length === 0 ? <Empty title="没有匹配的会话" body="换个关键词，或先去聊天里新建。" /> : null}
          {shown.length > 0 ? (
            <table>
              <thead>
                <tr>
                  <th className="tick">
                    <input
                      type="checkbox"
                      checked={allOn}
                      onChange={(e) => setPicked(e.target.checked ? new Set(shownIds) : new Set())}
                      aria-label="全选会话"
                    />
                  </th>
                  <th>标题</th>
                  <th>频道</th>
                  <th>模式</th>
                  <th>事件</th>
                  <th>预览</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {shown.map((s) => (
                  <tr key={s.id} className={s.id === current ? "sel" : undefined}>
                    <td className="tick">
                      <input
                        type="checkbox"
                        checked={picked.has(s.id)}
                        onChange={(e) => togglePick(s.id, e.target.checked)}
                        aria-label={`选择 ${sessionName(s)}`}
                      />
                    </td>
                    <td>
                      <b>{sessionName(s)}</b>
                      <div className="sub mono">{s.id}</div>
                    </td>
                    <td>{s.channel || "console"}</td>
                    <td className="mono">{s.mode}</td>
                    <td className="mono">{s.events ?? 0}</td>
                    <td>{(s.preview || "").slice(0, 72)}</td>
                    <td>
                      <div style={{ display: "flex", gap: 6 }}>
                        <button
                          className="btn ghost small"
                          onClick={async () => {
                            try {
                              await rpc("session.resume", { session: s.id });
                              onOpen();
                            } catch (e) {
                              void uiAlert("打开会话失败", failMsg(e));
                            }
                          }}
                        >
                          打开
                        </button>
                        <button
                          className="btn ghost small"
                          onClick={async () => {
                            const title = await uiPrompt("重命名会话", sessionName(s));
                            if (title) {
                              await rpc("session.title", { session: s.id, title });
                              await load();
                            }
                          }}
                        >
                          重命名
                        </button>
                        <button className="btn danger small" onClick={() => void deletePicked([s.id])}>
                          删除
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : null}
        </div>
      </div>
    </div>
  );
}

export function CronPage({ active = true, busy = false }: { active?: boolean; busy?: boolean }) {
  const [jobs, setJobs] = useState<CronJob[]>([]);
  const [dirty, setDirty] = useState(false);
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");
  const focusRef = useRef(false);
  const dirtyRef = useRef(false);
  dirtyRef.current = dirty;
  const load = () =>
    api<{ jobs: CronJob[] }>("/jobs").then((j) => {
      setJobs(j.jobs || []);
      setDirty(false);
    });
  useEffect(() => {
    if (!active) return;
    load();
    // 轮询只为刷新 last_run；正在编辑（聚焦或有未保存改动）时绝不覆盖表单。
    const id = window.setInterval(async () => {
      try {
        const j = await api<{ jobs: CronJob[] }>("/jobs");
        const server = j.jobs || [];
        if (focusRef.current || dirtyRef.current) {
          setJobs((cur) =>
            cur.map((x) => {
              const s = server.find((y) => y.id === x.id);
              return s && s.last_run !== x.last_run ? { ...x, last_run: s.last_run } : x;
            }),
          );
        } else setJobs(server);
      } catch {
        /* 轮询失败下次再试 */
      }
    }, 3000);
    return () => window.clearInterval(id);
  }, [active]);
  const edit = (i: number, patch: Partial<CronJob>) => {
    setJobs((cur) => {
      const n = [...cur];
      n[i] = { ...n[i], ...patch };
      return n;
    });
    setDirty(true);
    setMsg("");
  };
  const save = async () => {
    for (const x of jobs) {
      if (!Number.isFinite(x.interval_s) || x.interval_s < 1) {
        setErr(`任务「${x.name || x.id}」的间隔须为 ≥ 1 的秒数`);
        return;
      }
    }
    setErr("");
    try {
      await api("/jobs", { method: "POST", body: JSON.stringify({ jobs }) });
      setMsg("已保存");
      await load();
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const addJob = () =>
    api("/jobs", {
      method: "POST",
      body: JSON.stringify({
        add: { id: crypto.randomUUID(), name: "新任务", interval_s: 3600, prompt: "", enabled: false },
      }),
    })
      .then(load)
      .catch((e) => setErr(failMsg(e)));
  const removeJob = async (x: CronJob) => {
    if (!(await uiConfirm(`删除定时任务「${x.name || x.id}」？`, undefined, { danger: true, okLabel: "删除" }))) return;
    api("/jobs", { method: "POST", body: JSON.stringify({ remove: x.id }) })
      .then(load)
      .catch((e) => setErr(failMsg(e)));
  };
  const runNow = async (x: CronJob) => {
    if (busy) {
      const ok = await uiConfirm(
        "正在回复其他消息",
        "立即运行会按忙碌策略处理（默认打断当前轮）。仍要现在运行？",
        { okLabel: "运行" },
      );
      if (!ok) return;
    }
    rpc("turn.start", { prompt: `[cron:${x.name}] ${x.prompt}` }).catch((e) => setErr(failMsg(e)));
  };
  return (
    <div className="page">
      <PageHead
        title="定时任务"
        hint="间隔到点后当成一条用户消息发出"
        actions={
          <button className="btn primary small" onClick={() => addJob()}>
            <Icon name="plus" />
            创建任务
          </button>
        }
      />
      <div className="page-body">
        <div
          className="card"
          style={{ padding: "4px 0 2px" }}
          onFocusCapture={() => {
            focusRef.current = true;
          }}
          onBlurCapture={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null)) focusRef.current = false;
          }}
        >
          {jobs.length === 0 ? (
            <Empty title="还没有定时任务" body="在聊天里说「写个定时任务」，或点创建。间隔是秒，不是 crontab 表达式。任务写在工作区 .q38/cron.json。" />
          ) : null}
          {jobs.map((x, i) => (
            <div className="row wrap" key={x.id}>
              <Switch checked={!!x.enabled} onChange={(v) => edit(i, { enabled: v })} label={`启用 ${x.name}`} />
              <input
                className="input"
                style={{ width: 120 }}
                value={x.name}
                onChange={(e) => edit(i, { name: e.target.value })}
              />
              <input
                className="input mini mono"
                type="number"
                min={1}
                value={x.interval_s}
                onChange={(e) => edit(i, { interval_s: +e.target.value })}
              />
              <span className="suffix">秒</span>
              <input
                className="input"
                style={{ flex: 1 }}
                value={x.prompt}
                placeholder="发给模型的请求"
                onChange={(e) => edit(i, { prompt: e.target.value })}
              />
              <span className="mono sub" style={{ width: 88 }}>
                {fmtAgo(x.last_run)}
              </span>
              <button className="btn ghost small" title="立刻把这条任务发给模型" onClick={() => void runNow(x)}>
                <Icon name="play" />
                立即运行
              </button>
              <button className="btn danger small" onClick={() => void removeJob(x)}>
                删除
              </button>
            </div>
          ))}
        </div>
        <div className="toolbar" style={{ marginTop: 12 }}>
          <span className="sub">空闲时才触发。开关、名称、间隔、正文都要点「保存」才生效。</span>
          <span className="spacer" />
          {dirty ? <span className="pill warn">未保存修改</span> : null}
          {msg && !dirty ? <span className="sub">{msg}</span> : null}
          {err ? <span className="err" style={{ marginTop: 0 }}>{err}</span> : null}
          <button className="btn primary" disabled={!dirty} onClick={() => void save()}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

export function HeartbeatPage({ active = true, busy = false }: { active?: boolean; busy?: boolean }) {
  const [h, setH] = useState<Heartbeat>({ enabled: false, interval_s: 3600, prompt: "", last_run: null });
  const [resolved, setResolved] = useState("[heartbeat] Check workspace status. Reply with a short note.");
  const [saved, setSaved] = useState("");
  const [err, setErr] = useState("");
  useEffect(() => {
    if (!active) return;
    api<{ heartbeat: Heartbeat; resolved_prompt?: string }>("/heartbeat").then((j) => {
      setH((prev) => ({ ...prev, ...j.heartbeat }));
      if (j.resolved_prompt) setResolved(j.resolved_prompt);
    });
  }, [active]);
  const runNowPrompt = () => {
    const typed = (h.prompt || "").trim();
    if (typed) return `[heartbeat] ${typed}`;
    return resolved;
  };
  const runNow = async () => {
    if (busy) {
      const ok = await uiConfirm(
        "正在回复其他消息",
        "立即运行会按忙碌策略处理（默认打断当前轮）。仍要现在运行？",
        { okLabel: "运行" },
      );
      if (!ok) return;
    }
    rpc("turn.start", { prompt: runNowPrompt() }).catch((e) => setErr(failMsg(e)));
  };
  return (
    <div className="page">
      <PageHead title="心跳" hint="HEARTBEAT.md" />
      <div className="page-body">
        <div className="card form-span">
          <div className="switch-row">
            <div>
              <b>启用</b>
              <div className="sub">打开后按间隔把 prompt（或 HEARTBEAT.md）当作用户消息发给模型</div>
            </div>
            <Switch checked={h.enabled} onChange={(v) => setH({ ...h, enabled: v })} label="启用心跳" />
          </div>
          <div className="field">
            <label>间隔（秒）</label>
            <input
              className="input mono"
              type="number"
              min={1}
              value={h.interval_s}
              onChange={(e) => setH({ ...h, interval_s: +e.target.value })}
            />
          </div>
          <div className="field">
            <label>请求内容（空则读工作区 HEARTBEAT.md）</label>
            <textarea className="input" rows={8} value={h.prompt} onChange={(e) => setH({ ...h, prompt: e.target.value })} />
          </div>
          <div className="sub" style={{ marginTop: 10 }}>
            上次运行 {fmtAgo(h.last_run)}
          </div>
          <div className="toolbar" style={{ marginTop: 14 }}>
            <button className="btn ghost" onClick={() => void runNow()}>
              <Icon name="play" />
              立即运行
            </button>
            <button
              className="btn primary"
              onClick={async () => {
                if (!Number.isFinite(h.interval_s) || h.interval_s < 1) {
                  setErr("间隔须为 ≥ 1 的秒数");
                  return;
                }
                setErr("");
                try {
                  await api("/heartbeat", { method: "POST", body: JSON.stringify(h) });
                  setSaved("已保存");
                } catch (e) {
                  setErr(failMsg(e));
                }
              }}
            >
              保存
            </button>
            {saved && !err ? <span className="sub">{saved}</span> : null}
            {err ? <span className="err" style={{ marginTop: 0 }}>{err}</span> : null}
          </div>
        </div>
      </div>
    </div>
  );
}

const WS_RECENTS_KEY = "q38.workspace.recents";
const WS_RECENTS_MAX = 8;

function readWsRecents(): string[] {
  try {
    const raw = localStorage.getItem(WS_RECENTS_KEY);
    const j = raw ? (JSON.parse(raw) as unknown) : [];
    return Array.isArray(j) ? j.filter((x): x is string => typeof x === "string" && !!x.trim()) : [];
  } catch {
    return [];
  }
}

function pushWsRecent(path: string): string[] {
  const p = path.trim();
  if (!p) return readWsRecents();
  const next = [p, ...readWsRecents().filter((x) => x !== p)].slice(0, WS_RECENTS_MAX);
  localStorage.setItem(WS_RECENTS_KEY, JSON.stringify(next));
  return next;
}

type WsShortcut = { id: string; label: string; path: string };
type WsBrowse = { path: string; parent?: string | null; dirs: Array<{ name: string; path: string }> };

export function FilesPage({
  active = true,
  busy = false,
  workspace = "",
}: {
  active?: boolean;
  busy?: boolean;
  workspace?: string;
}) {
  const [entries, setEntries] = useState<Array<{ path: string; dir: boolean }>>([]);
  const [root, setRoot] = useState("");
  const [parent, setParent] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [sel, setSel] = useState("");
  const [preview, setPreview] = useState<{ kind: "text" | "img" | "html" | "empty"; body: string }>({
    kind: "empty",
    body: "",
  });
  const [err, setErr] = useState("");
  const [shortcuts, setShortcuts] = useState<WsShortcut[]>([]);
  const [recents, setRecents] = useState<string[]>(readWsRecents);
  const [applying, setApplying] = useState(false);
  const [picking, setPicking] = useState(false);
  const [browse, setBrowse] = useState<WsBrowse | null>(null);
  const [browseErr, setBrowseErr] = useState("");
  const locked = busy || applying || picking;

  const load = async () => {
    const [tree, ws] = await Promise.all([
      api<{ root: string; parent?: string | null; entries: Array<{ path: string; dir: boolean }> }>("/tree"),
      api<{ shortcuts?: WsShortcut[] }>("/workspace").catch(() => ({ shortcuts: [] as WsShortcut[] })),
    ]);
    setRoot(tree.root);
    setDraft(tree.root);
    setParent(tree.parent ?? null);
    setEntries(tree.entries || []);
    setShortcuts(ws.shortcuts || []);
  };

  useEffect(() => {
    if (active) void load().catch((e) => setErr(failMsg(e)));
  }, [active, workspace]);

  const applyPath = async (path: string) => {
    const p = path.trim();
    if (!p) return;
    setErr("");
    setApplying(true);
    try {
      const j = await api<{ ok?: boolean; cancelled?: boolean; workspace?: string }>("/workspace", {
        method: "POST",
        body: JSON.stringify({ path: p }),
      });
      if (j.cancelled) return;
      const next = j.workspace || p;
      setRecents(pushWsRecent(next));
      setSel("");
      setPreview({ kind: "empty", body: "" });
      await load();
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setApplying(false);
    }
  };

  const pickNative = async () => {
    setErr("");
    setPicking(true);
    try {
      const j = await api<{ ok?: boolean; cancelled?: boolean; workspace?: string }>("/workspace/pick", {
        method: "POST",
      });
      if (j.cancelled) return;
      if (j.workspace) setRecents(pushWsRecent(j.workspace));
      setSel("");
      setPreview({ kind: "empty", body: "" });
      await load();
    } catch (e) {
      setErr(failMsg(e));
    } finally {
      setPicking(false);
    }
  };

  const openBrowse = async (path?: string) => {
    setBrowseErr("");
    try {
      const q = path ? `?path=${encodeURIComponent(path)}` : "";
      const j = await api<WsBrowse>(`/workspace/ls${q}`);
      setBrowse(j);
    } catch (e) {
      setBrowseErr(failMsg(e));
    }
  };

  const openFile = async (e: { path: string; dir: boolean }) => {
    if (e.dir) {
      if (locked) return;
      const ok = await uiConfirm("把这个文件夹设为工作区？", e.path, { okLabel: "设为工作区" });
      if (ok) void applyPath(e.path);
      return;
    }
    setSel(e.path);
    setErr("");
    const url = `/api/files?path=${encodeURIComponent(e.path)}`;
    try {
      if (/\.(png|jpe?g|gif|webp|svg)$/i.test(e.path)) setPreview({ kind: "img", body: url });
      else if (/\.html?$/i.test(e.path)) setPreview({ kind: "html", body: url });
      else {
        const t = await (await fetch(url)).text();
        setPreview({ kind: "text", body: t.slice(0, 20000) });
      }
    } catch (ex) {
      setErr(String(ex));
    }
  };

  return (
    <div className="page">
      <PageHead
        title="文件"
        hint="代码 / 写作文件夹。换目录后 read / write / edit 都跟过来。"
        actions={
          <button className="btn ghost small" onClick={() => void load().catch((e) => setErr(failMsg(e)))}>
            刷新
          </button>
        }
      />
      <div className="page-body" style={{ display: "flex", flexDirection: "column" }}>
        <div className="toolbar">
          <input
            className="input inline mono"
            aria-label="工作区路径"
            placeholder="绝对路径，或 ~/Documents"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !locked) void applyPath(draft);
            }}
            spellCheck={false}
            disabled={applying}
          />
          <button className="btn primary small" disabled={locked || !draft.trim()} onClick={() => void applyPath(draft)}>
            {applying ? "打开中…" : "打开"}
          </button>
          <button className="btn small" disabled={locked} onClick={() => void pickNative()} aria-label="系统选择文件夹">
            {picking ? "选择中…" : "系统选择"}
          </button>
          <button
            className="btn small"
            disabled={locked}
            onClick={() => void openBrowse(root || undefined)}
          >
            浏览
          </button>
          <button
            className="btn ghost small"
            disabled={locked || !parent}
            onClick={() => parent && void applyPath(parent)}
            aria-label="上级文件夹"
          >
            上级
          </button>
        </div>
        {busy ? <div className="sub">正在跑一轮，结束后才能换工作区。</div> : null}
        {shortcuts.length > 0 ? (
          <div className="ws-chips">
            {shortcuts.map((s) => (
              <button
                key={s.id}
                type="button"
                className="chip"
                disabled={locked}
                onClick={() => void applyPath(s.path)}
                title={s.path}
              >
                {s.label}
              </button>
            ))}
          </div>
        ) : null}
        {recents.length > 0 ? (
          <div className="ws-chips">
            {recents.map((p) => (
              <button
                key={p}
                type="button"
                className="chip mono"
                disabled={locked || p === root}
                onClick={() => void applyPath(p)}
                title={p}
              >
                {p.split(/[\\/]/).filter(Boolean).pop() || p}
              </button>
            ))}
          </div>
        ) : null}
        {err ? <div className="err">{err}</div> : null}
        <div className="split tree-preview" style={{ flex: 1 }}>
          <div className="card" style={{ padding: 8, maxHeight: "70vh", overflow: "auto" }}>
            {parent ? (
              <button
                type="button"
                className="tree-row"
                disabled={locked}
                onClick={() => void applyPath(parent)}
              >
                <Icon name="folder" />
                ..
              </button>
            ) : null}
            {entries.map((e) => (
              <button
                key={e.path}
                type="button"
                className={`tree-row${sel === e.path ? " on" : ""}`}
                style={{ paddingLeft: 8 + Math.min(24, (e.path.split("/").length - 1) * 12) }}
                onClick={() => void openFile(e)}
              >
                <Icon name={e.dir ? "folder" : "file"} />
                {e.path.split("/").pop()}
              </button>
            ))}
          </div>
          <div className="card">
            {preview.kind === "empty" ? (
              <Empty
                title="选择一个文件"
                body="左侧是当前工作区。用上面的路径、系统选择或浏览换文件夹。q38 没有浏览器内保存编辑器，改文件走聊天里的 write/edit。"
              />
            ) : null}
            {preview.kind === "img" ? <img className="preview-img" src={preview.body} alt={sel} /> : null}
            {preview.kind === "html" ? (
              <iframe title={sel} src={preview.body} style={{ width: "100%", height: "70vh", border: 0 }} />
            ) : null}
            {preview.kind === "text" ? <pre className="pre">{preview.body}</pre> : null}
          </div>
        </div>
      </div>
      {browse ? (
        <Overlay onClose={() => setBrowse(null)}>
          <div className="modal wide" role="dialog" aria-labelledby="ws-browse-title">
            <h2 id="ws-browse-title">
              <Icon name="folder" />
              选择文件夹
            </h2>
            <div className="m-sub">{browse.path}</div>
            <div className="toolbar">
              <button
                className="btn ghost small"
                disabled={!browse.parent}
                onClick={() => browse.parent && void openBrowse(browse.parent)}
              >
                上级
              </button>
              <span className="spacer" />
              <button className="btn ghost small" onClick={() => setBrowse(null)}>
                取消
              </button>
              <button
                className="btn primary small"
                disabled={locked}
                onClick={() => {
                  const p = browse.path;
                  setBrowse(null);
                  void applyPath(p);
                }}
              >
                使用此文件夹
              </button>
            </div>
            {browseErr ? <div className="err">{browseErr}</div> : null}
            <div className="card" style={{ padding: 8, maxHeight: "46vh", overflow: "auto", margin: 0 }}>
              {browse.dirs.length === 0 ? (
                <Empty title="没有子文件夹" body="可以直接点「使用此文件夹」。" />
              ) : (
                browse.dirs.map((d) => (
                  <button
                    key={d.path}
                    type="button"
                    className="tree-row"
                    onClick={() => void openBrowse(d.path)}
                  >
                    <Icon name="folder" />
                    {d.name}
                  </button>
                ))
              )}
            </div>
          </div>
        </Overlay>
      ) : null}
    </div>
  );
}

export function SkillsPage({ active = true }: { active?: boolean }) {
  const [data, setData] = useState<{
    auto_catalog?: boolean;
    skills?: Array<{ name: string; description: string; path: string }>;
  }>({});
  const [q, setQ] = useState("");
  const [sel, setSel] = useState<string | null>(null);
  const [err, setErr] = useState("");
  const load = () => api<typeof data>("/skills").then(setData);
  useEffect(() => {
    if (active) load();
  }, [active]);
  const skills = (data.skills || []).filter((s) => `${s.name} ${s.description}`.toLowerCase().includes(q.toLowerCase()));
  const picked = skills.find((s) => s.name === sel) || skills[0];
  return (
    <div className="page">
      <PageHead title="技能" hint="SKILL.md · 不进 tools[]" />
      <div className="page-body">
        <div className="toolbar">
          <input className="input inline" placeholder="搜索技能名 / 描述" value={q} onChange={(e) => setQ(e.target.value)} />
          <span className="spacer" />
          <span className="sub">目录写入新会话 system</span>
          <Switch
            checked={!!data.auto_catalog}
            onChange={async (v) => {
              setErr("");
              try {
                await api("/skills", { method: "POST", body: JSON.stringify({ auto_catalog: v }) });
                load();
              } catch (e) {
                setErr(failMsg(e));
              }
            }}
            label="auto catalog"
          />
        </div>
        {err ? <div className="err">{err}</div> : null}
        <div className="split list-detail">
          <div className="grid">
            {skills.length === 0 ? (
              <div className="card">
                <Empty title="没有技能" body="在 ~/.q38-agent/skills 或工作区 .q38/skills 放 SKILL.md，会话里 /技能名 触发。" />
              </div>
            ) : null}
            {skills.map((s) => (
              <button
                key={s.name}
                type="button"
                className="card"
                style={{ textAlign: "left", borderColor: picked?.name === s.name ? "var(--brand)" : undefined }}
                onClick={() => setSel(s.name)}
              >
                <h2>
                  <Icon name="spark" />
                  {s.name}
                </h2>
                <div className="sub">{s.description || "无描述"}</div>
              </button>
            ))}
          </div>
          {picked ? (
            <div className="card">
              <h2>{picked.name}</h2>
              <p>{picked.description}</p>
              <div className="sub mono">{picked.path}</div>
              <div className="sub" style={{ marginTop: 12 }}>
                技能体以隐藏用户卡注入（≤400 tok）。改 SKILL.md 后下一轮自动加载。auto catalog 只写入之后新建的会话。q38 没有逐卡启用开关。
              </div>
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

type McpServer = {
  name: string;
  command: string;
  args?: string[];
  description: string;
  methods?: string[];
  cwd?: string;
  editable?: boolean;
  env_set?: boolean;
};

/** "K=V" 每行一条 → env map；忽略空行和没有 = 的行。 */
function parseEnvLines(s: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of s.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    const i = t.indexOf("=");
    if (i <= 0) continue;
    out[t.slice(0, i).trim()] = t.slice(i + 1).trim();
  }
  return out;
}

export function McpPage({ active = true }: { active?: boolean }) {
  const [data, setData] = useState<{
    auto_catalog?: boolean;
    servers?: McpServer[];
    editable?: McpServer[];
  }>({});
  const [draft, setDraft] = useState({ name: "", command: "", args: "", methods: "", description: "", env: "" });
  const [err, setErr] = useState("");
  const [created, setCreated] = useState("");
  const [testing, setTesting] = useState(false);
  const [testOut, setTestOut] = useState<{ ok: boolean; text: string } | null>(null);
  const load = () => api<typeof data>("/mcp").then(setData);
  useEffect(() => {
    if (active) load();
  }, [active]);
  const servers = data.servers || [];
  const editable = data.editable || [];
  const draftArgs = () => (draft.args.trim() ? draft.args.trim().split(/\s+/) : []);
  const testDraft = async () => {
    if (!draft.command.trim()) return;
    setTesting(true);
    setTestOut(null);
    try {
      const j = await api<{ ok: boolean; tools?: string[]; error?: string | null }>("/mcp/test", {
        method: "POST",
        body: JSON.stringify({
          command: draft.command.trim(),
          args: draftArgs(),
          env: parseEnvLines(draft.env),
        }),
      });
      setTestOut(
        j.ok
          ? {
              ok: true,
              text: `连通成功 · ${(j.tools || []).length} 个方法${
                j.tools?.length ? `：${j.tools.slice(0, 8).join(", ")}${j.tools.length > 8 ? " …" : ""}` : ""
              }`,
            }
          : { ok: false, text: `失败：${j.error || "未知错误"}` },
      );
    } catch (e) {
      setTestOut({ ok: false, text: `失败：${failMsg(e)}` });
    } finally {
      setTesting(false);
    }
  };
  return (
    <div className="page">
      <PageHead title="MCP" hint="一个 mcp() 工具 · stdio" />
      <div className="page-body">
        <div className="toolbar">
          <span className="sub">目录写入新会话 system</span>
          <Switch
            checked={!!data.auto_catalog}
            onChange={async (v) => {
              setErr("");
              try {
                await api("/mcp", { method: "POST", body: JSON.stringify({ auto_catalog: v }) });
                load();
              } catch (e) {
                setErr(failMsg(e));
              }
            }}
            label="mcp auto catalog"
          />
        </div>
        {err ? <div className="err">{err}</div> : null}
        <div className="split mcp">
          <div>
            <div className="card" style={{ padding: "6px 0 2px" }}>
              {servers.length === 0 ? <Empty title="没有服务器" body="右侧创建客户端。tools/list 不展开进冻结 tools[]。" /> : null}
              {servers.map((s) => (
                <div className="row" key={s.name}>
                  <div className="grow">
                    <b className="mono">{s.name}</b> <span className="sub mono">{s.command}</span>
                    <div className="sub">{s.description}</div>
                    <div style={{ marginTop: 6, display: "flex", gap: 5, flexWrap: "wrap" }}>
                      {(s.methods || []).map((m) => (
                        <span className="pill ink mono" key={m}>
                          {m}
                        </span>
                      ))}
                      {s.env_set || editable.some((x) => x.name === s.name && x.env_set) ? (
                        <span className="pill idle">env 已保存</span>
                      ) : null}
                      {s.editable === false ? (
                        <span className="pill idle">mcp.toml</span>
                      ) : null}
                    </div>
                  </div>
                  {editable.some((x) => x.name === s.name) ? (
                    <button
                      className="btn danger small"
                      onClick={() => {
                        setErr("");
                        api("/mcp", {
                          method: "POST",
                          body: JSON.stringify({ remove: s.name }),
                        })
                          .then(load)
                          .catch((e) => setErr(failMsg(e)));
                      }}
                    >
                      删除
                    </button>
                  ) : (
                    <span className="sub">在文件里删</span>
                  )}
                </div>
              ))}
            </div>
          </div>
          <div className="card">
            <h2>
              <Icon name="plus" />
              创建客户端
            </h2>
            <div className="sub">写入 config.toml 的 [mcp] servers；mcp.toml 里的条目不会被整表替换冲掉</div>
            <div className="field">
              <label>name</label>
              <input className="input mono" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
            </div>
            <div className="field">
              <label>command</label>
              <input className="input mono" value={draft.command} onChange={(e) => setDraft({ ...draft, command: e.target.value })} />
            </div>
            <div className="field">
              <label>args（空格分隔）</label>
              <input className="input mono" value={draft.args} onChange={(e) => setDraft({ ...draft, args: e.target.value })} />
            </div>
            <div className="field">
              <label>methods（可选，逗号分隔）</label>
              <input className="input mono" value={draft.methods} onChange={(e) => setDraft({ ...draft, methods: e.target.value })} />
            </div>
            <div className="field">
              <label>env（可选，每行一条 KEY=VALUE，如 API_KEY=xxx）</label>
              <textarea
                className="input mono"
                rows={3}
                value={draft.env}
                spellCheck={false}
                onChange={(e) => setDraft({ ...draft, env: e.target.value })}
              />
            </div>
            <div className="field">
              <label>description</label>
              <input className="input" value={draft.description} onChange={(e) => setDraft({ ...draft, description: e.target.value })} />
            </div>
            <div className="toolbar" style={{ margin: "14px 0 0" }}>
              <button
                className="btn ghost"
                disabled={!draft.command.trim() || testing}
                title="先起一次进程跑 initialize + tools/list，验证命令和 env 是否可用"
                onClick={() => void testDraft()}
              >
                {testing ? "测试中…" : "测试连通"}
              </button>
              <button
                className="btn primary"
                onClick={() => {
                  if (!draft.name.trim() || !draft.command.trim()) return;
                  setErr("");
                  setCreated("");
                  api("/mcp", {
                    method: "POST",
                    body: JSON.stringify({
                      add: {
                        name: draft.name.trim(),
                        command: draft.command.trim(),
                        args: draftArgs(),
                        env: parseEnvLines(draft.env),
                        methods: draft.methods
                          .split(",")
                          .map((s) => s.trim())
                          .filter(Boolean),
                        description: draft.description,
                      },
                    }),
                  })
                    .then(() => {
                      setCreated(`已创建「${draft.name.trim()}」。当前会话需发 /reload 或新建会话后才可调用。`);
                      setDraft({ name: "", command: "", args: "", methods: "", description: "", env: "" });
                      setTestOut(null);
                      load();
                    })
                    .catch((e) => setErr(failMsg(e)));
                }}
              >
                创建
              </button>
            </div>
            {testOut ? (
              <div className={testOut.ok ? "sub" : "err"} style={{ marginTop: 10, overflowWrap: "anywhere" }}>
                {testOut.text}
              </div>
            ) : null}
            {created ? <div className="sub" style={{ marginTop: 10 }}>{created}</div> : null}
          </div>
        </div>
      </div>
    </div>
  );
}

export function ToolsPage({ active = true }: { active?: boolean }) {
  const [j, setJ] = useState<{
    note?: string;
    frozen?: Array<{ function?: { name: string; description: string; parameters?: unknown } }>;
    view?: { function?: { name: string; description: string } };
    skill?: { function?: { name: string; description: string } };
    mcp?: { function?: { name: string; description: string } };
    web?: { function?: { name?: string; description?: string } };
  }>({});
  const [webCfg, setWebCfg] = useState<WebCfg | null>(null);
  useEffect(() => {
    if (!active) return;
    api<typeof j>("/tools").then(setJ);
    api<{ web?: WebCfg }>("/config")
      .then((c) => setWebCfg(c.web ?? null))
      .catch(() => setWebCfg(null));
  }, [active]);
  const frozen = j.frozen || [];
  // 实际引擎以 tools 描述里的 "provider: xxx" 为准（它考虑了 env / MCP 里的 key），config 兜底。
  const webDesc = j.web?.function?.description || "";
  const webProvider =
    /provider:\s*(\w+)/.exec(webDesc)?.[1] ||
    (webCfg?.provider === "tavily" || webCfg?.tavily_key_set ? "tavily" : "builtin");
  const webEnabled = webCfg ? !!webCfg.enabled : !!j.web;
  return (
    <div className="page">
      <PageHead title="工具" hint="冻结 OpenAI tools[]" />
      <div className="page-body">
        <p className="sub">{j.note}</p>
        <div className="card" style={{ padding: "4px 0 2px", marginTop: 8 }}>
          {frozen.map((t, i) => (
            <div className="row" key={t.function?.name}>
              <b className="mono" style={{ width: 26, color: "var(--label-3)" }}>
                {i + 1}
              </b>
              <span className="mono" style={{ width: 90, fontWeight: 700 }}>
                {t.function?.name}
              </span>
              <span className="grow">{t.function?.description}</span>
              <span className="pill ok">冻结</span>
            </div>
          ))}
        </div>
        <div className="grid c3" style={{ marginTop: 12 }}>
          <div className="card">
            <h2>
              <Icon name="file" />
              view
            </h2>
            <div className="sub">{j.view?.function?.description || "媒体预览"}</div>
            <span className="pill ok" style={{ marginTop: 8, display: "inline-block" }}>
              追加 blob
            </span>
          </div>
          <div className="card">
            <h2>
              <Icon name="spark" />
              skill
            </h2>
            <div className="sub">{j.skill?.function?.description || "按名加载 SKILL.md"}</div>
            <span className="pill idle" style={{ marginTop: 8, display: "inline-block" }}>
              不进 tools[]
            </span>
          </div>
          <div className="card">
            <h2>
              <Icon name="plug" />
              mcp
            </h2>
            <div className="sub">{j.mcp?.function?.description || "MCP 调用"}</div>
            <span className="pill idle" style={{ marginTop: 8, display: "inline-block" }}>
              有服务器时追加
            </span>
          </div>
          <div className="card">
            <h2>
              <Icon name="globe" />
              web
            </h2>
            <div className="sub">{webDesc || "联网搜索与抓页（query 搜索 / url 抓正文）"}</div>
            <div style={{ marginTop: 8, display: "flex", gap: 6, flexWrap: "wrap" }}>
              <span className={`pill ${webEnabled ? "ok" : "idle"}`}>{webEnabled ? "已启用" : "未启用"}</span>
              {webEnabled ? <span className="pill ink mono">{webProvider}</span> : null}
            </div>
            <div className="sub" style={{ marginTop: 8 }}>
              在「模型」页配置开关与 Tavily key。
            </div>
          </div>
        </div>
        <div className="card" style={{ marginTop: 12 }}>
          <h2>
            <Icon name="shield" />
            内建护栏
          </h2>
          <div className="sub" style={{ marginBottom: 4 }}>
            始终在线，无需配置。触发时会以「注记」出现在聊天轨迹里。
          </div>
          <ul className="sub" style={{ margin: "6px 0 0", paddingLeft: 18, lineHeight: 1.9 }}>
            <li>盲覆写保护：write 一个本会话没读过的已有文件会被拒绝，先 read 再写。</li>
            <li>复读围栏：同一工具同参数连打先提醒、再停机，防死循环烧 token。</li>
            <li>审批门控：ask 模式下 write / edit / bash / run_code / mcp 逐一批准。</li>
            <li>工作区边界：文件工具只能操作工作区内路径。</li>
          </ul>
        </div>
        <div className="sub" style={{ marginTop: 12 }}>
          tools[] 顺序和字节冻结，不能逐项开关。code 模式整表换成 run_code / read / bash。
        </div>
      </div>
    </div>
  );
}

type WebCfg = { enabled?: boolean; provider?: string; tavily_key_set?: boolean };

export function SettingsPage({ active = true }: { active?: boolean }) {
  const [url, setUrl] = useState("");
  const [key, setKey] = useState("");
  const [keySet, setKeySet] = useState(false);
  const [model, setModel] = useState("");
  const [meta, setMeta] = useState("");
  const [lossy, setLossy] = useState(false);
  const [windowTok, setWindowTok] = useState("");
  const [maxTokens, setMaxTokens] = useState("");
  const [maxSteps, setMaxSteps] = useState("");
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");
  const [envIgnored, setEnvIgnored] = useState<string[]>([]);
  const [web, setWeb] = useState<WebCfg | null>(null);
  const [tavilyKey, setTavilyKey] = useState("");
  const [webMsg, setWebMsg] = useState("");
  const [probe, setProbe] = useState<{ busy: boolean; ok: boolean | null; model?: string; error?: string }>({
    busy: false,
    ok: null,
  });
  const load = () =>
    api<{
      server: {
        base_url: string;
        api_key: string;
        api_key_set?: boolean;
        model: string;
        family: string;
        profile: string;
      };
      policy: { low_precision?: boolean; max_steps?: number; max_tokens?: number };
      context?: { working_window?: number };
      env_ignored?: Record<string, boolean> | string[];
      web?: WebCfg;
    }>("/config").then((j) => {
      setUrl(j.server.base_url);
      setModel(j.server.model);
      setKeySet(!!j.server.api_key_set);
      setMeta(`${j.server.family} · ${j.server.profile}`);
      setLossy(!!j.policy.low_precision);
      setWindowTok(String(j.context?.working_window ?? ""));
      setMaxTokens(String(j.policy.max_tokens ?? ""));
      setMaxSteps(String(j.policy.max_steps ?? ""));
      // 兼容两种形态：老 map（{base_url: true}）与新数组（["Q38_BASE_URL"]）。
      const raw = j.env_ignored;
      setEnvIgnored(
        Array.isArray(raw)
          ? raw
          : Object.entries(raw || {})
              .filter(([, v]) => v)
              .map(([k]) => (k.startsWith("Q38_") ? k : `Q38_${k.toUpperCase()}`)),
      );
      setWeb(j.web ?? null);
    });
  const testConn = async () => {
    setProbe((p) => ({ ...p, busy: true }));
    try {
      const j = await api<{ ok: boolean; model?: string; error?: string }>("/model");
      setProbe({ busy: false, ok: !!j.ok, model: j.model, error: j.error });
    } catch (e) {
      setProbe({ busy: false, ok: false, error: failMsg(e) });
    }
  };
  useEffect(() => {
    if (!active) return;
    load();
    testConn();
  }, [active]);
  const persistServer = async () => {
    const u = url.trim();
    if (!/^https?:\/\//.test(u)) {
      setErr("base_url 需以 http:// 或 https:// 开头");
      return;
    }
    setErr("");
    try {
      await api("/config", {
        method: "POST",
        body: JSON.stringify({ base_url: u, api_key: key, model }),
      });
      setKey("");
      setMsg("连接已写入 config.toml");
      await load();
      await testConn();
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const saveWeb = async (patch: { web_enabled?: boolean; web_tavily_api_key?: string }) => {
    setWebMsg("");
    try {
      await api("/config", { method: "POST", body: JSON.stringify(patch) });
      setTavilyKey("");
      setWebMsg("已保存");
      await load();
    } catch (e) {
      setWebMsg(failMsg(e));
    }
  };
  const parsedWindow = parseTok(windowTok);
  const presetId =
    parsedWindow && WINDOW_PRESETS.some((p) => p.n === parsedWindow) ? String(parsedWindow) : "custom";
  return (
    <div className="page">
      <PageHead title="模型" hint="GET·POST /api/config" />
      <div className="page-body">
        {envIgnored.length > 0 ? (
          <div className="banner warn">
            检测到环境变量 {envIgnored.join("、")}。q38 web 不读取 Q38_* 覆盖——生效的是本页表单与
            config.toml；环境变量只对 CLI / TUI 生效。
          </div>
        ) : null}
        <div className="grid c2">
          <div className="card">
            <h2>
              <Icon name="cpu" />
              连接 · [server]
            </h2>
            <div className="row" style={{ padding: "8px 0", borderBottom: "none" }}>
              <span
                className="dot"
                style={{
                  width: 8,
                  height: 8,
                  borderRadius: 999,
                  flex: "0 0 8px",
                  background: probe.ok === true ? "var(--ok)" : probe.ok === false ? "var(--danger)" : "var(--label-3)",
                }}
              />
              <span className="grow sub" style={{ fontSize: 12 }}>
                {probe.busy
                  ? "正在探测端点…"
                  : probe.ok === true
                    ? `模型可达${probe.model ? ` · ${probe.model}` : ""}`
                    : probe.ok === false
                      ? "模型不可达"
                      : "未探测"}
              </span>
              <button className="btn ghost small" disabled={probe.busy} onClick={() => void testConn()}>
                测试连接
              </button>
            </div>
            {probe.ok === false && probe.error ? (
              <div className="err" style={{ marginTop: 0, whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
                {probe.error}
              </div>
            ) : null}
            <div className="field">
              <label>base_url</label>
              <input
                className="input mono"
                value={url}
                placeholder="http://127.0.0.1:8080/v1"
                onChange={(e) => setUrl(e.target.value)}
                spellCheck={false}
              />
            </div>
            <div className="field">
              <label>model（空 = /v1/models 第一个）</label>
              <input
                className="input mono"
                value={model}
                onChange={(e) => setModel(e.target.value)}
                spellCheck={false}
              />
            </div>
            <div className="field">
              <label>{keySet ? "api_key（已保存，留空不改）" : "api_key（本地端点通常留空）"}</label>
              <input
                className="input mono"
                type="password"
                value={key}
                onChange={(e) => setKey(e.target.value)}
                autoComplete="off"
              />
            </div>
            <div className="toolbar" style={{ margin: "12px 0 0" }}>
              <button className="btn primary small" onClick={() => void persistServer()}>
                保存连接
              </button>
              <span className="sub">{meta}</span>
            </div>
            <div className="sub" style={{ marginTop: 8 }}>
              写入 ~/.q38-agent/config.toml，重启 q38 web 仍用这一份。
            </div>
          </div>
          <div className="card">
            <h2>
              <Icon name="sliders" />
              行为
            </h2>
            <div className="switch-row">
              <div>
                <b>low_precision</b>
                <div className="sub">收紧 doom / 复读围栏。模型看不见。</div>
              </div>
              <Switch checked={lossy} onChange={setLossy} label="低精度" />
            </div>
            <div className="field">
              <label>max_steps · 每轮最大步数</label>
              <input
                className="input mono"
                inputMode="numeric"
                placeholder="80"
                value={maxSteps}
                onChange={(e) => setMaxSteps(e.target.value)}
                spellCheck={false}
              />
              <div className="sub" style={{ marginTop: 6 }}>
                手填整数。每轮工具循环上限，默认 80，超长任务可加大。范围 1–10000。下一轮生效。
              </div>
            </div>
            <div className="sub" style={{ marginTop: 10 }}>
              技能 / MCP 目录开关在各自页上，保存本页不会改它们。
            </div>
          </div>
        </div>
        <div className="card" style={{ marginTop: 12 }}>
          <h2>
            <Icon name="chart" />
            上下文窗口
          </h2>
          <div className="sub" style={{ margin: "4px 0 10px" }}>
            写入 [context] working_window。超过才 compact。这不是单次生成上限。
          </div>
          <Seg
            value={presetId}
            options={[
              ...WINDOW_PRESETS.map((p) => ({ id: p.id, label: p.label })),
              { id: "custom", label: "自定义" },
            ]}
            onChange={(id) => {
              const p = WINDOW_PRESETS.find((x) => x.id === id);
              if (p) setWindowTok(String(p.n));
            }}
          />
          <div className="field">
            <label>working_window（token，可用 32k / 128k）</label>
            <input
              className="input mono"
              value={windowTok}
              onChange={(e) => setWindowTok(e.target.value)}
              spellCheck={false}
            />
          </div>
          <div className="field">
            <label>max_tokens · 单次生成上限</label>
            <input
              className="input mono"
              value={maxTokens}
              onChange={(e) => setMaxTokens(e.target.value)}
              spellCheck={false}
            />
          </div>
        </div>
        {web ? (
          <div className="card" style={{ marginTop: 12 }}>
            <h2>
              <Icon name="globe" />
              联网搜索 · [web]
            </h2>
            <div className="switch-row">
              <div>
                <b>启用 web 工具</b>
                <div className="sub">
                  当前引擎：{web.tavily_key_set ? "Tavily（已配 key）" : "内置（Bing / DuckDuckGo，免配置）"}。
                  填 Tavily key 可自动升级搜索质量，失败时回退内置引擎。
                </div>
              </div>
              <Switch
                checked={!!web.enabled}
                onChange={(v) => void saveWeb({ web_enabled: v })}
                label="启用 web 工具"
              />
            </div>
            <div className="field">
              <label>{web.tavily_key_set ? "Tavily API key（已保存，留空不改）" : "Tavily API key（可选）"}</label>
              <div style={{ display: "flex", gap: 8 }}>
                <input
                  className="input mono"
                  type="password"
                  value={tavilyKey}
                  placeholder="tvly-…"
                  onChange={(e) => setTavilyKey(e.target.value)}
                  autoComplete="off"
                />
                <button
                  className="btn ghost small"
                  style={{ flex: "0 0 auto", height: 31 }}
                  disabled={!tavilyKey.trim()}
                  onClick={() => void saveWeb({ web_tavily_api_key: tavilyKey.trim() })}
                >
                  保存 key
                </button>
              </div>
            </div>
            {webMsg ? <div className="sub" style={{ marginTop: 8 }}>{webMsg}</div> : null}
          </div>
        ) : null}
        <div className="toolbar" style={{ marginTop: 12 }}>
          <button
            className="btn primary"
            onClick={async () => {
              setErr("");
              setMsg("");
              if (!/^https?:\/\//.test(url.trim())) {
                setErr("base_url 需以 http:// 或 https:// 开头");
                return;
              }
              const win = parseTok(windowTok);
              const tokens = parseTok(maxTokens);
              const steps = parseSteps(maxSteps);
              if (win == null) {
                setErr("working_window 无效");
                return;
              }
              if (tokens == null) {
                setErr("max_tokens 无效");
                return;
              }
              if (steps == null) {
                setErr("max_steps 须为 1–10000 的整数");
                return;
              }
              try {
                await api("/config", {
                  method: "POST",
                  body: JSON.stringify({
                    base_url: url,
                    api_key: key,
                    model,
                    low_precision: lossy,
                    working_window: win,
                    max_tokens: tokens,
                    max_steps: steps,
                  }),
                });
                setKey("");
                setMsg("已写入 config.toml 并应用到当前会话");
                load();
              } catch (e) {
                setErr(String(e));
              }
            }}
          >
            保存并应用
          </button>
          {msg ? <span className="sub">{msg}</span> : null}
          {err ? <span className="err" style={{ marginTop: 0 }}>{err}</span> : null}
        </div>
      </div>
    </div>
  );
}

export function SecurityPage({ active = true }: { active?: boolean }) {
  const [mode, setMode] = useState("ask");
  const [scope, setScope] = useState("workspace");
  const [msg, setMsg] = useState("");
  useEffect(() => {
    if (!active) return;
    api<{ features: { approvals: string; workspace_write_only: boolean } }>("/config")
      .then((j) => {
        const next = j.features?.approvals;
        if (next) setMode(next);
        setScope(j.features?.workspace_write_only === false ? "global" : "workspace");
      })
      .catch(() => {});
  }, [active]);
  return (
    <div className="page">
      <PageHead title="安全" hint="approvals" />
      <div className="page-body">
        <div className="card form-span">
          <h2>
            <Icon name="lock" />
            审批模式
          </h2>
          <div className="sub" style={{ margin: "8px 0 12px" }}>
            ask 逐一审批修改类工具 · auto 放行 write/edit · yolo 从不审批。计划模式走聊天 /plan。
          </div>
          <Seg
            value={mode}
            options={[
              { id: "ask", label: "ask" },
              { id: "auto", label: "auto" },
              { id: "yolo", label: "yolo" },
            ]}
            onChange={setMode}
          />
          <div className="sub" style={{ marginTop: 12 }}>
            门控：write · edit · bash · run_code · mcp
          </div>
          <div className="setting-divider" />
          <h2>
            <Icon name="folder" />
            Agent 作用域
          </h2>
          <div className="sub" style={{ margin: "8px 0 12px" }}>
            工作区仅允许文件工具访问当前文件夹；全局允许使用绝对路径访问其他位置。
            终端与 Python 始终从工作区启动，但不是操作系统沙箱。
          </div>
          <Seg
            value={scope}
            options={[
              { id: "workspace", label: "工作区（推荐）" },
              { id: "global", label: "全局" },
            ]}
            onChange={setScope}
          />
          {scope === "global" ? (
            <div className="scope-warning">全局模式会扩大 Agent 可读取和修改的路径范围。</div>
          ) : null}
          <div style={{ display: "flex", alignItems: "center", marginTop: 16 }}>
            <button
              className="btn primary"
              onClick={async () => {
                try {
                  const saved = await api<{ agent_scope?: string }>("/config", {
                    method: "POST",
                    body: JSON.stringify({
                      approvals: mode,
                      workspace_write_only: scope === "workspace",
                    }),
                  });
                  if (saved.agent_scope !== scope) {
                    throw new Error("作用域未被后端应用，请重启 Qwenthin 服务后重试");
                  }
                  setMsg("已保存");
                } catch (e) {
                  setMsg(failMsg(e));
                }
              }}
            >
              保存
            </button>
            {msg ? <span className="sub" style={{ marginLeft: 10 }}>{msg}</span> : null}
          </div>
        </div>
      </div>
    </div>
  );
}

export function UsagePage({ snap }: { snap: Snap }) {
  const [u, setU] = useState<Usage | null>(null);
  useEffect(() => {
    api<Usage>("/usage")
      .then(setU)
      .catch(() => setU(null));
  }, [snap.session, snap.usage?.assistant_steps]);
  const hit =
    u?.cached_reported && u.hit_pct != null ? Number(u.hit_pct).toFixed(1) : null;
  const cached = u?.cached_tokens ?? 0;
  const prompt = u?.prompt_tokens ?? 0;
  const fresh = Math.max(0, (u?.cache_prompt_tokens ?? prompt) - cached);
  const pct = u?.cache_prompt_tokens ? Math.round((cached / u.cache_prompt_tokens) * 100) : 0;
  return (
    <div className="page">
      <PageHead title="用量" hint="UsageRecap" />
      <div className="page-body">
        <div className="grid c3">
          <div className="card">
            <h2>
              <Icon name="chart" />
              prompt
            </h2>
            <div className="stat-num">{(u?.prompt_tokens ?? 0).toLocaleString()}</div>
            <div className="sub">
              当前前缀 {(u?.live_prompt_tokens ?? u?.last_prompt_tokens ?? 0).toLocaleString()}
              {" · "}completion {(u?.completion_tokens ?? 0).toLocaleString()}
              {u?.compacts ? ` · compact ${u.compacts}` : ""}
            </div>
          </div>
          <div className="card">
            <h2>
              <Icon name="zap" />
              前缀命中
            </h2>
            <div className="stat-num">{hit ? `${hit}%` : "n/a"}</div>
            <div className="sub">
              first hop {u?.first_hop_hit_rate != null ? `${(Number(u.first_hop_hit_rate) * 100).toFixed(1)}%` : "—"}
            </div>
          </div>
          <div className="card">
            <h2>
              <Icon name="cpu" />
              步数
            </h2>
            <div className="stat-num">{u?.assistant_steps ?? 0}</div>
            <div className="sub">window {snap.window || u?.window || "—"}</div>
          </div>
        </div>
        <div className="card" style={{ marginTop: 12 }}>
          <h2>prompt 构成</h2>
          <div className="stack" style={{ marginTop: 12 }}>
            <div className="s1" style={{ width: `${pct}%` }} />
          </div>
          <div className="stack-legend">
            <span>
              <i style={{ background: "var(--brand)" }} />
              cached {u?.cached_reported ? cached.toLocaleString() : "n/a"}
            </span>
            <span>
              <i style={{ background: "var(--paper-3)" }} />
              其余 {fresh.toLocaleString()}
            </span>
          </div>
          <div className="sub" style={{ marginTop: 10 }}>
            stuck_first_hops = {u?.stuck_first_hops ?? 0}
            {u?.prefix_note ? ` · ${u.prefix_note}` : ""}
          </div>
        </div>
      </div>
    </div>
  );
}
