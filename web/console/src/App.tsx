import { useEffect, useRef, useState, type ReactNode } from "react";
import { api, connectEvents, rpc, type Clarify, type Permit, type SessionEvent, type Snap } from "./api";
import { ChatPage, ClarifyModal, PermitModal, RunChip, runPhase } from "./Chat";
import { applyHistoryIncoming, nextLive, preferFresherHistory } from "./chat-live";
import {
  ChannelsPage,
  CronPage,
  FilesPage,
  HeartbeatPage,
  InboxPage,
  McpPage,
  SecurityPage,
  SessionsPage,
  SettingsPage,
  SkillsPage,
  ToolsPage,
  UsagePage,
} from "./pages";
import logo from "./assets/logo.png";
import { DialogHost, Icon } from "./ui";

function isWinDesktop() {
  return window.qwenthinDesktop?.platform === "win32";
}

function isMacDesktop() {
  return window.qwenthinDesktop?.platform === "darwin";
}

function WinCaptionIcon({ kind }: { kind: "min" | "max" | "close" }) {
  if (kind === "min") {
    return (
      <svg viewBox="0 0 10 10" aria-hidden>
        <path d="M1 5h8" />
      </svg>
    );
  }
  if (kind === "max") {
    return (
      <svg viewBox="0 0 10 10" aria-hidden>
        <rect x="1.5" y="1.5" width="7" height="7" />
      </svg>
    );
  }
  return (
    <svg viewBox="0 0 10 10" aria-hidden>
      <path d="M2 2l6 6M8 2l-6 6" />
    </svg>
  );
}

function WindowButtons() {
  const desktop = window.qwenthinDesktop;
  if (!desktop) {
    return (
      <div className="traffic" aria-hidden>
        <span className="tl-r" />
        <span className="tl-y" />
        <span className="tl-g" />
      </div>
    );
  }
  if (desktop.platform === "win32") {
    return (
      <div className="win-caps">
        <button type="button" className="win-cap min" aria-label="最小化" onClick={() => desktop.minimize()}>
          <WinCaptionIcon kind="min" />
        </button>
        <button type="button" className="win-cap max" aria-label="最大化" onClick={() => desktop.toggleMaximize()}>
          <WinCaptionIcon kind="max" />
        </button>
        <button type="button" className="win-cap close" aria-label="关闭" onClick={() => desktop.close()}>
          <WinCaptionIcon kind="close" />
        </button>
      </div>
    );
  }
  return (
    <div className="traffic">
      <button type="button" className="tl-r" aria-label="关闭" onClick={() => desktop.close()} />
      <button type="button" className="tl-y" aria-label="最小化" onClick={() => desktop.minimize()} />
      <button type="button" className="tl-g" aria-label="最大化" onClick={() => desktop.toggleMaximize()} />
    </div>
  );
}

/** 右侧状态栏默认态：宽屏展开，窄屏收起；用户手动切换后记住偏好。 */
function initialDetails(): boolean {
  const saved = localStorage.getItem("q38.details.open");
  if (saved === "1") return true;
  if (saved === "0") return false;
  return window.matchMedia("(min-width: 1181px)").matches;
}

export type PageId =
  | "chat"
  | "inbox"
  | "channels"
  | "sessions"
  | "cron"
  | "heartbeat"
  | "files"
  | "skills"
  | "mcp"
  | "tools"
  | "settings"
  | "security"
  | "usage";

const NAV: Array<{ group: string; items: Array<{ id: PageId; label: string; icon: string; badge?: boolean }> }> = [
  {
    group: "聊天",
    items: [
      { id: "chat", label: "聊天", icon: "chat" },
      { id: "inbox", label: "收件箱", icon: "shield", badge: true },
    ],
  },
  {
    group: "控制",
    items: [
      { id: "channels", label: "频道", icon: "radio" },
      { id: "sessions", label: "会话", icon: "list" },
      { id: "cron", label: "定时任务", icon: "clock" },
      { id: "heartbeat", label: "心跳", icon: "pulse" },
    ],
  },
  {
    group: "工作区",
    items: [
      { id: "files", label: "文件", icon: "folder" },
      { id: "skills", label: "技能", icon: "spark" },
      { id: "mcp", label: "MCP", icon: "plug" },
      { id: "tools", label: "工具", icon: "wrench" },
    ],
  },
];

const FOOT: Array<{ id: PageId; label: string; icon: string }> = [
  { id: "settings", label: "模型", icon: "cpu" },
  { id: "security", label: "安全", icon: "lock" },
  { id: "usage", label: "用量", icon: "chart" },
];

const TITLES: Record<PageId, string> = {
  chat: "聊天",
  inbox: "收件箱",
  channels: "频道",
  sessions: "会话",
  cron: "定时任务",
  heartbeat: "心跳",
  files: "文件",
  skills: "技能",
  mcp: "MCP",
  tools: "工具",
  settings: "模型",
  security: "安全",
  usage: "用量",
};

function pageFromHash(): PageId {
  const h = location.hash.replace(/^#/, "") as PageId;
  if (h && TITLES[h]) return h;
  return "chat";
}

function KeepPane({
  id,
  page,
  seen,
  children,
}: {
  id: PageId;
  page: PageId;
  seen: Set<PageId>;
  children: ReactNode;
}) {
  if (page !== id && !seen.has(id)) return null;
  return (
    <div className="main-pane" hidden={page !== id} aria-hidden={page !== id}>
      {children}
    </div>
  );
}

export function App() {
  const [page, setPage] = useState<PageId>(pageFromHash);
  const [seen, setSeen] = useState<Set<PageId>>(() => new Set(["chat", pageFromHash()]));
  const [rail, setRail] = useState(false);
  const [details, setDetails] = useState(initialDetails);
  const [wsUp, setWsUp] = useState(true);
  const [snap, setSnap] = useState<Snap>({});
  const sessionRef = useRef(snap.session || "");
  if (snap.session) sessionRef.current = snap.session;
  const [events, setEvents] = useState<SessionEvent[]>([]);
  const [live, setLive] = useState({ think: "", content: "" });
  const [permit, setPermit] = useState<Permit>(null);
  const [clarify, setClarify] = useState<Clarify>(null);
  const [elapsed, setElapsed] = useState(0);
  const [link, setLink] = useState<{ ok: boolean | null; model: string; error?: string }>({
    ok: null,
    model: "",
  });

  const go = (id: PageId) => {
    setPage(id);
    history.replaceState(null, "", `#${id}`);
  };

  const onReload = async () => {
    const prevSess = sessionRef.current;
    const st = await api<Snap>("/state");
    if (st.session) sessionRef.current = st.session;
    setSnap(st);
    setPermit(st.permit ?? null);
    setClarify(st.clarify ?? null);
    const h = await api<{ events: SessionEvent[] }>("/history");
    const incoming = h.events || [];
    const switched = !!st.session && !!prevSess && st.session !== prevSess;
    setEvents((cur) => applyHistoryIncoming(cur, incoming, switched));
    setLive((l) => (switched ? { think: "", content: "" } : nextLive(incoming, l)));
  };

  const busy = !!snap.turn_in_flight;

  const goChat = () => {
    const fromOther = page !== "chat";
    go("chat");
    if (fromOther && !busy) void onReload();
  };

  useEffect(() => {
    const onHash = () => setPage(pageFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    setSeen((s) => {
      if (s.has(page)) return s;
      const n = new Set(s);
      n.add(page);
      return n;
    });
  }, [page]);

  useEffect(() => {
    return connectEvents(
      (msg) => {
        if (msg.method === "hello") {
          const p = msg.params as {
            state?: Snap;
            events?: SessionEvent[];
            permit?: Permit;
            clarify?: Clarify;
          };
          setSnap(p.state || {});
          if (p.state?.session) sessionRef.current = p.state.session;
          setEvents(p.events || []);
          setPermit(p.permit ?? null);
          setClarify(p.clarify ?? null);
          // 重连后的基线里没有断线前的旧增量，清掉避免流式文本重复。
          setLive({ think: "", content: "" });
        } else if (msg.method === "history.replace") {
          const p = msg.params as { events?: SessionEvent[]; session?: string; reset?: boolean };
          if (p.reset) {
            if (p.session) sessionRef.current = p.session;
            setEvents(p.events || []);
            setLive({ think: "", content: "" });
            return;
          }
          if (p.session && sessionRef.current && p.session !== sessionRef.current) return;
          if (p.events) {
            setEvents((cur) => preferFresherHistory(cur, p.events || []));
            setLive((l) => nextLive(p.events || [], l));
          }
        } else if (msg.method === "event.append") {
          const e = msg.params as SessionEvent;
          if (e.type === "delta") {
            setLive((l) => {
              if (e.reset) return { think: "", content: "" };
              if (e.channel === "reasoning") return { ...l, think: l.think + (e.text || "") };
              return { ...l, content: l.content + (e.text || "") };
            });
            return;
          }
          if (e.type === "assistant") {
            setEvents((xs) => {
              for (let i = xs.length - 1; i >= 0; i--) {
                if (xs[i].type !== "assistant") continue;
                if ((xs[i].content || "") === (e.content || "") && (e.content || "").trim()) return xs;
                break;
              }
              return [...xs, e];
            });
            if ((e.content || "").trim()) setLive({ think: "", content: "" });
            return;
          }
          // stop 只收束转盘，不清 live：assistant / history.replace 还没带上正文时
          // 清掉缓冲会让回复从页面上消失，要切页再回来才看得到。
          setEvents((xs) => [...xs, e]);
        } else if (msg.method === "permit.ask") {
          setPermit(msg.params as Permit);
        } else if (msg.method === "permit.clear") {
          setPermit(null);
        } else if (msg.method === "clarify.ask") {
          setClarify(msg.params as Clarify);
        } else if (msg.method === "clarify.clear") {
          setClarify(null);
        } else if (msg.method === "state") {
          const st = msg.params as Snap;
          if (st.session) sessionRef.current = st.session;
          setSnap(st);
        }
      },
      (up) => setWsUp(up),
    );
  }, []);

  const phase = runPhase({ busy, live, events, permit, clarify });
  const linked = link.ok === true;
  const modelLabel = (link.model || snap.model || "").trim();

  const toggleDetails = () =>
    setDetails((d) => {
      localStorage.setItem("q38.details.open", d ? "0" : "1");
      return !d;
    });

  useEffect(() => {
    if (!busy) {
      setElapsed(0);
      return;
    }
    const t0 = Date.now();
    const id = setInterval(() => setElapsed(Math.floor((Date.now() - t0) / 1000)), 250);
    return () => clearInterval(id);
  }, [busy, snap.session]);

  useEffect(() => {
    let stop = false;
    const tick = async () => {
      try {
        const j = await api<{ ok: boolean; model?: string; error?: string }>("/model");
        if (!stop) setLink({ ok: !!j.ok, model: j.model || "", error: j.error });
      } catch (e) {
        if (!stop) setLink((cur) => ({ ...cur, ok: false, error: String(e) }));
      }
    };
    tick();
    const id = setInterval(tick, 12000);
    return () => {
      stop = true;
      clearInterval(id);
    };
  }, [snap.model]);

  return (
    <>
      <div className="desktop" />
      <div className="window">
        <header
          className={`titlebar${isWinDesktop() ? " win" : ""}`}
          onDoubleClick={() => window.qwenthinDesktop?.toggleMaximize()}
        >
          {isWinDesktop() || isMacDesktop() ? null : <WindowButtons />}
          <div className="titlebar-title">
            <span className="doc-title">Qwenthin 控制台</span>
            <span className="doc-sub"> · {TITLES[page]}</span>
          </div>
          <div className="spacer" />
          <div className="no-drag">
            <RunChip
              phase={phase}
              elapsed={elapsed}
              queued={snap.queued ?? 0}
              steered={snap.steered ?? 0}
              onClick={() => goChat()}
            />
            <button
              type="button"
              className={`chip link-chip${linked ? "" : link.ok === false ? " bad" : ""}`}
              title={link.error || (linked ? "模型端点可达" : "点此检查模型连接")}
              onClick={() => go("settings")}
            >
              <span className={`dot${linked ? "" : link.ok === null ? " wait" : " off"}`} />
              <span className="link-txt">
                {linked
                  ? modelLabel
                    ? `模型可达 · ${modelLabel}`
                    : "模型可达"
                  : link.ok === null
                    ? "检测中"
                    : "模型不可达"}
              </span>
            </button>
          </div>
          {isWinDesktop() ? <WindowButtons /> : null}
        </header>
        {!wsUp ? (
          <div className="ws-banner" role="alert">
            与 q38 服务的连接已断开，正在自动重连… 若刚重启过服务，几秒内会自动恢复。
          </div>
        ) : null}
        <div className="body">
          <button
            type="button"
            className={`collapse-btn${rail ? " rail" : ""}`}
            title={rail ? "展开侧边栏" : "折叠侧边栏"}
            aria-label={rail ? "展开侧边栏" : "折叠侧边栏"}
            onClick={() => setRail(!rail)}
          >
            <Icon name={rail ? "chev-r" : "chev-l"} />
          </button>
          <aside className={`sidebar${rail ? " rail" : ""}`}>
            <div className="sb-head">
              <div className="wordmark">
                <img className="logo" src={logo} alt="Qwenthin" />
                <div className="name">
                  Qwenthin<small>qwen3.8-customized-harness</small>
                </div>
              </div>
            </div>
            <button
              type="button"
              className="new-session"
              onClick={async () => {
                await rpc("session.new", {});
                go("chat");
                await onReload();
              }}
            >
              <Icon name="plus" />
              <em>新建会话</em>
            </button>
            <div className="sb-scroll">
              {NAV.map((g) => (
                <div className="sb-section" key={g.group}>
                  <div className="sb-caption">{g.group}</div>
                  {g.items.map((it) => (
                    <button
                      key={it.id}
                      type="button"
                      className={`nav-item${page === it.id ? " on" : ""}${it.badge && permit ? " has-badge" : ""}`}
                      onClick={() => (it.id === "chat" ? goChat() : go(it.id))}
                    >
                      <Icon name={it.icon} />
                      <span className="txt">{it.label}</span>
                      {it.badge && permit ? <span className="badge">1</span> : null}
                    </button>
                  ))}
                </div>
              ))}
            </div>
            <div className="sb-foot">
              {FOOT.map((it) => (
                <button
                  key={it.id}
                  type="button"
                  className={`nav-item${page === it.id ? " on" : ""}`}
                  onClick={() => go(it.id)}
                >
                  <Icon name={it.icon} />
                  <span className="txt">{it.label}</span>
                </button>
              ))}
            </div>
          </aside>
          <div className="main">
            <KeepPane id="chat" page={page} seen={seen}>
              <ChatPage
                snap={snap}
                events={events}
                live={live}
                busy={busy}
                permit={permit}
                clarify={clarify}
                elapsed={elapsed}
                detailsOpen={details}
                onToggleDetails={toggleDetails}
                onReload={onReload}
              />
            </KeepPane>
            <KeepPane id="inbox" page={page} seen={seen}>
              <InboxPage permit={permit} onPermit={setPermit} />
            </KeepPane>
            <KeepPane id="channels" page={page} seen={seen}>
              <ChannelsPage active={page === "channels"} />
            </KeepPane>
            <KeepPane id="sessions" page={page} seen={seen}>
              <SessionsPage current={snap.session} active={page === "sessions"} onOpen={goChat} />
            </KeepPane>
            <KeepPane id="cron" page={page} seen={seen}>
              <CronPage active={page === "cron"} busy={busy} />
            </KeepPane>
            <KeepPane id="heartbeat" page={page} seen={seen}>
              <HeartbeatPage active={page === "heartbeat"} busy={busy} />
            </KeepPane>
            <KeepPane id="files" page={page} seen={seen}>
              <FilesPage active={page === "files"} busy={busy} workspace={snap.workspace || ""} />
            </KeepPane>
            <KeepPane id="skills" page={page} seen={seen}>
              <SkillsPage active={page === "skills"} />
            </KeepPane>
            <KeepPane id="mcp" page={page} seen={seen}>
              <McpPage active={page === "mcp"} />
            </KeepPane>
            <KeepPane id="tools" page={page} seen={seen}>
              <ToolsPage active={page === "tools"} />
            </KeepPane>
            <KeepPane id="settings" page={page} seen={seen}>
              <SettingsPage active={page === "settings"} />
            </KeepPane>
            <KeepPane id="security" page={page} seen={seen}>
              <SecurityPage active={page === "security"} />
            </KeepPane>
            <KeepPane id="usage" page={page} seen={seen}>
              <UsagePage snap={snap} />
            </KeepPane>
          </div>
        </div>
      </div>
      {permit && page !== "inbox" ? <PermitModal permit={permit} onClose={() => setPermit(null)} /> : null}
      {clarify ? <ClarifyModal clarify={clarify} onClose={() => setClarify(null)} /> : null}
      <DialogHost />
    </>
  );
}
