//! Catalog, fork, compact, undo, and JSONL store for sidecar sessions.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::Config;
use crate::memory::MemoryStore;
use crate::policy::ThinkPolicy;
use crate::session::catalog;
use crate::session::{new_session_id, SessionEvent, SessionLog, SessionMode, SessionStart};
use crate::skills::SkillCatalog;
use crate::slash::{
    self, compact_reply, context_text, history_text, model_text, sessions_text, status_text,
    ModelAction,
};
use crate::tools_schema::code_tools;

use super::helpers::{
    make_start, parse_params, sidecar_agent_surface, SessionOpenParams, SessionSearchParams,
};
use super::types::{Dispatch, RpcError, TurnResult};
use super::{EventStore, SidecarSession};

impl SidecarSession {
    pub(crate) fn session_open(&mut self, params: Option<&Value>) -> Dispatch {
        let p: SessionOpenParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        if let Some(id) = p.session.filter(|s| !s.is_empty()) {
            self.session_id = id;
        }
        if let Some(ws) = p.workspace.filter(|s| !s.is_empty()) {
            self.workspace = PathBuf::from(ws);
        }
        if let Some(raw) = p.mode {
            match raw.parse::<SessionMode>() {
                Ok(mode) => {
                    self.mode = mode;
                    let budget = self.caps.think_budget();
                    if self.effort_locked {
                        match mode {
                            SessionMode::Chat => {
                                self.policy = ThinkPolicy::off_with(&budget);
                            }
                            SessionMode::Think => {
                                self.policy.preserve = true;
                                self.policy.max_tokens =
                                    budget.think_mode_max_tokens.max(self.policy.max_tokens);
                            }
                            SessionMode::Agent | SessionMode::Code => {}
                        }
                    } else {
                        self.policy = mode.default_policy_on(&budget);
                        self.effort_locked = matches!(mode, SessionMode::Think | SessionMode::Chat);
                    }
                }
                Err(e) => return Dispatch::Error(RpcError::invalid_params(e.to_string())),
            }
        }
        if self.session_id.is_empty() {
            self.session_id = new_session_id();
        }

        let start = make_start(
            &self.session_id,
            &self.workspace,
            self.mode,
            self.policy.clone(),
            &self.channel,
            self.home.as_deref(),
        );
        let events = self.bind_store(start);
        self.opened = true;
        self.refresh_surface();
        self.refresh_title();
        self.remember_open_session();
        Dispatch::Result {
            result: json!({"ok": true, "session": self.session_id, "channel": self.channel}),
            events,
        }
    }

    pub(crate) fn session_list(&self, params: Option<&Value>) -> Dispatch {
        let p: SessionSearchParams = parse_params(params).unwrap_or_default();
        match catalog::list(match self.persist_dir() {
            Ok(d) => d,
            Err(e) => return Dispatch::Error(RpcError::internal(e.to_string())),
        }) {
            Ok(mut rows) => {
                if let Some(q) = p.search.as_deref() {
                    let q = q.to_ascii_lowercase();
                    rows.retain(|r| {
                        r.id.contains(&q)
                            || r.title.to_ascii_lowercase().contains(&q)
                            || r.preview.to_ascii_lowercase().contains(&q)
                    });
                }
                if let Some(ch) = p.channel.as_deref().filter(|s| !s.is_empty()) {
                    rows.retain(|r| r.channel.eq_ignore_ascii_case(ch));
                }
                Dispatch::Result {
                    result: json!({"ok": true, "sessions": rows}),
                    events: Vec::new(),
                }
            }
            Err(e) => Dispatch::Error(RpcError::internal(e.to_string())),
        }
    }

    pub(crate) fn reply_sessions(&self, search: Option<&str>) -> Dispatch {
        let dir = match self.persist_dir() {
            Ok(d) => d,
            Err(_) => return self.reply_text("**Sessions**\n\n(no persist dir)".into()),
        };
        let mut rows = catalog::list(&dir).unwrap_or_default();
        if let Some(q) = search {
            let q = q.to_ascii_lowercase();
            rows.retain(|r| {
                r.id.contains(&q)
                    || r.title.to_ascii_lowercase().contains(&q)
                    || r.preview.to_ascii_lowercase().contains(&q)
            });
        }
        self.reply_text(sessions_text(&rows))
    }

    pub(crate) fn session_resume(&mut self, params: Option<&Value>) -> Dispatch {
        let p: SessionSearchParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        let q = p
            .session
            .or(p.search)
            .or(p.text)
            .or(p.prompt)
            .unwrap_or_default();
        self.resume_query(if q.is_empty() { None } else { Some(q.as_str()) })
    }

    pub(crate) fn resume_query(&mut self, query: Option<&str>) -> Dispatch {
        if self.turn_in_flight {
            return Dispatch::Error(RpcError::internal("turn in progress"));
        }
        let dir = match self.persist_dir() {
            Ok(d) => d,
            Err(e) => return Dispatch::Error(RpcError::internal(e.to_string())),
        };
        let hit = match catalog::resolve(&dir, query.unwrap_or("latest")) {
            Ok(Some(h)) => h,
            Ok(None) => return self.reply_text("No matching session.".into()),
            Err(e) => return Dispatch::Error(RpcError::internal(e.to_string())),
        };
        self.session_id = hit.id.clone();
        self.channel = hit.channel.clone();
        let start = make_start(
            &self.session_id,
            &self.workspace,
            self.mode,
            self.policy.clone(),
            &self.channel,
            self.home.as_deref(),
        );
        let events = self.bind_store(start);
        self.refresh_surface();
        self.refresh_title();
        self.remember_open_session();
        Dispatch::Result {
            result: json!({"ok": true, "session": self.session_id, "title": self.title}),
            events,
        }
    }

    pub(crate) fn session_new(&mut self, params: Option<&Value>) -> Dispatch {
        let p: SessionSearchParams = parse_params(params).unwrap_or_default();
        self.fresh_session(p.title.as_deref(), true)
    }

    pub(crate) fn session_title(&mut self, params: Option<&Value>) -> Dispatch {
        let p: SessionSearchParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        let Some(title) = p.title.filter(|s| !s.is_empty()) else {
            return Dispatch::Error(RpcError::invalid_params("title is required"));
        };
        if let Some(id) = p
            .session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if id != self.session_id {
                let dir = match self.persist_dir() {
                    Ok(d) => d,
                    Err(e) => return Dispatch::Error(RpcError::internal(e.to_string())),
                };
                return match catalog::set_title(&dir, id, &title) {
                    Ok(()) => self.reply_text(format!("title={title}")),
                    Err(e) => Dispatch::Error(RpcError::internal(e.to_string())),
                };
            }
        }
        self.set_title(&title)
    }

    pub(crate) fn session_delete(&mut self, params: Option<&Value>) -> Dispatch {
        let p: SessionSearchParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        let mut ids: Vec<String> = Vec::new();
        if let Some(id) = p
            .session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            ids.push(id.to_string());
        }
        for id in p.sessions {
            let id = id.trim();
            if !id.is_empty() {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return Dispatch::Error(RpcError::invalid_params("session is required"));
        }
        let drop_open = ids.iter().any(|id| id == &self.session_id);
        if drop_open && self.turn_in_flight {
            return Dispatch::Error(RpcError::internal("turn in progress"));
        }
        let dir = match self.persist_dir() {
            Ok(d) => d,
            Err(e) => return Dispatch::Error(RpcError::internal(e.to_string())),
        };
        let mut deleted = Vec::new();
        for id in &ids {
            if id == &self.session_id {
                continue;
            }
            match catalog::delete(&dir, id) {
                Ok(()) => deleted.push(id.clone()),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("not found") {
                        continue;
                    }
                    return Dispatch::Error(RpcError::internal(format!("{id}: {msg}")));
                }
            }
        }
        if drop_open {
            match catalog::delete(&dir, &self.session_id) {
                Ok(()) => deleted.push(self.session_id.clone()),
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("not found") {
                        return Dispatch::Error(RpcError::internal(msg));
                    }
                    deleted.push(self.session_id.clone());
                }
            }
            let next = catalog::preferred_console(&dir).ok().flatten().or_else(|| {
                catalog::list(&dir)
                    .ok()
                    .and_then(|rows| rows.into_iter().next())
            });
            if let Some(hit) = next {
                return match self.resume_query(Some(&hit.id)) {
                    Dispatch::Result { mut result, events } => {
                        result["deleted"] = json!(deleted);
                        result["ok"] = json!(true);
                        Dispatch::Result { result, events }
                    }
                    other => other,
                };
            }
            let mut out = self.fresh_session(None, false);
            if let Dispatch::Result { result, .. } = &mut out {
                result["deleted"] = json!(deleted);
            }
            return out;
        }
        Dispatch::Result {
            result: json!({"ok": true, "deleted": deleted, "session": self.session_id}),
            events: Vec::new(),
        }
    }

    pub(crate) fn session_status(&self) -> Dispatch {
        let view = self.view();
        self.reply_text(status_text(&view))
    }

    pub(crate) fn session_history(&self) -> Dispatch {
        self.reply_text(history_text(self.events(), 8000))
    }

    pub(crate) fn session_context(&self) -> Dispatch {
        let view = self.view();
        self.reply_text(context_text(&view))
    }

    pub(crate) fn set_title(&mut self, title: &str) -> Dispatch {
        self.title = title.trim().to_string();
        if self.persist {
            if let Ok(dir) = self.persist_dir() {
                let _ = catalog::set_title(&dir, &self.session_id, &self.title);
            }
        }
        self.reply_text(format!("title={}", self.title))
    }

    /// First real user prompt becomes the session name unless `/title` already set one.
    pub fn maybe_autotitle(&mut self, prompt: &str) {
        if !self.title.trim().is_empty() {
            return;
        }
        let title = catalog::title_from_text(prompt);
        if title.is_empty() {
            return;
        }
        self.title = title;
        if self.persist {
            if let Ok(dir) = self.persist_dir() {
                let _ = catalog::set_title(&dir, &self.session_id, &self.title);
            }
        }
    }

    pub(crate) fn fresh_session(&mut self, title: Option<&str>, save_memory: bool) -> Dispatch {
        if self.turn_in_flight {
            return Dispatch::Error(RpcError::internal("turn in progress"));
        }
        if save_memory {
            if let Some(plan) = crate::session::plan_compact(self.events()) {
                if let Some(home) = self.agent_home() {
                    if let Ok(mem) = MemoryStore::open(home) {
                        let _ = mem.write_compact_note(
                            &self.session_id,
                            plan.until_seq,
                            &plan.archive_body(),
                        );
                    }
                }
            }
        }
        let old = self.session_id.clone();
        self.session_id = new_session_id();
        self.title = title.unwrap_or("").to_string();
        let start = make_start(
            &self.session_id,
            &self.workspace,
            self.mode,
            self.policy.clone(),
            &self.channel,
            self.home.as_deref(),
        );
        let events = self.bind_store(start);
        self.refresh_surface();
        if !self.title.is_empty() && self.persist {
            if let Ok(dir) = self.persist_dir() {
                let _ = catalog::set_title(&dir, &self.session_id, &self.title);
            }
        }
        self.remember_open_session();
        Dispatch::Result {
            result: json!({
                "ok": true,
                "session": self.session_id,
                "from": old,
                "title": self.title,
            }),
            events,
        }
    }

    pub(crate) fn force_compact(&mut self, hint: Option<&str>) -> Dispatch {
        let Some(plan) = crate::session::plan_compact(self.events()) else {
            return self.reply_text(compact_reply(None));
        };
        let plan = match hint {
            Some(h) => plan.with_hint(h),
            None => plan,
        };
        if let Some(home) = self.agent_home() {
            if let Ok(mem) = MemoryStore::open(home) {
                let _ =
                    mem.write_compact_note(&self.session_id, plan.until_seq, &plan.archive_body());
            }
        }
        let text = compact_reply(Some(&plan));
        let event = SessionEvent::compact(plan);
        self.record(event.clone());
        Dispatch::Result {
            result: json!({"ok": true, "text": text}),
            events: vec![event],
        }
    }

    pub(crate) fn undo_last(&mut self) -> Dispatch {
        let Some((from, until)) = slash::undo_range(self.events()) else {
            return self.reply_text("Nothing to undo.".into());
        };
        let event = SessionEvent::undo(from, until);
        self.record(event.clone());
        Dispatch::Result {
            result: json!({"ok": true, "text": format!("undid seq {from}..{until}")}),
            events: vec![event],
        }
    }

    pub(crate) fn retry_last(&mut self) -> Dispatch {
        let events = self.events().to_vec();
        let Some((from, until)) = slash::undo_range(&events) else {
            return self.reply_text("Nothing to retry.".into());
        };
        let Some(SessionEvent::User(u)) = events.get(from as usize) else {
            return self.reply_text("Nothing to retry.".into());
        };
        let prompt = u.text.clone();
        let event = SessionEvent::undo(from, until);
        self.record(event.clone());
        if self.turn_in_flight {
            self.mailbox.push_queue(prompt);
            return Dispatch::Result {
                result: json!({"ok": true, "queued": true, "text": "retry queued"}),
                events: vec![event],
            };
        }
        self.turn_in_flight = true;
        Dispatch::turn(prompt)
    }

    pub(crate) fn switch_model(&mut self, args: &str) -> Dispatch {
        match model_text(&self.model, args) {
            ModelAction::Show(text) => self.reply_text(text),
            ModelAction::Switch { name, global } => {
                self.model = name.clone();
                if global {
                    if let Ok(path) = Config::default_path() {
                        let _ = Config::mutate_disk(&path, |cfg| {
                            cfg.server.model = name.clone();
                        });
                    }
                }
                self.reply_text(format!(
                    "model={name} ({})",
                    if global { "global" } else { "session" }
                ))
            }
        }
    }

    pub(crate) fn skill_catalog(&self) -> SkillCatalog {
        let home = self.agent_home();
        SkillCatalog::load(
            home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
            &self.workspace,
        )
    }

    pub(crate) fn mcp_registry(&self) -> crate::mcp::McpRegistry {
        let home = self.agent_home();
        let base = crate::config::Config::default().mcp;
        crate::mcp::McpRegistry::load(home.as_deref(), &self.workspace, &base)
    }

    pub fn refresh_surface(&mut self) {
        let home = self.agent_home();
        let (_, tools) = match self.mode {
            SessionMode::Chat => (String::new(), crate::tools_schema::web_only_tools()),
            SessionMode::Code => (String::new(), code_tools()),
            SessionMode::Agent | SessionMode::Think => sidecar_agent_surface(
                &self.workspace.display().to_string(),
                &self.workspace,
                home.as_deref(),
            ),
        };
        self.tools = tools;
        if matches!(self.mode, SessionMode::Code) {
            self.tools = code_tools();
        }
        self.sync_ask();
    }

    pub(crate) fn sync_ask(&mut self) {
        let armed = matches!(self.mode, SessionMode::Agent | SessionMode::Think)
            && (self.plan_mode || self.clarify_mode);
        crate::tools_schema::sync_ask_tool(&mut self.tools, armed);
    }

    pub(crate) fn refresh_title(&mut self) {
        if !self.persist {
            return;
        }
        if let Ok(dir) = self.persist_dir() {
            self.title = catalog::title_of(&dir, &self.session_id);
        }
    }

    pub(crate) fn remember_open_session(&self) {
        if !self.persist || self.session_id.is_empty() {
            return;
        }
        if let Ok(dir) = self.persist_dir() {
            let _ = catalog::remember(&dir, &self.session_id);
        }
    }

    pub fn events(&self) -> &[SessionEvent] {
        match &self.store {
            EventStore::Log(log) => log.events(),
            EventStore::Memory(events) => events,
        }
    }

    pub fn reload(&mut self) {
        if !self.persist || self.session_id.is_empty() {
            return;
        }
        let Ok(dir) = self.persist_dir() else {
            return;
        };
        if let Ok(log) = SessionLog::open_in(&dir, &self.session_id) {
            if let Some(p) = log.policy() {
                if !self.effort_locked {
                    self.policy = p;
                }
            }
            self.store = EventStore::Log(log);
            self.refresh_title();
        }
    }

    pub fn finish_turn(&mut self, result: &TurnResult) -> Vec<SessionEvent> {
        self.turn_in_flight = false;
        for note in &result.pending_steer {
            self.mailbox.push_queue(note.clone());
        }
        if result.streamed {
            if self.persist {
                self.reload();
            }
            if result.aborted {
                // 中止的流式 turn 不会自己写 stop;补记落盘,刷新后仍有结束标记
                let stop = SessionEvent::stop("aborted");
                self.record(stop.clone());
                return vec![stop];
            }
            return Vec::new();
        }
        if self.persist {
            self.reload();
            let mut out = Vec::new();
            if result.aborted {
                let stop = SessionEvent::stop("aborted");
                self.record(stop.clone());
                out.push(stop);
            } else if let Some(err) = &result.error {
                let stop = SessionEvent::stop(err.clone());
                self.record(stop.clone());
                out.push(stop);
            } else {
                if !result.text.is_empty() {
                    out.push(SessionEvent::assistant(
                        result.text.clone(),
                        String::new(),
                        None,
                    ));
                }
                out.push(SessionEvent::stop(
                    result.stop_reason.clone().unwrap_or_else(|| "stop".into()),
                ));
            }
            return out;
        }
        let mut out = result.events.clone();
        for e in &result.events {
            self.record(e.clone());
        }
        if result.aborted {
            let stop = SessionEvent::stop("aborted");
            self.record(stop.clone());
            out.push(stop);
            return out;
        }
        if let Some(err) = &result.error {
            let stop = SessionEvent::stop(err.clone());
            self.record(stop.clone());
            out.push(stop);
            return out;
        }
        if !result.text.is_empty() {
            let assistant = SessionEvent::assistant(result.text.clone(), String::new(), None);
            self.record(assistant.clone());
            out.push(assistant);
        }
        let reason = result.stop_reason.clone().unwrap_or_else(|| "stop".into());
        let stop = SessionEvent::stop(reason);
        self.record(stop.clone());
        out.push(stop);
        out
    }

    pub(crate) fn bind_store(&mut self, start: SessionStart) -> Vec<SessionEvent> {
        self.opened = true;
        if !self.persist {
            let event = SessionEvent::Start(start);
            self.store = EventStore::Memory(vec![event.clone()]);
            return vec![event];
        }
        match self.persist_dir() {
            Err(e) => {
                eprintln!("q38: session JSONL unavailable ({e}); this session stays in memory");
                let event = SessionEvent::Start(start);
                self.store = EventStore::Memory(vec![event.clone()]);
                return vec![event];
            }
            Ok(dir) => match SessionLog::open_in(&dir, &self.session_id) {
                Ok(log) => {
                    if let Some(p) = log.policy() {
                        if !self.effort_locked {
                            self.policy = p;
                        }
                    }
                    if let Some(s) = log.start() {
                        self.mode = s.mode;
                        self.session_id = s.id.clone();
                        if !s.workspace.is_empty() {
                            self.workspace = PathBuf::from(&s.workspace);
                        }
                    }
                    self.store = EventStore::Log(log);
                    Vec::new()
                }
                Err(_) => match SessionLog::create_in(&dir, start.clone()) {
                    Ok(log) => {
                        self.store = EventStore::Log(log);
                        vec![SessionEvent::Start(start)]
                    }
                    Err(e) => {
                        eprintln!(
                            "q38: session JSONL unavailable ({e}); this session stays in memory"
                        );
                        let event = SessionEvent::Start(start);
                        self.store = EventStore::Memory(vec![event.clone()]);
                        vec![event]
                    }
                },
            },
        }
    }

    pub(crate) fn record(&mut self, event: SessionEvent) {
        if event.is_ephemeral() {
            return;
        }
        match &mut self.store {
            EventStore::Memory(events) => events.push(event),
            EventStore::Log(log) => {
                if matches!(event, SessionEvent::Start(_)) {
                    return;
                }
                let _ = log.append(event);
            }
        }
    }
}
