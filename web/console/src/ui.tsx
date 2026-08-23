import { useEffect, useState, type ReactNode } from "react";

const PATHS: Record<string, string> = {
  chat: "M21 11.5a8.38 8.38 0 01-.9 3.8 8.5 8.5 0 01-7.6 4.7 8.38 8.38 0 01-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 01-.9-3.8 8.5 8.5 0 014.7-7.6 8.38 8.38 0 013.8-.9h.5a8.48 8.48 0 018 8v.5z",
  shield: "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z",
  radio: "M12 14a2 2 0 100-4 2 2 0 000 4zM16.2 7.8a6 6 0 010 8.4M7.8 16.2a6 6 0 010-8.4M19 5a10 10 0 010 14M5 19A10 10 0 015 5",
  clock: "M12 21a9 9 0 100-18 9 9 0 000 18zM12 7v5l3 2",
  pulse: "M22 12h-4l-3 9L9 3l-3 9H2",
  folder: "M4 20h16a2 2 0 002-2V8a2 2 0 00-2-2h-7.9a2 2 0 01-1.69-.9L9.6 3.9A2 2 0 007.93 3H4a2 2 0 00-2 2v13a2 2 0 002 2z",
  spark: "M12 3l1.9 5.8a2 2 0 001.3 1.3L21 12l-5.8 1.9a2 2 0 00-1.3 1.3L12 21l-1.9-5.8a2 2 0 00-1.3-1.3L3 12l5.8-1.9a2 2 0 001.3-1.3L12 3z",
  plug: "M9 7V2M15 7V2M6 7h12v5a6 6 0 01-6 6 6 6 0 01-6-6V7zM12 18v4",
  wrench: "M14.7 6.3a1 1 0 000 1.4l1.6 1.6a1 1 0 001.4 0l3.77-3.77a6 6 0 01-7.94 7.94l-6.91 6.91a2.12 2.12 0 01-3-3l6.91-6.91a6 6 0 017.94-7.94l-3.76 3.76z",
  chart: "M18 20V10M12 20V4M6 20v-6",
  sliders: "M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3",
  lock: "M7 11V7a5 5 0 0110 0v4M5 11h14v10H5z",
  plus: "M12 5v14M5 12h14",
  "arrow-up": "M12 19V5M5 12l7-7 7 7",
  clip: "M21.44 11.05l-9.19 9.19a6 6 0 01-8.49-8.49l8.57-8.57A4 4 0 1118 8.84l-8.59 8.57a2 2 0 01-2.83-2.83l8.49-8.48",
  command: "M18 3a3 3 0 00-3 3v12a3 3 0 003 3 3 3 0 003-3 3 3 0 00-3-3H6a3 3 0 00-3 3 3 3 0 003 3 3 3 0 003-3V6a3 3 0 00-3-3 3 3 0 00-3 3 3 3 0 003 3h12a3 3 0 003-3 3 3 0 00-3-3z",
  panel: "M5 5h14a2 2 0 012 2v10a2 2 0 01-2 2H5a2 2 0 01-2-2V7a2 2 0 012-2zM15 5v14",
  "chev-l": "M15 18l-6-6 6-6",
  "chev-r": "M9 18l6-6-6-6",
  "chev-d": "M6 9l6 6 6-6",
  file: "M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8zM14 2v6h6M16 13H8M16 17H8",
  search: "M11 19a8 8 0 100-16 8 8 0 000 16zM21 21l-4.35-4.35",
  edit: "M17 3a2.83 2.83 0 114 4L7.5 20.5 2 22l1.5-5.5L17 3z",
  book: "M4 19.5A2.5 2.5 0 016.5 17H20M6.5 2H20v20H6.5A2.5 2.5 0 014 19.5v-15A2.5 2.5 0 016.5 2z",
  globe: "M12 21a9 9 0 100-18 9 9 0 000 18zM3 12h18M12 3c2.5 2.6 4 5.7 4 9s-1.5 6.4-4 9c-2.5-2.6-4-5.7-4-9s1.5-6.4 4-9z",
  terminal: "M4 17l6-5-6-5M12 19h8",
  cpu: "M9 9h6v6H9zM4 4h16v16H4zM9 1v3M15 1v3M9 20v3M15 20v3M1 9h3M1 15h3M20 9h3M20 15h3",
  zap: "M13 2L3 14h9l-1 8 10-12h-9l1-8z",
  list: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01",
  stop: "M6 6h12v12H6z",
  steer: "M15 10l5 5-5 5M4 4v7a4 4 0 004 4h12",
  queue: "M8 6h13M8 12h13M8 18h8M3 6h.01M3 12h.01M3 18h.01",
  trash: "M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6",
  x: "M18 6L6 18M6 6l12 12",
  play: "M8 5v14l11-7z",
  copy: "M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2M10 8h8a2 2 0 012 2v8a2 2 0 01-2 2h-8a2 2 0 01-2-2V10a2 2 0 012-2z",
  check: "M20 6L9 17l-5-5",
};

export function Icon({ name, className = "ico" }: { name: string; className?: string }) {
  const d = PATHS[name] || PATHS.file;
  return (
    <svg className={className} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d={d} />
    </svg>
  );
}

export function Switch({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label?: string;
}) {
  return (
    <label className="switch" title={label}>
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} aria-label={label} />
      <span />
    </label>
  );
}

export function Seg({
  value,
  options,
  onChange,
}: {
  value: string;
  options: Array<{ id: string; label: string }>;
  onChange: (id: string) => void;
}) {
  return (
    <div className="seg" role="tablist">
      {options.map((o) => (
        <button key={o.id} type="button" className={value === o.id ? "on" : ""} onClick={() => onChange(o.id)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function PageHead({
  title,
  hint,
  actions,
}: {
  title: string;
  hint?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="page-head">
      <h1>{title}</h1>
      {hint ? <span className="hint">{hint}</span> : null}
      {actions ? <div className="actions">{actions}</div> : null}
    </div>
  );
}

export function Empty({ title, body }: { title: string; body: string }) {
  return (
    <div className="empty">
      <b>{title}</b>
      {body}
    </div>
  );
}

export function Overlay({
  children,
  onClose,
}: {
  children: ReactNode;
  onClose: () => void;
}) {
  return (
    <div
      className="overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      {children}
    </div>
  );
}

/* ── 统一对话框：替代原生 confirm / prompt / alert，风格与审批弹窗一致 ── */

type DialogPending = {
  kind: "confirm" | "prompt" | "alert";
  title: string;
  body?: string;
  initial?: string;
  danger?: boolean;
  okLabel?: string;
  resolve: (v: string | boolean | null) => void;
};

let dialogEnqueue: ((p: DialogPending) => void) | null = null;

export function uiConfirm(
  title: string,
  body?: string,
  opts?: { danger?: boolean; okLabel?: string },
): Promise<boolean> {
  return new Promise((res) => {
    if (!dialogEnqueue) {
      res(window.confirm(body ? `${title}\n${body}` : title));
      return;
    }
    dialogEnqueue({
      kind: "confirm",
      title,
      body,
      danger: opts?.danger,
      okLabel: opts?.okLabel,
      resolve: (v) => res(v === true),
    });
  });
}

export function uiPrompt(title: string, initial = "", body?: string): Promise<string | null> {
  return new Promise((res) => {
    if (!dialogEnqueue) {
      res(window.prompt(body ? `${title}\n${body}` : title, initial));
      return;
    }
    dialogEnqueue({
      kind: "prompt",
      title,
      body,
      initial,
      resolve: (v) => res(typeof v === "string" ? v : null),
    });
  });
}

export function uiAlert(title: string, body?: string): Promise<void> {
  return new Promise((res) => {
    if (!dialogEnqueue) {
      window.alert(body ? `${title}\n${body}` : title);
      res();
      return;
    }
    dialogEnqueue({ kind: "alert", title, body, resolve: () => res() });
  });
}

/** 挂在 App 根部。一次只显示一个对话框，Esc / 点遮罩 = 取消（安全方向）。 */
export function DialogHost() {
  const [queue, setQueue] = useState<DialogPending[]>([]);
  const [text, setText] = useState("");
  const cur = queue[0];

  useEffect(() => {
    dialogEnqueue = (p) => setQueue((q) => [...q, p]);
    return () => {
      dialogEnqueue = null;
    };
  }, []);

  useEffect(() => {
    setText(cur?.initial ?? "");
  }, [cur]);

  useEffect(() => {
    if (!cur) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        cur.resolve(cur.kind === "confirm" ? false : null);
        setQueue((q) => q.slice(1));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cur]);

  if (!cur) return null;
  const finish = (v: string | boolean | null) => {
    cur.resolve(v);
    setQueue((q) => q.slice(1));
  };
  const cancel = () => finish(cur.kind === "confirm" ? false : null);
  const ok = () => finish(cur.kind === "prompt" ? text : true);
  return (
    <Overlay onClose={cancel}>
      <div className="modal dialog" role="dialog" aria-modal="true" aria-label={cur.title}>
        <h2>{cur.title}</h2>
        {cur.body ? (
          <div className="sub" style={{ margin: "2px 0 4px", whiteSpace: "pre-wrap", lineHeight: 1.6 }}>
            {cur.body}
          </div>
        ) : null}
        {cur.kind === "prompt" ? (
          <input
            className="input"
            style={{ marginTop: 10 }}
            autoFocus
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing || e.keyCode === 229) return;
              if (e.key === "Enter") {
                e.preventDefault();
                ok();
              }
            }}
          />
        ) : null}
        <div className="m-actions" style={{ marginTop: 16 }}>
          {cur.kind !== "alert" ? (
            <button className="btn ghost" onClick={cancel}>
              取消
            </button>
          ) : null}
          <button className={`btn ${cur.danger ? "danger" : "primary"}`} autoFocus={cur.kind !== "prompt"} onClick={ok}>
            {cur.okLabel || (cur.kind === "alert" ? "知道了" : "确定")}
          </button>
        </div>
      </div>
    </Overlay>
  );
}
