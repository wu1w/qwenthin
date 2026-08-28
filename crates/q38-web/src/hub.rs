use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use q38_loop::clarify::{ClarifyDecision, ClarifyHub, ClarifyRequest};
use q38_loop::config::Config;
use q38_loop::media::MediaPart;
use q38_loop::permit::{PermitDecision, PermitHub, PermitRequest};
use q38_loop::session::{DeltaChannel, SessionEvent};
use q38_loop::sidecar::{
    execute_turn, Dispatch, EventSink, RpcRequest, SidecarSession, TurnRequest, TurnResult,
};
use q38_loop::CancelFlag;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::cron::{heartbeat_prompt, now_s, CronStore};

pub type Bus = broadcast::Sender<Value>;

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Mutex<Inner>>,
    pub bus: Bus,
}

pub struct Inner {
    pub session: SidecarSession,
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub live: Option<LiveTurn>,
    pub permit: PermitHub,
    pub pending: VecDeque<PendingPermit>,
    pub permit_seq: u64,
    pub clarify: ClarifyHub,
    pub pending_clarify: VecDeque<PendingClarify>,
    pub clarify_seq: u64,
    pub agents_md: bool,
    pub agents_md_head: bool,
    pub cron: CronStore,
    /// endpoint id → 运行状态,watcher 重建、serve 任务退出时更新
    pub channel_runtime: HashMap<String, ChannelRuntime>,
    /// watcher 代际,防止被 abort 的旧 serve 任务写入过期状态
    channel_gen: u64,
    ev_tx: mpsc::UnboundedSender<SessionEvent>,
    bus: Bus,
}

/// 单个频道 endpoint 的运行状态,GET /api/channels 的 `runtime` 字段
#[derive(Clone)]
pub struct ChannelRuntime {
    pub state: &'static str, // running | error | no_credentials | off
    pub detail: Option<String>,
}

impl ChannelRuntime {
    pub fn json(&self) -> Value {
        json!({"state": self.state, "detail": self.detail})
    }
}

pub struct LiveTurn {
    pub cancel: CancelFlag,
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
}

pub struct PendingPermit {
    pub id: u64,
    pub req: PermitRequest,
}

impl PendingPermit {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "tool": self.req.ask.tool,
            "preview": self.req.ask.preview,
        })
    }
}

pub struct PendingClarify {
    pub id: u64,
    pub req: ClarifyRequest,
}

impl PendingClarify {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.req.ask.title,
            "prompt": self.req.ask.prompt,
            "options": self.req.ask.options.iter().map(|o| json!({
                "id": o.id,
                "label": o.label,
            })).collect::<Vec<_>>(),
        })
    }
}

impl AppState {
    pub fn new(
        session: SidecarSession,
        cfg: Config,
        cfg_path: PathBuf,
        agents_md: bool,
        agents_md_head: bool,
    ) -> Result<Self> {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SessionEvent>();
        // Token deltas are tiny but frequent. 512 filled up around ~30 tool hops
        // on a slow JSON/React client; RecvError::Lagged then pushed the whole
        // history over WS and froze the UI.
        let (bus, _) = broadcast::channel(8192);
        let bus_fwd = bus.clone();
        tokio::spawn(async move {
            forward_session_events(ev_rx, bus_fwd).await;
        });
        let approvals = session.approvals();
        let (permit, permit_rx) = PermitHub::pair(approvals);
        let (clarify, clarify_rx) = ClarifyHub::pair();
        let inner = Arc::new(Mutex::new(Inner {
            session,
            cfg,
            cfg_path,
            live: None,
            permit,
            pending: VecDeque::new(),
            permit_seq: 0,
            clarify,
            pending_clarify: VecDeque::new(),
            clarify_seq: 0,
            agents_md,
            agents_md_head,
            cron: CronStore::load(),
            channel_runtime: HashMap::new(),
            channel_gen: 0,
            ev_tx,
            bus: bus.clone(),
        }));
        let pending_state = inner.clone();
        let bus_p = bus.clone();
        tokio::spawn(async move {
            let mut rx = permit_rx;
            while let Some(req) = rx.recv().await {
                let mut g = pending_state.lock().await;
                g.permit_seq += 1;
                let id = g.permit_seq;
                let p = PendingPermit { id, req };
                let payload = p.json();
                let was_empty = g.pending.is_empty();
                g.pending.push_back(p);
                // Modal is FIFO: only announce a new ask when nothing else is waiting.
                // Later items surface via decide_permit → permit.ask(front).
                if was_empty {
                    let _ = bus_p.send(notify("permit.ask", payload));
                }
            }
        });
        let pending_clarify = inner.clone();
        let bus_c = bus.clone();
        tokio::spawn(async move {
            let mut rx = clarify_rx;
            while let Some(req) = rx.recv().await {
                let mut g = pending_clarify.lock().await;
                g.clarify_seq += 1;
                let id = g.clarify_seq;
                let p = PendingClarify { id, req };
                let payload = p.json();
                let was_empty = g.pending_clarify.is_empty();
                g.pending_clarify.push_back(p);
                if was_empty {
                    let _ = bus_c.send(notify("clarify.ask", payload));
                }
            }
        });
        Ok(Self { inner, bus })
    }

    pub fn spawn_background(&self) {
        spawn_channel_watch(self.inner.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let mut g = inner.lock().await;
                g.cron = CronStore::reload(g.session.workspace(), &g.cron);
                if g.live.is_some() || g.session.turn_in_flight() {
                    continue;
                }
                let now = now_s();
                let due: Vec<String> = g.cron.due(now);
                if let Some(id) = due.into_iter().next() {
                    if let Some(prompt) = g.cron.mark(&id, now) {
                        let _ = g.cron.save_with_workspace(g.session.workspace());
                        start_turn(
                            &mut g,
                            inner.clone(),
                            prompt,
                            Vec::new(),
                            Some(CronRetry::Job { id }),
                        );
                        continue;
                    }
                }
                if g.cron.heartbeat_due(now) {
                    let prompt = heartbeat_prompt(&g.cron, g.session.workspace());
                    g.cron.heartbeat.last_run = Some(now);
                    let _ = g.cron.save();
                    start_turn(
                        &mut g,
                        inner.clone(),
                        prompt,
                        Vec::new(),
                        Some(CronRetry::Heartbeat),
                    );
                }
            }
        });
    }

    pub async fn rpc(&self, method: &str, params: Option<Value>) -> Value {
        let mut g = self.inner.lock().await;
        if method == "slash" {
            let text = params
                .as_ref()
                .and_then(|p| p.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if text == "/reload" {
                if let Ok(disk) = Config::load_from(&g.cfg_path) {
                    g.cfg = disk;
                }
                g.session.refresh_surface();
            }
        }
        let before = g.session.session_id().to_string();
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        let dispatch = g.session.handle(&req);
        let out = apply_dispatch(&mut g, self.inner.clone(), dispatch);
        // 聊天 /approvals 只改 session+写盘;这里回读同步 PermitHub,闸门才真正生效
        g.permit.set_mode(g.session.approvals());
        if g.session.session_id() != before {
            let _ = g.bus.send(notify(
                "history.replace",
                history_replace_params(g.session.events(), g.session.session_id(), true),
            ));
        }
        out
    }

    pub async fn decide_permit(&self, id: u64, decision: PermitDecision) -> Result<Value, String> {
        let mut g = self.inner.lock().await;
        let idx = g
            .pending
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| "no matching permit".to_string())?;
        let p = g.pending.remove(idx).expect("idx");
        if decision == PermitDecision::Always {
            g.permit.remember(&p.req.ask.tool);
        }
        let _ = p.req.reply.send(decision);
        if let Some(next) = g.pending.front() {
            let _ = g.bus.send(notify("permit.ask", next.json()));
        } else {
            let _ = g.bus.send(notify("permit.clear", json!(null)));
        }
        Ok(json!({"ok": true, "id": id}))
    }

    pub async fn decide_clarify(
        &self,
        id: u64,
        decision: ClarifyDecision,
    ) -> Result<Value, String> {
        let mut g = self.inner.lock().await;
        let idx = g
            .pending_clarify
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| "no matching clarify".to_string())?;
        let p = g.pending_clarify.remove(idx).expect("idx");
        let _ = p.req.reply.send(decision);
        if let Some(next) = g.pending_clarify.front() {
            let _ = g.bus.send(notify("clarify.ask", next.json()));
        } else {
            let _ = g.bus.send(notify("clarify.clear", json!(null)));
        }
        Ok(json!({"ok": true, "id": id}))
    }
}

fn extra_len(ep: &q38_loop::ChannelEndpoint, keys: &[&str]) -> usize {
    keys.iter()
        .filter_map(|k| ep.extra.get(*k))
        .map(|s| s.trim().len())
        .find(|n| *n > 0)
        .unwrap_or(0)
}

fn extra_has(ep: &q38_loop::ChannelEndpoint, keys: &[&str]) -> bool {
    extra_len(ep, keys) > 0
}

/// True when this process can start a live client for `ep`.
fn endpoint_runnable(ep: &q38_loop::ChannelEndpoint) -> bool {
    if !ep.enabled {
        return false;
    }
    match ep.kind.to_ascii_lowercase().as_str() {
        "telegram" => extra_has(ep, &["bot_token", "token"]),
        "webhook" | "http" | "console" => true,
        "qq" => extra_has(ep, &["app_id"]) && extra_has(ep, &["client_secret"]),
        "wechat" => extra_has(ep, &["bot_token", "token"]),
        "wecom" => extra_has(ep, &["bot_id"]) && extra_has(ep, &["secret"]),
        "dingtalk" => extra_has(ep, &["client_id"]) && extra_has(ep, &["client_secret"]),
        "feishu" => {
            extra_has(ep, &["app_id", "client_id"])
                && extra_has(ep, &["app_secret", "client_secret"])
        }
        _ => false,
    }
}

/// 该 kind 的适配器是否在本进程内(与 [`endpoint_runnable`] 的分支一致)。
fn endpoint_in_process(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "telegram"
            | "webhook"
            | "http"
            | "console"
            | "qq"
            | "wechat"
            | "wecom"
            | "dingtalk"
            | "feishu"
    )
}

/// 不含运行结果的静态分类:off / no_credentials / running(即将启动)。
pub fn endpoint_static_runtime(ep: &q38_loop::ChannelEndpoint) -> ChannelRuntime {
    let state = if !ep.enabled || !endpoint_in_process(&ep.kind) {
        "off"
    } else if !endpoint_runnable(ep) {
        "no_credentials"
    } else {
        "running"
    };
    ChannelRuntime {
        state,
        detail: None,
    }
}

/// 字符安全截断(错误文本进 runtime.detail 前压到 300 字符)。
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// 每个 endpoint 整体序列化后拼指纹:allow/deny、策略、bind、凭据内容任何
/// 变化都会触发 watcher 重启(旧版只看 id/kind/enabled/凭据长度)。
fn channels_fingerprint(cfg: &Config) -> String {
    let mut rows: Vec<String> = cfg
        .channels
        .endpoints
        .iter()
        .map(|e| serde_json::to_string(e).unwrap_or_default())
        .collect();
    rows.sort();
    rows.join("|")
}

fn spawn_channel_watch(inner: Arc<Mutex<Inner>>) {
    tokio::spawn(async move {
        let mut last = String::new();
        let mut jobs: Vec<JoinHandle<()>> = Vec::new();
        loop {
            let (sig, cfg, workspace) = {
                let g = inner.lock().await;
                (
                    channels_fingerprint(&g.cfg),
                    g.cfg.clone(),
                    g.session.workspace().to_path_buf(),
                )
            };
            if sig != last {
                last = sig.clone();
                for j in jobs.drain(..) {
                    j.abort();
                }
                // 重建全量运行状态表并换代,旧任务的退出回写会被代际拦下
                let gen = {
                    let mut g = inner.lock().await;
                    g.channel_gen += 1;
                    g.channel_runtime = cfg
                        .channels
                        .endpoints
                        .iter()
                        .map(|e| (e.id.clone(), endpoint_static_runtime(e)))
                        .collect();
                    g.channel_gen
                };
                for ep in cfg
                    .channels
                    .endpoints
                    .iter()
                    .filter(|e| endpoint_runnable(e))
                {
                    let ep = ep.clone();
                    let cfg = cfg.clone();
                    let workspace = workspace.clone();
                    let inner_ep = inner.clone();
                    eprintln!("q38 {}: starting in-process client ({})", ep.kind, ep.id);
                    jobs.push(tokio::spawn(async move {
                        let id = ep.id.clone();
                        let kind = ep.kind.clone();
                        q38_loop::channel::keep_client_watched(
                            &kind,
                            &id,
                            {
                                let cfg = cfg.clone();
                                let workspace = workspace.clone();
                                let ep = ep.clone();
                                move || {
                                    q38_loop::channel::serve_endpoint(
                                        cfg.clone(),
                                        workspace.clone(),
                                        ep.clone(),
                                    )
                                }
                            },
                            {
                                let inner_ep = inner_ep.clone();
                                let id = id.clone();
                                move |st| {
                                    let inner_ep = inner_ep.clone();
                                    let id = id.clone();
                                    async move {
                                        let (state, detail) = match st {
                                            q38_loop::channel::ClientWatch::Running => {
                                                ("running", None)
                                            }
                                            q38_loop::channel::ClientWatch::Retry {
                                                detail,
                                                wait_secs,
                                            } => (
                                                "error",
                                                Some(clip_chars(
                                                    &format!("retry in {wait_secs}s: {detail}"),
                                                    300,
                                                )),
                                            ),
                                            q38_loop::channel::ClientWatch::Fatal { detail } => {
                                                ("error", Some(clip_chars(&detail, 300)))
                                            }
                                        };
                                        let mut g = inner_ep.lock().await;
                                        if g.channel_gen == gen {
                                            g.channel_runtime
                                                .insert(id, ChannelRuntime { state, detail });
                                        }
                                    }
                                }
                            },
                        )
                        .await;
                    }));
                }
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    });
}

pub fn notify(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Focused-session transcript. `reset: true` means the console must drop the
/// on-screen events (session.new / resume), not keep a "fresher" transcript
/// from the previous chat.
fn history_replace_params(events: &[SessionEvent], session: &str, reset: bool) -> Value {
    json!({
        "events": console_events(events),
        "session": session,
        "reset": reset,
    })
}

/// Session events for the console: drop inline `data:` URLs so hello / history
/// cannot replay screenshots as multi-megabyte JSON.
pub fn console_events(events: &[SessionEvent]) -> Value {
    let mut v = serde_json::to_value(events).unwrap_or_else(|_| json!([]));
    redact_data_uris(&mut v);
    v
}

fn redact_data_uris(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(url)) = map.get_mut("url") {
                if url.starts_with("data:") {
                    url.clear();
                }
            }
            for child in map.values_mut() {
                redact_data_uris(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                redact_data_uris(child);
            }
        }
        _ => {}
    }
}

const DELTA_FLUSH_MS: u64 = 32;
const DELTA_FLUSH_CHARS: usize = 4096;

/// Merge consecutive token deltas so one WS frame covers ~32ms of tokens.
async fn forward_session_events(mut ev_rx: mpsc::UnboundedReceiver<SessionEvent>, bus: Bus) {
    let mut reason = String::new();
    let mut content = String::new();
    loop {
        let pending = !reason.is_empty() || !content.is_empty();
        let next = if pending {
            tokio::select! {
                ev = ev_rx.recv() => ev,
                _ = tokio::time::sleep(Duration::from_millis(DELTA_FLUSH_MS)) => {
                    flush_deltas(&mut reason, &mut content, &bus);
                    continue;
                }
            }
        } else {
            ev_rx.recv().await
        };
        let Some(ev) = next else {
            flush_deltas(&mut reason, &mut content, &bus);
            break;
        };
        match ev {
            SessionEvent::Delta(d) if d.reset => {
                flush_deltas(&mut reason, &mut content, &bus);
                let _ = bus.send(notify("event.append", json!(SessionEvent::Delta(d))));
            }
            SessionEvent::Delta(d) => {
                match d.channel {
                    DeltaChannel::Reasoning => reason.push_str(&d.text),
                    DeltaChannel::Content => content.push_str(&d.text),
                }
                if reason.len() + content.len() >= DELTA_FLUSH_CHARS {
                    flush_deltas(&mut reason, &mut content, &bus);
                }
            }
            other => {
                flush_deltas(&mut reason, &mut content, &bus);
                let _ = bus.send(notify("event.append", json!(other)));
            }
        }
    }
}

fn flush_deltas(reason: &mut String, content: &mut String, bus: &Bus) {
    if !reason.is_empty() {
        let text = std::mem::take(reason);
        let _ = bus.send(notify(
            "event.append",
            json!(SessionEvent::delta_chunk(DeltaChannel::Reasoning, text)),
        ));
    }
    if !content.is_empty() {
        let text = std::mem::take(content);
        let _ = bus.send(notify(
            "event.append",
            json!(SessionEvent::delta_chunk(DeltaChannel::Content, text)),
        ));
    }
}

pub fn apply_dispatch(inner: &mut Inner, shared: Arc<Mutex<Inner>>, dispatch: Dispatch) -> Value {
    match dispatch {
        Dispatch::Result { result, events } => {
            for e in &events {
                let _ = inner.bus.send(notify("event.append", json!(e)));
            }
            push_state(inner);
            result
        }
        Dispatch::Error(err) => json!({"ok": false, "error": err.message, "code": err.code}),
        Dispatch::TurnStart { prompt, parts } => {
            start_turn(inner, shared, prompt, parts, None);
            json!({"ok": true, "started": true})
        }
        Dispatch::Abort => {
            if let Some(live) = &inner.live {
                live.cancel.cancel();
            }
            clear_pending_permits(inner);
            clear_pending_clarifies(inner);
            push_state(inner);
            json!({"ok": true, "aborted": true})
        }
        Dispatch::AbortClear { cleared } => {
            if let Some(live) = &inner.live {
                live.cancel.cancel();
            }
            clear_pending_permits(inner);
            clear_pending_clarifies(inner);
            push_state(inner);
            json!({"ok": true, "aborted": true, "cleared": cleared})
        }
    }
}

/// abort/panic 后丢弃全部待审批(drop reply 即隐式 deny)并让前端关掉弹窗,
/// 否则死弹窗压住 FIFO,后续审批永远弹不出来。
fn clear_pending_permits(inner: &mut Inner) {
    if inner.pending.is_empty() {
        return;
    }
    inner.pending.clear();
    let _ = inner.bus.send(notify("permit.clear", json!(null)));
}

fn clear_pending_clarifies(inner: &mut Inner) {
    if inner.pending_clarify.is_empty() {
        return;
    }
    for p in inner.pending_clarify.drain(..) {
        let _ = p.req.reply.send(ClarifyDecision::Skip);
    }
    let _ = inner.bus.send(notify("clarify.clear", json!(null)));
}

pub(crate) fn push_state(inner: &Inner) {
    let _ = inner.bus.send(notify("state", inner.session.state_json()));
}

/// Retry bookkeeping for a cron-triggered turn. On error we defer the next
/// fire by `CRON_RETRY_DELAY_S` so a down LLM cannot write a stop-storm
/// every 1s tick, without sitting out the full `interval_s`.
pub(crate) enum CronRetry {
    Job { id: String },
    Heartbeat,
}

/// turn 任务 panic 兜底:正常路径 disarm;panic 时 Drop 收尾 turn、清 live 与
/// 待审批,否则 `live` 永远是 Some,整个控制台永久 busy 到重启。
struct TurnPanicGuard {
    shared: Arc<Mutex<Inner>>,
    armed: bool,
}

impl Drop for TurnPanicGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut g) = self.shared.try_lock() {
            cleanup_after_panic(&mut g);
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let shared = self.shared.clone();
            handle.spawn(async move {
                let mut g = shared.lock().await;
                cleanup_after_panic(&mut g);
            });
        }
    }
}

fn cleanup_after_panic(g: &mut Inner) {
    // finish_turn 顺带把 turn_in_flight 复位,并落一条错误 stop
    let extra = g
        .session
        .finish_turn(&TurnResult::fail("internal error: turn task panicked"));
    for e in extra {
        let _ = g.bus.send(notify("event.append", json!(e)));
    }
    g.live = None;
    clear_pending_permits(g);
    clear_pending_clarifies(g);
    push_state(g);
}

pub fn start_turn(
    inner: &mut Inner,
    shared: Arc<Mutex<Inner>>,
    prompt: String,
    parts: Vec<MediaPart>,
    cron_retry: Option<CronRetry>,
) {
    inner.session.maybe_autotitle(&prompt);
    inner.session.begin_turn();
    let user = SessionEvent::user(&prompt);
    let _ = inner.bus.send(notify("event.append", json!(user)));
    let cancel = CancelFlag::new();
    let req = TurnRequest {
        prompt,
        parts,
        snapshot: inner.session.snapshot(),
        cancel: cancel.clone(),
        emit: EventSink::new(inner.ev_tx.clone()),
        messages: Vec::new(),
        steer: inner.session.steer_slot(),
        persist: true,
        permit: Some(inner.permit.with_session(inner.session.session_id())),
        clarify: Some(inner.clarify.with_session(inner.session.session_id())),
    };
    let cfg = inner.cfg.clone();
    let agents_md = inner.agents_md;
    let agents_md_head = inner.agents_md_head;
    let join = tokio::spawn(async move {
        let mut guard = TurnPanicGuard {
            shared: shared.clone(),
            armed: true,
        };
        let result = execute_turn(cfg, agents_md, agents_md_head, req).await;
        let mut g = shared.lock().await;
        guard.armed = false;
        let extra = g.session.finish_turn(&result);
        let extra_has_stop = extra.iter().any(|e| matches!(e, SessionEvent::Stop(_)));
        for e in extra {
            let _ = g.bus.send(notify("event.append", json!(e)));
        }
        g.live = None;
        if let Some(err) = result.error {
            match cron_retry {
                Some(CronRetry::Job { id }) => {
                    g.cron
                        .defer_job(&id, now_s(), crate::cron::CRON_RETRY_DELAY_S);
                    let _ = g.cron.save_with_workspace(g.session.workspace());
                }
                Some(CronRetry::Heartbeat) => {
                    g.cron
                        .defer_heartbeat(now_s(), crate::cron::CRON_RETRY_DELAY_S);
                    let _ = g.cron.save();
                }
                None => {}
            }
            if !extra_has_stop {
                let _ = g
                    .bus
                    .send(notify("event.append", json!(SessionEvent::stop(err))));
            }
        }
        // finish_turn 已 reload 落盘事件;整体重播一次,中途刷新/WS Lagged
        // 的客户端才能补回本 turn 的前半段
        let _ = g.bus.send(notify(
            "history.replace",
            history_replace_params(g.session.events(), g.session.session_id(), false),
        ));
        let _ = g.bus.send(notify("state", g.session.state_json()));
        if let Some(next) = g.session.pop_follow_up() {
            start_turn(&mut g, shared.clone(), next, Vec::new(), None);
        }
    });
    inner.live = Some(LiveTurn { cancel, join });
    push_state(inner);
}

pub fn redact_key(key: &str) -> String {
    let t = key.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.chars().count() <= 4 {
        return "****".into();
    }
    // Char-safe tail: byte slicing panics on non-ASCII keys.
    let tail: String = t
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

#[cfg(test)]
mod tests {
    use super::{
        channels_fingerprint, console_events, endpoint_static_runtime, history_replace_params,
        redact_key,
    };
    use q38_loop::config::Config;
    use q38_loop::session::{SessionEvent, StoredMedia};
    use q38_loop::ChannelEndpoint;

    #[test]
    fn redact_ascii_and_unicode() {
        assert_eq!(redact_key(""), "");
        assert_eq!(redact_key("ab"), "****");
        assert_eq!(redact_key("sk-abcdef1234"), "****1234");
        // Non-ASCII key: the old byte slice `&t[t.len()-4..]` panicked.
        assert_eq!(redact_key("密钥-abcdef"), "****cdef");
    }

    fn cfg_with(ep: ChannelEndpoint) -> Config {
        let mut cfg = Config::default();
        cfg.channels.endpoints = vec![ep];
        cfg
    }

    #[test]
    fn fingerprint_sees_policy_and_credential_changes() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            ..ChannelEndpoint::default()
        };
        ep.extra.insert("bot_token".into(), "123:abc".into());
        let base = channels_fingerprint(&cfg_with(ep.clone()));

        let mut allow = ep.clone();
        allow.allow_from = vec!["alice".into()];
        assert_ne!(base, channels_fingerprint(&cfg_with(allow)), "allow_from");

        let mut token = ep.clone();
        token.extra.insert("bot_token".into(), "123:abd".into());
        assert_ne!(base, channels_fingerprint(&cfg_with(token)), "等长新凭据");

        let mut policy = ep.clone();
        policy.group_policy = "closed".into();
        assert_ne!(
            base,
            channels_fingerprint(&cfg_with(policy)),
            "group_policy"
        );
    }

    #[test]
    fn static_runtime_classification() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: false,
            ..ChannelEndpoint::default()
        };
        assert_eq!(endpoint_static_runtime(&ep).state, "off");
        ep.enabled = true;
        assert_eq!(endpoint_static_runtime(&ep).state, "no_credentials");
        ep.extra.insert("bot_token".into(), "123:abc".into());
        assert_eq!(endpoint_static_runtime(&ep).state, "running");
        ep.kind = "discord".into(); // 不在进程内的平台
        assert_eq!(endpoint_static_runtime(&ep).state, "off");
    }

    #[test]
    fn console_events_drop_inline_data_uris() {
        let ev = SessionEvent::tool("c1", "view", "Image loaded").with_media(vec![StoredMedia {
            kind: "image".into(),
            mime: "image/png".into(),
            url: "data:image/png;base64,AAAA".into(),
        }]);
        let path = SessionEvent::tool("c2", "view", "ok").with_media(vec![StoredMedia {
            kind: "image".into(),
            mime: "image/png".into(),
            url: ".q38/generated/shot.png".into(),
        }]);
        let v = console_events(&[ev, path]);
        assert_eq!(v[0]["media"][0]["url"], "");
        assert_eq!(v[1]["media"][0]["url"], ".q38/generated/shot.png");
    }

    #[test]
    fn session_switch_history_replace_sets_reset() {
        let ev = SessionEvent::user("hi");
        let reset = history_replace_params(&[ev.clone()], "sess-new", true);
        assert_eq!(reset["reset"], true);
        assert_eq!(reset["session"], "sess-new");
        assert_eq!(reset["events"][0]["type"], "user");
        let keep = history_replace_params(&[ev], "sess-new", false);
        assert_eq!(keep["reset"], false);
    }
}
