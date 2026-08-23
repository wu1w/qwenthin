//! Newline-delimited JSON-RPC 2.0 for `q38 --sidecar`.
//!
//! Transport is **stdio only** (no HTTP, no Unix socket). The dsh plugin is a
//! dumb pipe: it must not run a second tool loop, rewrite Cordis, or speak MCP.
//!
//! `event.append` params use the [`SessionEvent`] schema. JSONL rows are
//! `user` / `assistant` / `tool` / `policy` / `stop` / `session/*`. Live
//! token chunks are `type: "delta"` and are never persisted.

mod dispatch;
mod helpers;
mod rpc;
mod run;
mod session;
mod types;

pub use rpc::{encode_error, encode_notification, encode_response, parse_request_line, serve_rpc};
pub use run::execute_turn;
pub use types::{
    Dispatch, EventSink, PolicyCaps, RpcError, RpcRequest, SidecarOpts, TurnRequest, TurnResult,
    TurnSnapshot, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
};

use std::path::PathBuf;

use serde_json::json;

use crate::channel::{ChannelsConfig, Mailbox};
use crate::family::Family;
use crate::permit::ApprovalMode;
use crate::policy::ThinkPolicy;
use crate::session::{SessionEvent, SessionLog, SessionMode};

use types::JSONRPC;

pub(crate) enum EventStore {
    Log(SessionLog),
    /// In-memory log. `persist=false` (tests / `--print`), or fallback when
    /// `~/.q38-agent/sessions` cannot be created.
    Memory(Vec<SessionEvent>),
}

pub struct SidecarSession {
    pub(crate) opened: bool,
    pub(crate) turn_in_flight: bool,
    pub(crate) session_id: String,
    pub(crate) workspace: PathBuf,
    pub(crate) mode: SessionMode,
    pub(crate) policy: ThinkPolicy,
    pub(crate) caps: PolicyCaps,
    pub(crate) persist: bool,
    pub(crate) effort_locked: bool,
    pub(crate) store: EventStore,
    pub(crate) mailbox: Mailbox,
    pub(crate) model: String,
    pub(crate) family: Family,
    pub(crate) window: u32,
    pub(crate) channels: ChannelsConfig,
    pub(crate) channel: String,
    pub(crate) title: String,
    pub(crate) tools: Vec<serde_json::Value>,
    pub(crate) plan_mode: bool,
    pub(crate) approvals: ApprovalMode,
    pub(crate) low_precision: bool,
}

impl SidecarSession {
    pub fn new(opts: SidecarOpts) -> Self {
        let mut mailbox = Mailbox::default();
        mailbox.busy = opts.busy;
        Self {
            opened: false,
            turn_in_flight: false,
            session_id: opts.session_id,
            workspace: opts.workspace,
            mode: opts.mode,
            policy: opts.policy,
            caps: opts.caps,
            persist: opts.persist,
            effort_locked: opts.effort_locked,
            store: EventStore::Memory(Vec::new()),
            mailbox,
            model: opts.model,
            family: opts.family,
            window: opts.window,
            channels: opts.channels,
            channel: opts.channel,
            title: String::new(),
            tools: Vec::new(),
            plan_mode: false,
            approvals: opts.approvals,
            low_precision: opts.low_precision,
        }
    }

    pub fn set_model(&mut self, name: impl Into<String>) {
        self.model = name.into();
    }

    pub fn plan_mode(&self) -> bool {
        self.plan_mode
    }

    pub fn set_plan_mode(&mut self, on: bool) {
        self.plan_mode = on;
    }

    pub fn approvals(&self) -> ApprovalMode {
        self.approvals
    }

    pub fn set_approvals_mode(&mut self, mode: ApprovalMode) {
        self.approvals = mode;
    }

    pub fn set_busy(&mut self, busy: crate::channel::BusyPolicy) {
        self.mailbox.busy = busy;
    }

    pub fn low_precision(&self) -> bool {
        self.low_precision
    }

    pub fn set_low_precision(&mut self, on: bool) {
        self.low_precision = on;
    }

    pub fn workspace(&self) -> &std::path::Path {
        &self.workspace
    }

    /// Switch the session root. Reloads skills/MCP from the new folder.
    /// Caller must refuse this while a turn is in flight.
    pub fn set_workspace(&mut self, path: PathBuf) {
        self.workspace = path;
        if self.opened {
            self.refresh_surface();
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn window(&self) -> u32 {
        self.window
    }

    pub fn set_window(&mut self, n: u32) {
        if n > 0 {
            self.window = n;
        }
    }

    pub fn set_max_tokens_cap(&mut self, n: u32) {
        if n > 0 {
            self.caps.max_tokens = n;
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn mode(&self) -> SessionMode {
        self.mode
    }

    pub fn turn_in_flight(&self) -> bool {
        self.turn_in_flight
    }

    pub fn begin_turn(&mut self) {
        self.turn_in_flight = true;
    }

    pub fn queued(&self) -> usize {
        self.mailbox.queued()
    }

    pub fn busy(&self) -> crate::channel::BusyPolicy {
        self.mailbox.busy
    }

    pub fn state_json(&self) -> serde_json::Value {
        json!({
            "ok": true,
            "session": self.session_id,
            "title": self.title,
            "workspace": self.workspace.display().to_string(),
            "mode": self.mode.as_str(),
            "model": self.model,
            "channel": self.channel,
            "plan_mode": self.plan_mode,
            "approvals": self.approvals.as_str(),
            "low_precision": self.low_precision,
            "busy": self.mailbox.busy.as_str(),
            "queued": self.mailbox.queued(),
            "steered": self.mailbox.steered(),
            "queue_preview": preview_list(&self.mailbox.peek_queue()),
            "steer_preview": preview_list(&self.mailbox.peek_steer()),
            "turn_in_flight": self.turn_in_flight,
            "window": self.window,
            "usage": crate::slash::UsageRecap::from_events(self.events()).json(),
        })
    }

    pub fn snapshot(&self) -> TurnSnapshot {
        TurnSnapshot {
            session_id: self.session_id.clone(),
            workspace: self.workspace.clone(),
            mode: self.mode,
            policy: self.policy.clone(),
            effort_locked: self.effort_locked,
            model: self.model.clone(),
            plan_mode: self.plan_mode,
            approvals: self.approvals,
            low_precision: self.low_precision,
        }
    }

    pub fn handle(&mut self, req: &RpcRequest) -> Dispatch {
        if req.jsonrpc != JSONRPC {
            return Dispatch::Error(RpcError::invalid_request("jsonrpc must be \"2.0\""));
        }
        match req.method.as_str() {
            "session.open" => self.session_open(req.params.as_ref()),
            "session.list" => self.session_list(req.params.as_ref()),
            "session.resume" => self.session_resume(req.params.as_ref()),
            "session.new" => self.session_new(req.params.as_ref()),
            "session.title" => self.session_title(req.params.as_ref()),
            "session.delete" => self.session_delete(req.params.as_ref()),
            "session.status" => self.session_status(),
            "session.history" => self.session_history(),
            "session.context" => self.session_context(),
            "session.compress" => self.force_compact(None),
            "slash" => self.slash(req.params.as_ref()),
            "turn.start" => self.turn_start(req.params.as_ref()),
            "turn.abort" => self.turn_abort(),
            "turn.queue" => self.turn_queue(req.params.as_ref()),
            "turn.steer" => self.turn_steer(req.params.as_ref()),
            "channel.list" => Dispatch::Result {
                result: json!({"ok": true, "channels": self.channels.list_json()}),
                events: Vec::new(),
            },
            "channel.inbound" => self.channel_inbound(req.params.as_ref()),
            other => Dispatch::Error(RpcError::method_not_found(other)),
        }
    }
}

fn preview_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .take(3)
        .map(|s| {
            let t = s.trim();
            if t.chars().count() > 80 {
                format!("{}…", t.chars().take(80).collect::<String>())
            } else {
                t.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::policy::{Effort, ThinkPolicy};
    use crate::session::{SessionEvent, SessionMode};
    use serde_json::Value;

    use super::*;
    use serde_json::json;

    #[test]
    fn parse_session_open_request_line() {
        let req = parse_request_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"s1","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .expect("parse");
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, Some(json!(1)));
        assert_eq!(req.method, "session.open");
        assert_eq!(req.params.unwrap()["session"], "s1");
    }

    #[test]
    fn serialize_notification_has_no_id() {
        let line = encode_notification(&SessionEvent::user("hello"));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "event.append");
        assert!(v.get("id").is_none());
        assert_eq!(v["params"]["type"], "user");
        assert_eq!(v["params"]["text"], "hello");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn serialize_delta_notification() {
        let line = encode_notification(&SessionEvent::delta_chunk(
            crate::session::DeltaChannel::Reasoning,
            "ab",
        ));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["method"], "event.append");
        assert!(v.get("id").is_none());
        assert_eq!(v["params"]["type"], "delta");
        assert_eq!(v["params"]["channel"], "reasoning");
        assert_eq!(v["params"]["text"], "ab");
        assert_eq!(v["params"]["delta"], true);
        assert!(v["params"].get("reset").is_none());
    }

    #[test]
    fn configured_approvals_survive_new() {
        let mut opts = SidecarOpts::default();
        opts.approvals = ApprovalMode::Yolo;
        let session = SidecarSession::new(opts);
        assert_eq!(session.approvals(), ApprovalMode::Yolo);
        assert_eq!(session.state_json()["approvals"], "yolo");
    }

    #[test]
    fn default_session_approvals_are_ask() {
        let session = SidecarSession::new(SidecarOpts::default());
        assert_eq!(session.approvals(), ApprovalMode::Ask);
        assert_eq!(session.state_json()["approvals"], "ask");
    }

    #[test]
    fn reject_unknown_method() {
        let mut session = SidecarSession::new(SidecarOpts::default());
        let req = parse_request_line(r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#).unwrap();
        match session.handle(&req) {
            Dispatch::Error(err) => {
                assert_eq!(err.code, METHOD_NOT_FOUND);
                assert!(err.message.contains("nope"));
            }
            other => panic!("expected method-not-found, got {other:?}"),
        }
    }

    #[test]
    fn slash_mode_forks_without_second_start_on_disk() {
        let mut session = SidecarSession::new(SidecarOpts::default());
        let open = parse_request_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"s1","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .unwrap();
        match session.handle(&open) {
            Dispatch::Result { .. } => {}
            other => panic!("{other:?}"),
        }
        let slash = parse_request_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"slash","params":{"text":"/mode think"}}"#,
        )
        .unwrap();
        match session.handle(&slash) {
            Dispatch::Result { result, events } => {
                assert_eq!(result["ok"], true);
                assert_eq!(result["mode"], "think");
                assert_eq!(session.snapshot().mode, SessionMode::Think);
                assert!(session.snapshot().effort_locked);
                assert_eq!(session.snapshot().policy.effort, Some(Effort::Medium));
                assert!(events.iter().any(|e| matches!(e, SessionEvent::Fork(_))));
                let starts = events
                    .iter()
                    .filter(|e| matches!(e, SessionEvent::Start(_)))
                    .count();
                assert_eq!(starts, 1);
            }
            other => panic!("expected fork ok, got {other:?}"),
        }
        let think = parse_request_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"slash","params":{"text":"/think low"}}"#,
        )
        .unwrap();
        match session.handle(&think) {
            Dispatch::Result { events, .. } => {
                assert!(matches!(events[0], SessionEvent::Policy(_)));
                assert!(session.snapshot().effort_locked);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cli_locked_think_survives_session_open() {
        let b = crate::policy::ThinkBudget::default();
        let mut opts = SidecarOpts::default();
        opts.policy = ThinkPolicy::effort_with(&b, Effort::Xhigh);
        opts.effort_locked = true;
        let mut session = SidecarSession::new(opts);
        let open = parse_request_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"s-lock","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .unwrap();
        match session.handle(&open) {
            Dispatch::Result { .. } => {}
            other => panic!("{other:?}"),
        }
        let snap = session.snapshot();
        assert_eq!(snap.policy.effort, Some(Effort::Xhigh));
        assert_eq!(snap.policy.max_think_tokens, 4096);
        assert_eq!(snap.policy.max_tokens, 16384);
        assert!(snap.policy.preserve);
        assert!(snap.effort_locked);
    }

    #[test]
    fn cli_fast_survives_session_open() {
        let b = crate::policy::ThinkBudget::default();
        let mut opts = SidecarOpts::default();
        opts.policy = ThinkPolicy::off_with(&b);
        opts.effort_locked = true;
        let mut session = SidecarSession::new(opts);
        let open = parse_request_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"s-fast","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .unwrap();
        match session.handle(&open) {
            Dispatch::Result { .. } => {}
            other => panic!("{other:?}"),
        }
        let snap = session.snapshot();
        assert!(!snap.policy.enabled);
        assert!(snap.policy.effort.is_none());
        assert_eq!(snap.policy.max_tokens, 8192);
        assert!(snap.effort_locked);
    }

    #[test]
    fn slash_xhigh_uses_think_mode_max_tokens() {
        let mut session = open_mem();
        let think = parse_request_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"slash","params":{"text":"/think xhigh"}}"#,
        )
        .unwrap();
        match session.handle(&think) {
            Dispatch::Result { .. } => {}
            other => panic!("{other:?}"),
        }
        let snap = session.snapshot();
        assert_eq!(snap.policy.effort, Some(Effort::Xhigh));
        assert_eq!(snap.policy.max_tokens, 16384);
        assert_eq!(snap.policy.max_think_tokens, 4096);
        assert!(snap.effort_locked);
    }

    fn open_mem() -> SidecarSession {
        let mut session = SidecarSession::new(SidecarOpts::default());
        let open = parse_request_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"s1","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .unwrap();
        match session.handle(&open) {
            Dispatch::Result { .. } => {}
            other => panic!("{other:?}"),
        }
        session
    }

    #[test]
    fn slash_help_is_local() {
        let mut session = open_mem();
        let req = parse_request_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"slash","params":{"text":"/help"}}"#,
        )
        .unwrap();
        match session.handle(&req) {
            Dispatch::Result { result, events } => {
                assert!(events.is_empty());
                let text = result["text"].as_str().unwrap();
                assert!(text.contains("/think"), "{text}");
                assert!(text.contains("/compress"), "{text}");
                assert!(text.contains("/busy"), "{text}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn busy_queue_and_interrupt() {
        let mut session = open_mem();
        let start = parse_request_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"turn.start","params":{"prompt":"first"}}"#,
        )
        .unwrap();
        assert!(matches!(session.handle(&start), Dispatch::TurnStart { .. }));
        let again = parse_request_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"turn.start","params":{"prompt":"second"}}"#,
        )
        .unwrap();
        assert!(matches!(session.handle(&again), Dispatch::Abort));
        assert!(session.has_redirect());
        assert_eq!(session.pop_follow_up().as_deref(), Some("second"));

        let mut session = open_mem();
        let busy = parse_request_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"slash","params":{"text":"/busy queue"}}"#,
        )
        .unwrap();
        match session.handle(&busy) {
            Dispatch::Result { result, .. } => {
                assert!(result["text"].as_str().unwrap().contains("queue"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(session.handle(&start), Dispatch::TurnStart { .. }));
        match session.handle(&again) {
            Dispatch::Result { result, .. } => {
                assert_eq!(result["queued"], true);
                assert_eq!(result["n"], 1);
            }
            other => panic!("{other:?}"),
        }
        let steer = parse_request_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"turn.steer","params":{"text":"focus auth"}}"#,
        )
        .unwrap();
        match session.handle(&steer) {
            Dispatch::Result { result, .. } => assert_eq!(result["steered"], true),
            other => panic!("{other:?}"),
        }
        let st = session.state_json();
        assert_eq!(st["queued"], 1);
        assert_eq!(st["steered"], 1);
        assert_eq!(st["queue_preview"][0], "second");
        assert_eq!(st["steer_preview"][0], "focus auth");
    }

    #[test]
    fn channel_and_session_list() {
        let mut session = open_mem();
        let ch = parse_request_line(r#"{"jsonrpc":"2.0","id":2,"method":"channel.list"}"#).unwrap();
        match session.handle(&ch) {
            Dispatch::Result { result, .. } => {
                let rows = result["channels"].as_array().unwrap();
                assert!(rows.iter().any(|r| r["id"] == "sidecar"));
                assert!(rows.iter().any(|r| r["id"] == "cli"));
            }
            other => panic!("{other:?}"),
        }
        let inbound = parse_request_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"channel.inbound","params":{"channel":"console","text":"from im"}}"#,
        )
        .unwrap();
        match session.handle(&inbound) {
            Dispatch::TurnStart { prompt, .. } => assert_eq!(prompt, "from im"),
            other => panic!("{other:?}"),
        }
        let listed =
            parse_request_line(r#"{"jsonrpc":"2.0","id":4,"method":"session.list"}"#).unwrap();
        match session.handle(&listed) {
            Dispatch::Result { result, .. } => {
                assert_eq!(result["ok"], true);
                assert!(result["sessions"].is_array());
            }
            other => panic!("{other:?}"),
        }
        let stop = parse_request_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"slash","params":{"text":"/stop"}}"#,
        )
        .unwrap();
        match session.handle(&stop) {
            Dispatch::AbortClear { cleared } => assert_eq!(cleared, 0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn turn_start_keeps_image_parts() {
        let mut session = open_mem();
        let start = parse_request_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"turn.start","params":{"prompt":"look","content_parts":[{"type":"image","image_url":"data:image/png;base64,xx"}]}}"#,
        )
        .unwrap();
        match session.handle(&start) {
            Dispatch::TurnStart { prompt, parts } => {
                assert_eq!(prompt, "look");
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].kind, crate::media::MediaKind::Image);
                assert!(parts[0].url.starts_with("data:image/png"));
            }
            other => panic!("{other:?}"),
        }

        let mut session = open_mem();
        let inbound = parse_request_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"channel.inbound","params":{"channel":"console","content_parts":[{"type":"text","text":"see"},{"type":"image","url":"https://example.com/a.png"}]}}"#,
        )
        .unwrap();
        match session.handle(&inbound) {
            Dispatch::TurnStart { prompt, parts } => {
                assert_eq!(prompt, "see");
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].url, "https://example.com/a.png");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn serve_rpc_open_and_local_slash() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (client, server) = tokio::io::duplex(64 * 1024);
        let (cr, mut cw) = tokio::io::split(client);
        let (sr, sw) = tokio::io::split(server);
        let session = SidecarSession::new(SidecarOpts::default());
        let serve = tokio::spawn(async move {
            serve_rpc(BufReader::new(sr), sw, session, |_req| async {
                TurnResult::fail("unexpected turn")
            })
            .await
        });

        let mut lines = BufReader::new(cr).lines();
        cw.write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"rpc1","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .await
        .unwrap();
        cw.write_all(b"\n").await.unwrap();
        cw.flush().await.unwrap();

        let mut saw_open = false;
        for _ in 0..8 {
            let line = lines.next_line().await.unwrap().expect("open reply");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == 1 && v["result"]["ok"] == true {
                saw_open = true;
                break;
            }
        }
        assert!(saw_open, "missing session.open result");

        cw.write_all(br#"{"jsonrpc":"2.0","id":2,"method":"slash","params":{"text":"/help"}}"#)
            .await
            .unwrap();
        cw.write_all(b"\n").await.unwrap();
        cw.flush().await.unwrap();

        let mut help = String::new();
        for _ in 0..8 {
            let line = lines.next_line().await.unwrap().expect("slash reply");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == 2 {
                help = v["result"]["text"].as_str().unwrap_or("").to_string();
                break;
            }
        }
        assert!(help.contains("/compress"), "{help}");
        let _ = cw.shutdown().await;
        drop(cw);
        drop(lines);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), serve).await;
    }

    #[tokio::test]
    async fn serve_rpc_streams_tool_before_turn_result() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (client, server) = tokio::io::duplex(64 * 1024);
        let (cr, mut cw) = tokio::io::split(client);
        let (sr, sw) = tokio::io::split(server);
        let session = SidecarSession::new(SidecarOpts::default());
        let serve = tokio::spawn(async move {
            serve_rpc(BufReader::new(sr), sw, session, |req| async move {
                req.emit
                    .append(crate::session::SessionEvent::tool("c1", "bash", "pong"));
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                req.emit
                    .append(crate::session::SessionEvent::assistant("done", "", None));
                req.emit.append(crate::session::SessionEvent::stop("stop"));
                TurnResult {
                    text: "done".into(),
                    streamed: true,
                    ..TurnResult::default()
                }
            })
            .await
        });

        let mut lines = BufReader::new(cr).lines();
        cw.write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"session.open","params":{"session":"rpc2","workspace":"/tmp/ws","mode":"agent"}}"#,
        )
        .await
        .unwrap();
        cw.write_all(b"\n").await.unwrap();
        cw.flush().await.unwrap();

        for _ in 0..8 {
            let line = lines.next_line().await.unwrap().expect("open reply");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == 1 && v["result"]["ok"] == true {
                break;
            }
        }

        cw.write_all(br#"{"jsonrpc":"2.0","id":2,"method":"turn.start","params":{"prompt":"go"}}"#)
            .await
            .unwrap();
        cw.write_all(b"\n").await.unwrap();
        cw.flush().await.unwrap();

        let mut saw_tool = false;
        let mut saw_result = false;
        let mut tool_before_result = false;
        for _ in 0..24 {
            let Some(line) = lines.next_line().await.unwrap() else {
                break;
            };
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["method"] == "event.append" && v["params"]["type"] == "tool" {
                saw_tool = true;
                if !saw_result {
                    tool_before_result = true;
                }
            }
            if v["id"] == 2 && v["result"]["ok"] == true {
                saw_result = true;
                break;
            }
        }
        assert!(saw_tool, "tool event never streamed");
        assert!(
            tool_before_result,
            "tool arrived with or after the RPC result"
        );
        assert!(saw_result, "missing turn.start result");
        let _ = cw.shutdown().await;
        drop(cw);
        drop(lines);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), serve).await;
    }
}
