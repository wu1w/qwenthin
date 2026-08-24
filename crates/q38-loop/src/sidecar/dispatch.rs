//! Slash, busy/queue/steer, and channel inbound dispatch.

use serde_json::{json, Value};

use super::helpers::{make_start, parse_params, slash_policy, SlashParams, TurnStartParams};
use super::types::{Dispatch, RpcError};
use super::{EventStore, SidecarSession};
use crate::channel::{BusyDecision, SteerSlot};
use crate::config::Config;
use crate::media::MediaPart;
use crate::permit::{ApprovalMode, PlanAction, PLAN_IMPLEMENT};
use crate::policy::{Effort, XHIGH_WARN};
use crate::session::{new_session_id, PolicyReason, SessionEvent, SessionMode, SlashCmd};
use crate::slash::{
    approvals_text, clarify_text, config_text, context_text, diff_text, help_text, history_text,
    low_precision_text, mcp_text, parse_slash_with_periphery, plan_text, setup_text, skills_text,
    status_text, tools_text, unsupported_text, usage_view, version_text, SlashView,
};

impl SidecarSession {
    pub(crate) fn slash(&mut self, params: Option<&Value>) -> Dispatch {
        if !self.opened {
            return Dispatch::Error(RpcError::invalid_request("session not open"));
        }
        let p: SlashParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        let skills = self.skill_catalog();
        let mcp = self.mcp_registry();
        let Some(cmd) = parse_slash_with_periphery(&p.text, &skills, Some(&mcp)) else {
            return Dispatch::Error(RpcError::invalid_params(format!(
                "unknown slash command: {}",
                p.text.trim()
            )));
        };
        self.dispatch_slash(cmd)
    }

    pub(crate) fn dispatch_slash(&mut self, cmd: SlashCmd) -> Dispatch {
        match cmd {
            SlashCmd::Mode(mode) => self.fork_mode(mode),
            SlashCmd::Off | SlashCmd::Think(_) => {
                self.policy = slash_policy(&cmd, &self.caps);
                self.effort_locked = true;
                if matches!(self.policy.effort, Some(Effort::Xhigh)) {
                    eprintln!("{XHIGH_WARN}");
                }
                let event = SessionEvent::policy(self.policy.clone(), PolicyReason::Slash);
                self.record(event.clone());
                Dispatch::Result {
                    result: json!({"ok": true, "text": format!("thinking {}", if self.policy.enabled { "on" } else { "off" })}),
                    events: vec![event],
                }
            }
            SlashCmd::Help => self.reply_text(help_text()),
            SlashCmd::Status => {
                let view = self.view();
                self.reply_text(status_text(&view))
            }
            SlashCmd::Context { .. } => {
                let view = self.view();
                self.reply_text(context_text(&view))
            }
            SlashCmd::History => self.reply_text(history_text(self.events(), 8000)),
            SlashCmd::Usage => {
                let view = self.view();
                self.reply_text(usage_view(&view))
            }
            SlashCmd::Tools => self.reply_text(tools_text(&self.tools)),
            SlashCmd::Skills => {
                let cat = self.skill_catalog();
                self.reply_text(skills_text(&cat))
            }
            SlashCmd::Mcp => {
                let reg = self.mcp_registry();
                self.reply_text(mcp_text(&reg))
            }
            SlashCmd::Version => self.reply_text(version_text()),
            SlashCmd::Config => self.reply_text(config_text(
                &self.model,
                &self.workspace,
                self.mode,
                self.mailbox.busy,
            )),
            SlashCmd::Diff { args } => self.reply_text(diff_text(&self.workspace, &args)),
            SlashCmd::Sessions { search } => self.reply_sessions(search.as_deref()),
            SlashCmd::Unsupported { name } => self.reply_text(unsupported_text(&name)),
            SlashCmd::Title { name } => self.set_title(&name),
            SlashCmd::New { title } => self.fresh_session(title.as_deref(), true),
            SlashCmd::Clear => self.fresh_session(None, false),
            SlashCmd::Resume { query } => self.resume_query(query.as_deref()),
            SlashCmd::Compress { hint } => self.force_compact(hint.as_deref()),
            SlashCmd::Stop => {
                let n = self.mailbox.clear_queue();
                let _ = self.mailbox.take_redirect();
                Dispatch::AbortClear { cleared: n }
            }
            SlashCmd::Queue { text } => self.enqueue_prompt(text, false),
            SlashCmd::Steer { text } => self.enqueue_prompt(text, true),
            SlashCmd::Busy { policy } => match policy {
                None => self.reply_text(format!("busy={}", self.mailbox.busy.as_str())),
                Some(p) => {
                    self.mailbox.busy = p;
                    self.reply_text(format!("busy={}", p.as_str()))
                }
            },
            SlashCmd::Undo => self.undo_last(),
            SlashCmd::Retry => self.retry_last(),
            SlashCmd::Model { args } => self.switch_model(&args),
            SlashCmd::Reload => self.reply_text("config will apply on the next turn".into()),
            SlashCmd::Setup => self.reply_text(setup_text()),
            SlashCmd::Approvals { mode } => self.set_approvals(mode),
            SlashCmd::Plan { action } => self.set_plan(action),
            SlashCmd::Clarify { on } => self.set_clarify(on),
            SlashCmd::LowPrecision { on } => self.set_lossy(on),
            SlashCmd::InvokeSkill { name, args } => {
                let prompt = crate::sticky::skill_turn_prompt(&name, &args);
                if self.turn_in_flight {
                    self.enqueue_prompt(prompt, false)
                } else {
                    self.turn_in_flight = true;
                    Dispatch::turn(prompt)
                }
            }
            SlashCmd::InvokeMcp { name, args } => {
                let prompt = crate::sticky::mcp_turn_prompt(&name, &args);
                if self.turn_in_flight {
                    self.enqueue_prompt(prompt, false)
                } else {
                    self.turn_in_flight = true;
                    Dispatch::turn(prompt)
                }
            }
            SlashCmd::Cron { args } => {
                self.reply_text(crate::cron::apply_slash(&self.workspace, &args))
            }
        }
    }

    pub(crate) fn reply_text(&self, text: String) -> Dispatch {
        Dispatch::Result {
            result: json!({"ok": true, "text": text}),
            events: Vec::new(),
        }
    }

    pub(crate) fn view(&self) -> SlashView<'_> {
        SlashView {
            session_id: &self.session_id,
            workspace: &self.workspace,
            mode: self.mode,
            policy: &self.policy,
            events: self.events(),
            model: &self.model,
            busy: self.mailbox.busy,
            channel: &self.channel,
            title: &self.title,
            tools: &self.tools,
            skill_count: self.skill_catalog().skills.len(),
            mcp_count: self.mcp_registry().servers.len(),
            window: self.window,
            family: self.family,
            queued: self.mailbox.queued(),
            plan_mode: self.plan_mode,
            clarify_mode: self.clarify_mode,
            approvals: self.approvals,
            low_precision: self.low_precision,
        }
    }

    fn set_approvals(&mut self, mode: Option<ApprovalMode>) -> Dispatch {
        if let Some(mode) = mode {
            self.approvals = mode;
            if let Ok(path) = Config::default_path() {
                let _ = Config::mutate_disk(&path, |cfg| {
                    cfg.features.approvals = mode.as_str().into();
                });
            }
        }
        self.reply_text(approvals_text(self.approvals))
    }

    fn set_lossy(&mut self, on: Option<bool>) -> Dispatch {
        if let Some(on) = on {
            self.low_precision = on;
            if let Ok(path) = Config::default_path() {
                let _ = Config::mutate_disk(&path, |cfg| {
                    cfg.policy.low_precision = on;
                });
            }
        }
        self.reply_text(low_precision_text(self.low_precision))
    }

    fn set_plan(&mut self, action: PlanAction) -> Dispatch {
        match action {
            PlanAction::On => {
                self.plan_mode = true;
                self.sync_ask();
                self.reply_text(plan_text(true))
            }
            PlanAction::Off => {
                self.plan_mode = false;
                self.sync_ask();
                self.reply_text(plan_text(false))
            }
            PlanAction::Go => {
                if !self.plan_mode {
                    return self.reply_text(
                        "not in plan mode. `/plan` first, then `/plan go` after you like the plan."
                            .into(),
                    );
                }
                self.plan_mode = false;
                self.sync_ask();
                Dispatch::turn(PLAN_IMPLEMENT)
            }
        }
    }

    fn set_clarify(&mut self, on: Option<bool>) -> Dispatch {
        if let Some(on) = on {
            self.clarify_mode = on;
            self.sync_ask();
        }
        self.reply_text(clarify_text(self.clarify_mode, self.plan_mode))
    }

    pub(crate) fn fork_mode(&mut self, mode: SessionMode) -> Dispatch {
        if self.turn_in_flight {
            return Dispatch::Error(RpcError::internal("turn in progress"));
        }
        let from_id = self.session_id.clone();
        let new_id = new_session_id();
        let policy = mode.default_policy_on(&self.caps.think_budget());
        let start = make_start(
            &new_id,
            &self.workspace,
            mode,
            policy.clone(),
            &self.channel,
        );
        let fork = SessionEvent::fork(&from_id);

        match &mut self.store {
            EventStore::Log(log) => {
                let Some(old) = log.start().cloned() else {
                    return Dispatch::Error(RpcError::internal("missing session/start"));
                };
                let new_start = old.for_fork(
                    new_id.clone(),
                    mode,
                    start.system.clone(),
                    start.tools_hash.clone(),
                );
                match log.fork(new_start.clone()) {
                    Ok(forked) => {
                        self.store = EventStore::Log(forked);
                    }
                    Err(e) => {
                        return Dispatch::Error(RpcError::internal(e.to_string()));
                    }
                }
            }
            EventStore::Memory(events) => {
                if events.is_empty() || !matches!(events[0], SessionEvent::Start(_)) {
                    return Dispatch::Error(RpcError::internal("missing session/start"));
                }
                events[0] = SessionEvent::Start(start.clone());
                events.push(fork.clone());
            }
        }

        self.session_id = new_id.clone();
        self.mode = mode;
        self.policy = policy;
        self.effort_locked = matches!(mode, SessionMode::Think | SessionMode::Chat);
        self.refresh_surface();
        self.remember_open_session();
        Dispatch::Result {
            result: json!({"ok": true, "session": new_id, "mode": mode.as_str()}),
            events: vec![SessionEvent::Start(start), fork],
        }
    }

    pub(crate) fn turn_start(&mut self, params: Option<&Value>) -> Dispatch {
        if !self.opened {
            return Dispatch::Error(RpcError::invalid_request("session not open"));
        }
        let p: TurnStartParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        let prompt = p.prompt();
        let parts = p.parts();
        if prompt.trim().is_empty() && parts.is_empty() {
            return Dispatch::Error(RpcError::invalid_params("prompt is required"));
        }
        self.accept_prompt_parts(
            if prompt.is_empty() {
                " ".into()
            } else {
                prompt
            },
            parts,
        )
    }

    pub(crate) fn turn_abort(&mut self) -> Dispatch {
        Dispatch::Abort
    }

    pub(crate) fn turn_queue(&mut self, params: Option<&Value>) -> Dispatch {
        let p: TurnStartParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        self.enqueue_prompt(p.prompt(), false)
    }

    pub(crate) fn turn_steer(&mut self, params: Option<&Value>) -> Dispatch {
        let p: TurnStartParams = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        self.enqueue_prompt(p.prompt(), true)
    }

    pub(crate) fn channel_inbound(&mut self, params: Option<&Value>) -> Dispatch {
        if !self.opened {
            return Dispatch::Error(RpcError::invalid_request("session not open"));
        }
        let mut env: crate::channel::NativePayload = match parse_params(params) {
            Ok(p) => p,
            Err(e) => return Dispatch::Error(e),
        };
        if !env.channel.is_empty() {
            self.channel = env.channel.clone();
        }
        if env.session_id.is_empty() {
            if let Ok(mut router) = crate::channel::SessionRouter::in_home() {
                if let Ok(id) = router.resolve(&env) {
                    env.session_id = id;
                }
            }
        }
        if !env.session_id.is_empty() && env.session_id != self.session_id {
            self.session_id = env.session_id.clone();
            let start = make_start(
                &self.session_id,
                &self.workspace,
                self.mode,
                self.policy.clone(),
                &self.channel,
            );
            let _ = self.bind_store(start);
            self.refresh_surface();
            self.refresh_title();
        }
        let prompt = env.query_text();
        let parts = env.media_parts();
        if prompt.trim().is_empty() && parts.is_empty() {
            return Dispatch::Error(RpcError::invalid_params("text/content_parts is required"));
        }
        if parts.is_empty() {
            if let Some(cmd) = parse_slash_with_periphery(
                &prompt,
                &self.skill_catalog(),
                Some(&self.mcp_registry()),
            ) {
                return self.dispatch_slash(cmd);
            }
        }
        self.accept_prompt_parts(
            if prompt.trim().is_empty() {
                " ".into()
            } else {
                prompt
            },
            parts,
        )
    }

    pub(crate) fn accept_prompt_parts(
        &mut self,
        prompt: String,
        parts: Vec<MediaPart>,
    ) -> Dispatch {
        if self.turn_in_flight {
            return match self.mailbox.offer_while_busy(prompt) {
                BusyDecision::AbortThenRedirect => Dispatch::Abort,
                BusyDecision::Queued => Dispatch::Result {
                    result: json!({"ok": true, "queued": true, "n": self.mailbox.queued()}),
                    events: Vec::new(),
                },
                BusyDecision::Steered => Dispatch::Result {
                    result: json!({"ok": true, "steered": true}),
                    events: Vec::new(),
                },
            };
        }
        self.turn_in_flight = true;
        Dispatch::turn_parts(prompt, parts)
    }

    pub(crate) fn enqueue_prompt(&mut self, text: String, steer: bool) -> Dispatch {
        if text.trim().is_empty() {
            return Dispatch::Error(RpcError::invalid_params("text is required"));
        }
        if steer {
            if self.turn_in_flight {
                self.mailbox.push_steer(text);
                return Dispatch::Result {
                    result: json!({"ok": true, "steered": true}),
                    events: Vec::new(),
                };
            }
            self.mailbox.push_queue(text);
            return self.take_follow_up().unwrap_or_else(|| {
                self.reply_text("steered text queued until the next turn".into())
            });
        }
        if self.turn_in_flight {
            self.mailbox.push_queue(text);
            return Dispatch::Result {
                result: json!({"ok": true, "queued": true, "n": self.mailbox.queued()}),
                events: Vec::new(),
            };
        }
        self.turn_in_flight = true;
        Dispatch::turn(text)
    }

    pub(crate) fn take_follow_up(&mut self) -> Option<Dispatch> {
        let prompt = self
            .mailbox
            .take_redirect()
            .or_else(|| self.mailbox.pop_queue())?;
        self.turn_in_flight = true;
        Some(Dispatch::turn(prompt))
    }

    pub fn pop_follow_up(&mut self) -> Option<String> {
        self.mailbox
            .take_redirect()
            .or_else(|| self.mailbox.pop_queue())
    }

    pub fn has_redirect(&self) -> bool {
        self.mailbox.has_redirect()
    }

    pub fn steer_slot(&self) -> SteerSlot {
        self.mailbox.steer_slot()
    }
}
