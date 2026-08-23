import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { api, failMsg, type ChannelEp, type ChannelKind } from "./api";
import { Empty, PageHead, Seg, Switch, uiConfirm } from "./ui";

/** 频道运行时状态 → 状态点。后端没给 runtime 字段时不显示。 */
function runtimePill(e: ChannelEp): { cls: string; label: string } | null {
  const st = e.runtime?.state;
  if (!st) return null;
  if (st === "running") return { cls: "ok", label: "运行中" };
  if (st === "error") return { cls: "err", label: "连接错误" };
  if (st === "no_credentials") return { cls: "warn", label: "缺凭证" };
  if (e.enabled) return { cls: "idle", label: "未连接" };
  return null;
}

const DM_POLICY = [
  { id: "open", label: "开放" },
  { id: "allowlist", label: "白名单" },
  { id: "closed", label: "关闭" },
];
const GROUP_POLICY = [
  { id: "open", label: "开放" },
  { id: "allowlist", label: "白名单" },
  { id: "mention", label: "需提及" },
  { id: "closed", label: "关闭" },
];

function policyName(kind: "dm" | "group", id?: string) {
  const rows = kind === "dm" ? DM_POLICY : GROUP_POLICY;
  return rows.find((r) => r.id === (id || "open"))?.label || id || "开放";
}

function matchLine(e: ChannelEp) {
  const bits = [`私信 ${policyName("dm", e.dm_policy)}`, `群聊 ${policyName("group", e.group_policy)}`];
  if (e.require_mention && e.group_policy !== "mention") bits.push("群需@");
  const n = (e.allow_from || []).length;
  if (n) bits.push(`白名单 ${n}`);
  const d = (e.deny_from || []).length;
  if (d) bits.push(`拒绝 ${d}`);
  return bits.join(" · ");
}

function extraRecord(extra?: Record<string, unknown>): Record<string, string> {
  const out: Record<string, string> = {};
  if (!extra) return out;
  for (const [k, v] of Object.entries(extra)) {
    if (typeof v === "string") out[k] = v;
  }
  return out;
}

function extraStr(e: ChannelEp, key: string): string {
  const v = e.extra?.[key];
  return typeof v === "string" ? v : "";
}

function nextEpId(eps: ChannelEp[], kind: string) {
  if (!eps.some((e) => e.id === kind)) return kind;
  let i = 2;
  while (eps.some((e) => e.id === `${kind}-${i}`)) i += 1;
  return `${kind}-${i}`;
}

function toChannelPayload(eps: ChannelEp[]) {
  return eps.map((e) => {
    const extra = extraRecord(e.extra);
    if (e.kind === "telegram" && e.bot_token?.trim()) extra.bot_token = e.bot_token.trim();
    return {
      id: e.id.trim() || nextEpId(eps, e.kind),
      kind: e.kind,
      enabled: !!e.enabled,
      bind: e.bind || "",
      reply_url: e.reply_url || "",
      require_mention: !!e.require_mention,
      dm_policy: e.dm_policy || "open",
      group_policy: e.group_policy || "open",
      allow_from: e.allow_from || [],
      deny_from: e.deny_from || [],
      secret: e.secret || "",
      extra,
    };
  });
}

function kindSpec(catalog: ChannelKind[], kind: string): ChannelKind | undefined {
  return catalog.find((c) => c.id === kind);
}

function isBound(e: ChannelEp, spec?: ChannelKind) {
  if (e.bot_token_set || e.secret_set) return true;
  const set = new Set(e.creds_set || []);
  if (spec?.fields.some((f) => f.secret && set.has(f.key))) return true;
  return set.size > 0 && !!spec?.qr;
}

function ChannelTags({
  values,
  onChange,
  placeholder,
}: {
  values: string[];
  onChange: (v: string[]) => void;
  placeholder: string;
}) {
  const [draft, setDraft] = useState("");
  const add = (raw: string) => {
    const parts = raw
      .split(/[,，\s]+/)
      .map((s) => s.trim())
      .filter(Boolean);
    if (!parts.length) return;
    const next = [...values];
    for (const p of parts) if (!next.includes(p)) next.push(p);
    onChange(next);
    setDraft("");
  };
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing || e.keyCode === 229) return;
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      add(draft);
    }
    if (e.key === "Backspace" && !draft && values.length) onChange(values.slice(0, -1));
  };
  return (
    <div className="tag-list">
      {values.map((v) => (
        <span className="tag" key={v}>
          {v}
          <button type="button" aria-label={`移除 ${v}`} onClick={() => onChange(values.filter((x) => x !== v))}>
            ×
          </button>
        </span>
      ))}
      <input
        className="input tag-input"
        value={draft}
        placeholder={placeholder}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={onKey}
        onBlur={() => {
          if (draft.trim()) add(draft);
        }}
      />
    </div>
  );
}

function ChannelMark({ spec, sm }: { spec?: ChannelKind; sm?: boolean }) {
  const color = spec?.color || "#615CED";
  return (
    <span
      className={`ch-ico${sm ? " sm" : ""}`}
      data-kind={spec?.id || "webhook"}
      style={{ background: `${color}1f`, color }}
    >
      <span className="ch-mark">{spec?.mark || "?"}</span>
    </span>
  );
}

function qrPollGap(kind: string) {
  if (kind === "feishu") return 5000;
  if (kind === "qq" || kind === "wecom" || kind === "dingtalk") return 3000;
  return 1500;
}

function qrStatusLabel(status: string, kind?: string, live?: boolean) {
  if (status === "waiting") {
    if (kind === "qq") return "等待扫码 · QQ 开通机器人可能要一会儿";
    if (kind === "dingtalk") return "等待扫码 · 钉钉可能要创建/发布应用";
    return "等待扫码";
  }
  if (status === "scanned") return "已扫，请在手机上确认";
  if (status === "success") return live ? "凭证已写入，正在连接" : "凭证已写入";
  if (status === "expired") return "二维码已过期";
  if (status === "fail") return "授权失败";
  return status;
}

function QrBind({
  kind,
  domain,
  live,
  onBound,
}: {
  kind: string;
  domain?: string;
  live?: boolean;
  onBound: (creds: Record<string, string>) => void;
}) {
  const [image, setImage] = useState("");
  const [token, setToken] = useState("");
  const [status, setStatus] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const boundRef = useRef(onBound);
  boundRef.current = onBound;

  const start = async () => {
    setErr("");
    setBusy(true);
    setStatus("");
    try {
      const q = domain ? `?domain=${encodeURIComponent(domain)}` : "";
      const j = await api<{ image: string; poll_token: string }>(`/channels/${kind}/qrcode${q}`);
      setImage(j.image || "");
      setToken(j.poll_token || "");
      setStatus("waiting");
    } catch (e) {
      setErr(failMsg(e));
      setImage("");
      setToken("");
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (!token) return;
    let stop = false;
    let inFlight = false;
    const gap = qrPollGap(kind);
    const tick = async () => {
      if (stop || inFlight) return;
      inFlight = true;
      try {
        const q = new URLSearchParams({ token });
        if (domain) q.set("domain", domain);
        const j = await api<{ status: string; credentials?: Record<string, string> }>(
          `/channels/${kind}/qrcode/status?${q.toString()}`,
        );
        if (stop) return;
        setStatus(j.status);
        if (j.status === "success") {
          setToken("");
          boundRef.current(j.credentials || {});
        } else if (j.status === "fail" || j.status === "expired") {
          setToken("");
          const reason = j.credentials?.fail_reason;
          setErr(j.status === "expired" ? "二维码已过期，请重新获取" : reason || "授权失败");
        }
      } catch (e) {
        if (!stop) setErr(failMsg(e));
      } finally {
        inFlight = false;
      }
    };
    const id = window.setInterval(tick, gap);
    tick();
    return () => {
      stop = true;
      window.clearInterval(id);
    };
  }, [token, kind, domain]);

  return (
    <div className="ch-qr">
      <div className="ch-qr-head">
        <div>
          <b>扫码绑定</b>
          <div className="sub">
            {live ? "用对应 App 扫码，绑定后本进程会连上" : "用对应 App 扫码完成绑定"}
          </div>
        </div>
        <button type="button" className="btn primary small" disabled={busy} onClick={start}>
          {image ? "刷新二维码" : "获取二维码"}
        </button>
      </div>
      {err ? <div className="err">{err}</div> : null}
      {image ? (
        <img className="ch-qr-img" src={image} alt={`${kind} 扫码绑定`} />
      ) : (
        <div className="ch-qr-ph">尚未取码。点「获取二维码」开始。</div>
      )}
      {status ? <div className={`ch-qr-st ${status}`}>{qrStatusLabel(status, kind, live)}</div> : null}
    </div>
  );
}

export function ChannelsPage({ active = true }: { active?: boolean }) {
  const [busy, setBusy] = useState("interrupt");
  const [eps, setEps] = useState<ChannelEp[]>([]);
  const [catalog, setCatalog] = useState<ChannelKind[]>([]);
  const [sel, setSel] = useState(0);
  const [open, setOpen] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [showSoon, setShowSoon] = useState(false);
  const [err, setErr] = useState("");
  const load = (keepId?: string) =>
    api<{ busy: string; endpoints: ChannelEp[]; catalog?: ChannelKind[] }>("/channels").then((j) => {
      setBusy(j.busy);
      const rows = (j.endpoints || []).map((e) => ({ ...e, _origId: e.id }));
      setEps(rows);
      if (j.catalog?.length) setCatalog(j.catalog);
      if (keepId) {
        const i = rows.findIndex((e) => e.id === keepId);
        if (i >= 0) setSel(i);
      }
    });
  useEffect(() => {
    if (active && !open) load();
  }, [active, open]);
  const cur = eps[sel];
  const spec = cur ? kindSpec(catalog, cur.kind) : undefined;
  const enabled = eps.filter((e) => e.enabled);
  const idle = eps.filter((e) => !e.enabled);
  const configuredKinds = new Set(eps.map((e) => e.kind));
  const addable = catalog.filter((c) => !c.once || !configuredKinds.has(c.id));
  // 进程内平台立即可用；其余适配器未进进程，折到「即将支持」防误解。
  const addableLive = addable.filter((c) => c.in_process);
  const addableSoon = addable.filter((c) => !c.in_process);
  const patch = (p: Partial<ChannelEp>) => {
    if (!cur) return;
    const n = [...eps];
    n[sel] = { ...cur, ...p };
    setEps(n);
    setDirty(true);
  };
  const patchExtra = (key: string, value: string) => {
    if (!cur) return;
    const extra = { ...extraRecord(cur.extra), [key]: value };
    if (key === "bot_token") patch({ extra, bot_token: value });
    else patch({ extra });
  };
  const openAt = (i: number) => {
    setSel(i);
    setOpen(true);
    setDirty(false);
  };
  const add = (kind: string) => {
    const spec = kindSpec(catalog, kind);
    if (spec?.once) {
      const existing = eps.findIndex((e) => e.kind === kind);
      if (existing >= 0) {
        openAt(existing);
        return;
      }
    }
    const row: ChannelEp = {
      id: nextEpId(eps, kind),
      kind,
      enabled: false,
      dm_policy: "open",
      group_policy: "open",
      bind: kind === "webhook" ? "127.0.0.1:8788" : "",
      extra: kind === "feishu" ? { domain: "feishu" } : {},
      _local: true,
    };
    setEps([...eps, row]);
    setSel(eps.length);
    setOpen(true);
    setDirty(false);
  };
  /** 有未保存改动时先确认再关，防止点遮罩误丢配置。 */
  const closeDrawer = async () => {
    if (dirty) {
      const ok = await uiConfirm("放弃未保存的修改？", "这个频道刚才的改动还没保存。", {
        danger: true,
        okLabel: "放弃修改",
      });
      if (!ok) return;
    }
    setOpen(false);
    setDirty(false);
    await load();
  };
  useEffect(() => {
    if (!open) return;
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") void closeDrawer();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // closeDrawer 闭包依赖 dirty/eps，最新一份即可
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, dirty]);
  const saveBusy = async (v: string) => {
    setBusy(v);
    try {
      await api("/channels", { method: "POST", body: JSON.stringify({ busy: v }) });
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const saveDrawer = async (row = cur, close = true) => {
    if (!row) return;
    try {
      setErr("");
      const payload = toChannelPayload([row])[0];
      const orig = (row._origId || "").trim();
      await api("/channels", {
        method: "POST",
        body: JSON.stringify({
          upsert: payload,
          rename: orig && orig !== payload.id ? orig : undefined,
        }),
      });
      setDirty(false);
      if (close) {
        setOpen(false);
        await load();
      } else {
        await load(payload.id);
      }
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const onQrBound = (creds: Record<string, string>) => {
    if (!cur) return;
    const extra = { ...extraRecord(cur.extra) };
    for (const [k, v] of Object.entries(creds)) {
      if (v) extra[k] = v;
    }
    const row: ChannelEp = { ...cur, extra, enabled: true, _local: false };
    const n = [...eps];
    n[sel] = row;
    setEps(n);
    saveDrawer(row, false);
  };
  const removeCur = async () => {
    const id = (cur?._origId || cur?.id || "").trim();
    if (cur?._local || !id) {
      setSel(0);
      setOpen(false);
      setEps(eps.filter((_, i) => i !== sel));
      return;
    }
    const ok = await uiConfirm(`删除频道「${id}」？`, "已保存的凭证会一并删除；扫码平台需要重新扫码绑定。", {
      danger: true,
      okLabel: "删除",
    });
    if (!ok) return;
    setSel(0);
    setOpen(false);
    setDirty(false);
    try {
      setErr("");
      await api("/channels", { method: "POST", body: JSON.stringify({ remove: id }) });
      await load();
    } catch (e) {
      setErr(failMsg(e));
    }
  };
  const card = (e: ChannelEp) => {
    const i = eps.findIndex((x) => x.id === e.id && x.kind === e.kind);
    const s = kindSpec(catalog, e.kind);
    const bound = isBound(e, s);
    const rt = runtimePill(e);
    return (
      <button
        key={`${e.kind}-${e.id}`}
        type="button"
        className={`card ch-card${open && i === sel ? " on" : ""}${e.enabled ? "" : " dim"}`}
        onClick={() => openAt(i)}
      >
        <div className="ch-card-top">
          <ChannelMark spec={s} />
          <span className={`pill ${rt ? rt.cls : e.enabled ? "ok" : "idle"}`}>
            {rt ? rt.label : e.enabled ? "已启用" : "未启用"}
          </span>
        </div>
        <div className="ch-name">{s?.name || e.kind}</div>
        <div className="ch-id mono">{e.id}</div>
        <div className="ch-tags">
          {s?.qr ? <span className="pill ink">扫码</span> : null}
          {bound ? <span className="pill ok">{s?.in_process ? "已绑定" : "凭证已写入"}</span> : null}
          {s?.in_process ? <span className="pill idle">进程内</span> : null}
        </div>
        {e.runtime?.state === "error" && e.runtime.detail ? (
          <div className="ch-err" title={e.runtime.detail}>
            {e.runtime.detail}
          </div>
        ) : null}
        <div className="sub">{matchLine(e)}</div>
        <div className="sub">{s?.blurb}</div>
      </button>
    );
  };
  return (
    <div className="page ch-page">
      <PageHead title="频道" hint="扫码或填字段绑定；进程内频道由本控制台自动连接" />
      <div className="page-body">
        <div className="toolbar">
          <span className="sub">忙碌时</span>
          <Seg
            value={busy}
            options={[
              { id: "interrupt", label: "打断" },
              { id: "queue", label: "排队" },
              { id: "steer", label: "转向" },
            ]}
            onChange={saveBusy}
          />
          <span className="spacer" />
          <span className="pill ink mono">cli</span>
          <span className="pill ink mono">sidecar</span>
          <span className="pill ink mono">console</span>
        </div>
        {err ? <div className="err">{err}</div> : null}

        <div className="ch-sec">
          <h3>
            已启用
            <span>{enabled.length}</span>
          </h3>
          {enabled.length > 0 ? (
            <div className="ch-grid">{enabled.map(card)}</div>
          ) : (
            <div className="card">
              <Empty title="没有已启用的频道" body="从「可添加」选一个平台，扫码或填凭证后打开启用。" />
            </div>
          )}
        </div>

        {idle.length > 0 ? (
          <div className="ch-sec">
            <h3>
              未启用
              <span>{idle.length}</span>
            </h3>
            <div className="ch-grid">{idle.map(card)}</div>
          </div>
        ) : null}

        <div className="ch-sec">
          <h3>
            可添加
            <span>绑定后由本进程自动连接</span>
          </h3>
          <div className="ch-grid avail">
            {addableLive.map((c) => (
              <button type="button" className="card ch-avail" key={c.id} onClick={() => add(c.id)}>
                <ChannelMark spec={c} sm />
                <span className="grow">
                  <b>{c.name}</b>
                  <div className="sub">{c.blurb}</div>
                </span>
                <span className="sub">{configuredKinds.has(c.id) && !c.once ? "再添加" : c.qr ? "扫码" : "添加"}</span>
              </button>
            ))}
          </div>
        </div>

        {addableSoon.length > 0 ? (
          <div className="ch-sec ch-soon">
            <h3>
              即将支持
              <span>适配器尚未进进程，配置只会保存凭证、不会实际收发消息</span>
              <button type="button" className="btn ghost small" style={{ marginLeft: "auto" }} onClick={() => setShowSoon((v) => !v)}>
                {showSoon ? "收起" : `显示 ${addableSoon.length} 个`}
              </button>
            </h3>
            {showSoon ? (
              <div className="ch-grid avail">
                {addableSoon.map((c) => (
                  <button type="button" className="card ch-avail" key={c.id} onClick={() => add(c.id)}>
                    <ChannelMark spec={c} sm />
                    <span className="grow">
                      <b>{c.name}</b>
                      <div className="sub">{c.blurb}</div>
                    </span>
                    <span className="pill idle">未就绪</span>
                  </button>
                ))}
              </div>
            ) : null}
          </div>
        ) : null}
      </div>

      {open && cur ? (
        <>
          <div className="drawer-mask" onClick={() => closeDrawer()} />
          <aside className="drawer ch-drawer" aria-label="频道配置">
            <header>
              <ChannelMark spec={spec} sm />
              <div className="grow min0">
                <b>{spec?.name || cur.kind}</b>
                <div className="sub">{cur.id}</div>
              </div>
              <button className="btn ghost small" onClick={() => closeDrawer()}>
                关闭
              </button>
            </header>
            <div className="drawer-body">
              <div className="switch-row">
                <div>
                  <b>已启用</b>
                  <div className="sub">
                    {spec?.in_process
                      ? spec.id === "qq"
                        ? "扫码成功后本进程会连 QQ 官方网关，手机「连接中」才会结束"
                        : spec.id === "wechat"
                          ? "扫码成功后本进程会 iLink 长轮询，才能收到微信消息"
                          : spec.id === "wecom"
                            ? "扫码成功后本进程会连企微 WebSocket"
                            : spec.id === "dingtalk"
                              ? "扫码成功后本进程会连钉钉 Stream"
                              : spec.id === "feishu"
                                ? "扫码成功后本进程会连飞书长连接"
                        : spec.id === "telegram"
                          ? "保存后本进程会 long-poll Bot API"
                          : spec.id === "webhook"
                            ? "保存后本进程会在 bind 地址接听 POST /inbound"
                            : "保存后本进程会连接官方接口"
                      : "凭证写入 config.toml。该平台消息适配器尚未进进程。"}
                  </div>
                </div>
                <Switch
                  checked={!!cur.enabled}
                  onChange={async (v) => {
                    if (v && spec && !spec.in_process) {
                      const ok = await uiConfirm(
                        "该平台适配器尚未进进程",
                        "凭证会保存进 config.toml，但本进程暂时不会实际连接、收发消息。仍要标记为启用？",
                        { okLabel: "仍然启用" },
                      );
                      if (!ok) return;
                    }
                    patch({ enabled: v });
                  }}
                  label="启用频道"
                />
              </div>
              <div className="field">
                <label>名称 · id</label>
                <input className="input mono" value={cur.id} onChange={(e) => patch({ id: e.target.value })} />
              </div>

              {spec?.qr ? (
                <QrBind
                  kind={cur.kind}
                  live={!!spec?.in_process}
                  domain={cur.kind === "feishu" ? extraStr(cur, "domain") || "feishu" : undefined}
                  onBound={onQrBound}
                />
              ) : null}

              {cur.kind === "feishu" ? (
                <div className="field">
                  <label>域名</label>
                  <Seg
                    value={extraStr(cur, "domain") || "feishu"}
                    options={[
                      { id: "feishu", label: "飞书" },
                      { id: "lark", label: "Lark" },
                    ]}
                    onChange={(domain) => patchExtra("domain", domain)}
                  />
                </div>
              ) : null}

              {(spec?.fields || [])
                .filter((f) => !(cur.kind === "feishu" && f.key === "domain"))
                .map((f) => {
                const saved = (cur.creds_set || []).includes(f.key) || (f.key === "bot_token" && cur.bot_token_set);
                return (
                  <div className="field" key={f.key}>
                    <label>
                      {f.label}
                      {saved ? "（已保存，留空不改）" : ""}
                    </label>
                    <input
                      className="input mono"
                      type={f.secret ? "password" : "text"}
                      value={f.key === "bot_token" ? cur.bot_token || extraStr(cur, f.key) : extraStr(cur, f.key)}
                      onChange={(e) => patchExtra(f.key, e.target.value)}
                      autoComplete="off"
                      placeholder={f.hint || ""}
                    />
                  </div>
                );
              })}

              {cur.kind === "webhook" ? (
                <div className="field">
                  <label>监听地址 · bind</label>
                  <input className="input mono" value={cur.bind || ""} onChange={(e) => patch({ bind: e.target.value })} />
                </div>
              ) : null}

              <div className="field">
                <label>出站 reply_url（可选）</label>
                <input className="input mono" value={cur.reply_url || ""} onChange={(e) => patch({ reply_url: e.target.value })} />
              </div>
              {cur.kind === "webhook" ? (
                <div className="field">
                  <label>secret · X-Q38-Token（留空保留）</label>
                  <input
                    className="input mono"
                    type="password"
                    value={cur.secret || ""}
                    onChange={(e) => patch({ secret: e.target.value })}
                    autoComplete="off"
                  />
                </div>
              ) : null}

              <div className="ch-block">
                <h4>匹配方式</h4>
                <p className="sub">
                  按 sender_id 过滤。deny_from 优先拒绝；白名单非空时，即使策略是「开放」也只放行名单内。
                </p>
                <div className="field">
                  <label>私信 dm_policy</label>
                  <Seg value={cur.dm_policy || "open"} options={DM_POLICY} onChange={(dm_policy) => patch({ dm_policy })} />
                </div>
                <div className="field">
                  <label>群聊 group_policy</label>
                  <Seg
                    value={cur.group_policy || "open"}
                    options={GROUP_POLICY}
                    onChange={(group_policy) => patch({ group_policy })}
                  />
                </div>
                <div className="switch-row">
                  <div>
                    <b>群聊需 @提及</b>
                    <div className="sub">未 @ 的群消息直接丢弃</div>
                  </div>
                  <Switch
                    checked={!!cur.require_mention}
                    onChange={(v) => patch({ require_mention: v })}
                    label="群聊需提及"
                  />
                </div>
                <div className="field">
                  <label>白名单 allow_from</label>
                  <ChannelTags
                    values={cur.allow_from || []}
                    onChange={(allow_from) => patch({ allow_from })}
                    placeholder="sender_id，回车添加"
                  />
                </div>
                <div className="field">
                  <label>拒绝名单 deny_from</label>
                  <ChannelTags
                    values={cur.deny_from || []}
                    onChange={(deny_from) => patch({ deny_from })}
                    placeholder="sender_id，回车添加"
                  />
                </div>
              </div>
            </div>
            <footer>
              <button className="btn danger small" onClick={removeCur}>
                删除
              </button>
              <span className="spacer" />
              {dirty ? <span className="pill warn">未保存</span> : null}
              <button className="btn ghost small" onClick={() => closeDrawer()}>
                取消
              </button>
              <button className="btn primary small" disabled={!dirty && !cur._local} onClick={() => saveDrawer()}>
                保存
              </button>
            </footer>
          </aside>
        </>
      ) : null}
    </div>
  );
}
