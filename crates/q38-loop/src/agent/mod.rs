//! ReAct agent. Loop shape from QwenPaw `react_agent._reasoning`.
//!
//! Per user turn (`run`):
//! 1. apply deferred TERMINATE from a previous tool iter (before the model)
//! 2. model — thought lives in `reasoning_content`; if native `tool_calls` are
//!    empty, recover complete `<tool_call>` XML from that thought (QwenPaw)
//! 3. if tools: gates — TERMINATE with reason is deferred; execute via ToolCoordinator
//! 4. if text: gates — Stop ends the turn. Continue is not turned into a
//!    lecture; empty Continue is treated as Stop. Empty `content` with leftover
//!    think gets one wrap-up hop so Flash-Next cannot park the answer in
//!    `reasoning_content` and go silent.
//!
//! Parse failures and think-cap hits retry the same messages (kwargs only).
//! Trajectory detectors (doom / name / path / stutter / dump) inject one
//! hidden observation and then stay silent. Step, time, and context budgets
//! get the same treatment: one wrap-up hop, then a quiet finish that keeps
//! the last spoken text. User abort is the only user-visible stop reason.
//! `ThinkPolicy` kwargs stay frozen for the turn except an ephemeral no-tool
//! think clip, watchdog restore, and a same-session auto-upgrade. A later
//! clean step drops back to the turn baseline.

mod delta;
mod guard;
mod http;
mod verify;
mod xml_tools;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::channel::{take_steer, SteerSlot};
use crate::config::{Config, WorkingWindowOverlay};
use crate::echo::strip_greeting_echo;
use crate::error::Result;
use crate::family::Family;
use crate::mcp::{card_for as mcp_card, run_mcp, McpConfig, McpRegistry};
use crate::media::{MediaBins, MediaCaps, MediaKind};
use crate::memory::{card_for as memory_card, run_memory_search, MemoryStore};
use crate::paw_loop::{
    fs_tool_path, DoomLoopGate, Gate, GateCtx, GateDecision, IterationGate, NameStreakGate,
    PathLoopGate, StopHandler, TimeoutGate, ToolCallBudgetGate, ToolFingerprint, LOSSY_TOOL_BUDGET,
};
use crate::permit::{self, PermitDecision, PermitHub, PLAN_CARD};
use crate::policy::{EffortController, TemplateKwargs, ThinkPolicy};
use crate::prompt::{periphery_section, session_prompt};
use crate::session::{
    compact_messages, derive_messages, plan_compact, run_recall, tools_hash, OpenAiToolCall,
    PolicyReason, SessionEvent, SessionLog, SessionMode, SessionStart,
};
use crate::skills::{hidden_card, match_tool_output, match_user, run_skill, SkillCatalog};
use crate::sticky;
use crate::template::{is_hidden_user_text, render, wrap_tool_response, ChatMessage, RenderOpts};
use crate::tokenize::count_tokens;
use crate::tool_calls::{
    CancelFlag, ToolCall, ToolCoordinator, ToolResponse, ToolState,
    COORDINATOR_OWNED_EXEC_TIMEOUT_SECS,
};
use crate::tools::{
    bash_search_query, run_search, run_tool, search_dump_too_big, view, BlobStore, CodeIndex,
    ToolLimits, Workspace,
};
use crate::tools_schema::{
    agent_tools, ask_tool, code_tools, has_recall, has_tool, mcp_tool, memory_search_tool,
    recall_tool, search_tool, view_tool,
};

pub use delta::TokenSink;
pub use http::{parse_cached_tokens, parse_turn, HttpCompleter, ParseOutcome};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolSet {
    None,
    #[default]
    Agent,
    Code,
}

#[derive(Clone, Debug)]
pub struct ModelTurn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub raw_tool_calls: Option<Vec<Value>>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub watchdog_hit: bool,
    pub parse_fail: bool,
    /// Prefix cache hits. `None` if the engine omitted the field.
    pub cached_tokens: Option<u64>,
    /// llama.cpp `timings.predicted_per_second` (decode tok/s). `None` if omitted.
    pub decode_tok_s: Option<f64>,
}

impl ModelTurn {
    fn watchdog() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            raw_tool_calls: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            watchdog_hit: true,
            parse_fail: false,
            cached_tokens: None,
            decode_tok_s: None,
        }
    }
}

pub trait Completer: Send + Sync {
    fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> impl Future<Output = Result<ModelTurn>> + Send;

    /// When set, the loop meters the local Jinja prefix against the working window.
    fn prefix_meter(&self) -> Option<(Family, TemplateKwargs)> {
        None
    }

    fn set_policy(&self, _p: ThinkPolicy) {}

    fn policy(&self) -> Option<ThinkPolicy> {
        None
    }

    /// Live token sink. Default no-op (scripted tests).
    fn set_token_sink(&self, _sink: Option<TokenSink>) {}

    /// Pin llama.cpp KV slot for this session id (cross-turn prefix cache).
    fn pin_session(&self, _session_id: &str) {}

    /// Lossy overlay sampling (repetition_penalty 1.1). Default no-op.
    fn set_low_precision(&self, _on: bool) {}

    fn media_caps(&self) -> crate::media::MediaCaps {
        crate::media::MediaCaps::default()
    }
}

#[derive(Clone, Debug)]
pub struct RunOpts {
    pub workspace: PathBuf,
    pub session_id: String,
    pub confined: bool,
    pub with_tools: bool,
    pub tool_set: ToolSet,
    pub max_steps: u32,
    pub max_wall: Duration,
    pub agents_md: bool,
    pub agents_md_max_tokens: u32,
    /// Clip AGENTS.md to `agents_md_max_tokens` instead of omitting it.
    pub agents_md_head: bool,
    pub print: bool,
    pub tool_limits: ToolLimits,
    pub bash_timeout_secs: f64,
    pub inherit_env: bool,
    pub working_window: u32,
    pub generation_reserve: u32,
    /// Fraction of `working_window` that starts compact (clamped 0.10..=1.0).
    pub compact_ratio: f64,
    /// Set when `Q38_WORKING_WINDOW` replaced the file value. One-shot card.
    pub working_window_overlay: Option<WorkingWindowOverlay>,
    /// CLI `--think` / `--mode think` / `--fast` (and slash depth) lock auto effort.
    pub effort_locked: bool,
    pub persist_session: bool,
    pub session_mode: SessionMode,
    /// Override `~/.q38-agent/sessions` (tests).
    pub session_dir: Option<PathBuf>,
    /// Override `~/.q38-agent/blobs` (tests).
    pub blob_dir: Option<PathBuf>,
    /// Override `~/.q38-agent` (tests). Isolates MEMORY.md / skills / memory.sqlite.
    pub home: Option<PathBuf>,
    /// Append memory_search / mcp (never splice the frozen four). `skill` stays
    /// out of tools[]; bodies are hidden-user notes. `mcp` is one extra blob,
    /// appended only when servers are mounted.
    pub peripheral: bool,
    pub skills_auto_catalog: bool,
    pub mcp_auto_catalog: bool,
    pub mcp: McpConfig,
    /// Append `web` (search/fetch) at session start. Builtin engines need no
    /// key; a Tavily key upgrades the backend transparently.
    pub web: crate::config::WebConfig,
    /// Append `view` in agent mode (not spliced into the frozen four).
    pub media: bool,
    pub media_max_bytes: usize,
    pub media_bins: MediaBins,
    /// `AGENT.md` filename (workspace then home).
    pub prompt_file: String,
    /// Builtin 编程助手 when no AGENT.md exists.
    pub coding_identity: bool,
    /// Hidden plan card + mutating-tool deny. Does not change the frozen four.
    pub plan_mode: bool,
    /// Session `/clarify`. Combined with plan_mode, appends the `ask` tool.
    pub clarify_mode: bool,
    /// TUI permission bridge. None = YOLO (`--print`, tests).
    pub permit: Option<crate::permit::PermitHub>,
    /// Blocking ask overlay. None = skip to the recommended option.
    pub clarify: Option<crate::clarify::ClarifyHub>,
    /// User switch: tighter doom/parse/repeat guards. Default off.
    pub low_precision: bool,
    /// IM / console channel stamped on new JSONL `session/start`. Empty = default `cli`.
    pub channel: String,
    /// Interactive narration style card (TUI/web only; `--print` and IM stay silent).
    pub narrate: bool,
}

impl RunOpts {
    pub fn from_config(cfg: &Config, workspace: PathBuf) -> Self {
        Self {
            workspace,
            session_id: "print".into(),
            confined: cfg.features.workspace_write_only,
            with_tools: true,
            tool_set: ToolSet::Agent,
            max_steps: cfg.policy.max_steps,
            max_wall: Duration::from_secs(cfg.policy.max_wall_seconds),
            agents_md: false,
            agents_md_max_tokens: cfg.context.agents_md_max_tokens,
            agents_md_head: false,
            print: true,
            tool_limits: ToolLimits::from(&cfg.tools),
            bash_timeout_secs: cfg.code_mode.timeout_s as f64,
            inherit_env: cfg.code_mode.inherit_env,
            working_window: cfg.context.working_window,
            compact_ratio: if cfg.context.compact_ratio.is_finite() {
                cfg.context.compact_ratio
            } else {
                DEFAULT_COMPACT_RATIO
            },
            working_window_overlay: cfg.working_window_overlay,
            generation_reserve: clamp_generation_reserve(
                cfg.context.working_window,
                cfg.policy
                    .max_tokens
                    .saturating_add(cfg.policy.max_think_tokens_xhigh),
            ),
            effort_locked: false,
            persist_session: false,
            session_mode: SessionMode::Agent,
            session_dir: None,
            blob_dir: None,
            home: Config::home_dir().ok(),
            peripheral: true,
            skills_auto_catalog: cfg.features.skills_auto_catalog,
            mcp_auto_catalog: cfg.features.mcp_auto_catalog,
            mcp: cfg.mcp.clone(),
            web: cfg.web.clone(),
            media: cfg.media.enabled,
            media_max_bytes: cfg.media.max_bytes as usize,
            media_bins: MediaBins::from_config(&cfg.media),
            prompt_file: cfg.prompt.file.clone(),
            coding_identity: cfg.prompt.coding,
            plan_mode: false,
            clarify_mode: false,
            permit: None,
            clarify: None,
            low_precision: cfg.policy.low_precision,
            channel: String::new(),
            narrate: cfg.prompt.narrate,
        }
    }
}

/// Console-facing channels where a human watches progress live. IM bridges
/// deliver per-message and would surface narration as chat spam.
pub(crate) fn interactive_channel(channel: &str) -> bool {
    matches!(channel, "" | "cli" | "tui" | "web" | "console")
}

/// Hermes-shaped unattended caps: gateway `max_turns` 500, no hard wall while working.
pub fn apply_unattended_policy(opts: &mut RunOpts, cfg: &crate::config::Config) {
    if interactive_channel(&opts.channel) {
        return;
    }
    if cfg.policy.max_steps_unattended > 0 {
        opts.max_steps = cfg.policy.max_steps_unattended;
    }
    opts.max_wall = Duration::from_secs(cfg.policy.max_wall_unattended_seconds);
}

const DEFAULT_COMPACT_RATIO: f64 = 0.70;
const TURN_START_COMPACT_PREFIX: u32 = 120_000;
/// A finished tool-heavy turn should not be replayed as a cold prefill, even
/// when the cheap byte estimate sits under 120k.
const TURN_START_COMPACT_TOOLS: usize = 8;
/// In-memory images from the previous turn. Archive sooner than the wire cap.
const TURN_START_COMPACT_IMAGES: usize = 4;

fn clamp_generation_reserve(window: u32, reserve: u32) -> u32 {
    if window == 0 {
        return reserve;
    }
    let cap = (window / 4).max(64).min(window.saturating_sub(1));
    reserve.min(cap)
}

fn clamp_compact_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(0.10, 1.0)
    } else {
        DEFAULT_COMPACT_RATIO
    }
}

#[cfg(test)]
fn compact_soft_limit(working_window: u32, compact_ratio: f64) -> u32 {
    (working_window as f64 * clamp_compact_ratio(compact_ratio)) as u32
}

fn over_soft_threshold(prefix: u32, reserve: u32, working_window: u32, compact_ratio: f64) -> bool {
    working_window != 0
        && (prefix.saturating_add(reserve) as f64)
            > (working_window as f64) * clamp_compact_ratio(compact_ratio)
}

fn over_hard_threshold(prefix: u32, reserve: u32, working_window: u32) -> bool {
    working_window != 0 && prefix.saturating_add(reserve) > working_window
}

fn should_compact_at_user_turn(
    prefix: u32,
    reserve: u32,
    working_window: u32,
    compact_ratio: f64,
) -> bool {
    if working_window == 0 {
        return false;
    }
    over_soft_threshold(prefix, reserve, working_window, compact_ratio)
        || prefix > TURN_START_COMPACT_PREFIX
}

fn should_compact_follow_up(
    prefix: u32,
    reserve: u32,
    working_window: u32,
    compact_ratio: f64,
    tool_messages: usize,
    image_parts: usize,
    tool_threshold: usize,
) -> bool {
    if working_window == 0 {
        return false;
    }
    tool_messages >= tool_threshold
        || image_parts > TURN_START_COMPACT_IMAGES
        || should_compact_at_user_turn(prefix, reserve, working_window, compact_ratio)
}

fn live_tool_count(messages: &[ChatMessage]) -> usize {
    messages.iter().filter(|m| m.role == "tool").count()
}

fn live_image_count(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.parts
                .iter()
                .filter(|p| p.kind == MediaKind::Image)
                .count()
        })
        .sum()
}

/// Cheap compact gate. Skips HuggingFace encode and does not walk data-URI
/// payloads. English/JSON ≈ 4 bytes/token; CJK still crosses 120k on a 60-tool
/// fold.
fn estimate_prefix_tokens(
    messages: &[ChatMessage],
    tools: &[serde_json::Value],
    keep_reasoning: bool,
) -> u32 {
    let mut bytes = 0usize;
    for m in messages {
        if let Some(c) = &m.content {
            bytes += charged_text_len(c);
        }
        if keep_reasoning {
            if let Some(r) = &m.reasoning_content {
                bytes += charged_text_len(r);
            }
        }
        if let Some(calls) = &m.tool_calls {
            bytes += calls.iter().map(|v| v.to_string().len()).sum::<usize>();
        }
        for p in &m.parts {
            bytes += if p.url.starts_with("data:") {
                1024
            } else {
                p.url.len().min(256)
            };
        }
    }
    for t in tools {
        bytes += t.to_string().len();
    }
    (bytes / 4) as u32
}

fn charged_text_len(s: &str) -> usize {
    if s.starts_with("data:") {
        return 1024;
    }
    if s.len() > 16_384 && (s.contains("data:image") || s.contains(";base64,")) {
        2048
    } else {
        s.len()
    }
}

#[derive(Clone, Debug)]
pub struct AgentOutcome {
    pub text: String,
    pub stop_reason: Option<String>,
    pub steps: u32,
    pub session_id: String,
    pub pending_steer: Vec<String>,
    /// CLI `print` already painted answer tokens to stdout.
    pub streamed_text: bool,
}

pub struct Agent<C> {
    completer: C,
    workspace: Workspace,
    handler: StopHandler,
    coordinator: ToolCoordinator,
    session_id: String,
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    pending_stop: Option<String>,
    /// Runner proven usable by the turn-start baseline probe (`--print` only).
    oracle_cmd: Option<String>,
    oracle_runs: u32,
    print: bool,
    limits: ToolLimits,
    inherit_env: bool,
    working_window: u32,
    generation_reserve: u32,
    compact_ratio: f64,
    effort: EffortController,
    log: Option<SessionLog>,
    last_policy: ThinkPolicy,
    blobs: BlobStore,
    memory: Option<MemoryStore>,
    skills: SkillCatalog,
    mcp: McpRegistry,
    web: Option<crate::tools::WebRunner>,
    media_caps: MediaCaps,
    media_max_bytes: usize,
    media_bins: MediaBins,
    cancel: CancelFlag,
    steer: SteerSlot,
    emit: Option<crate::sidecar::EventSink>,
    stdio: std::sync::Arc<delta::StdioState>,
    plan_mode: bool,
    clarify_mode: bool,
    permit: Option<PermitHub>,
    clarify: Option<crate::clarify::ClarifyHub>,
    low_precision: bool,
    parse_stop_after: u32,
    /// Last substantial assistant content this user turn. Harness-only.
    last_spoken: Option<String>,
    /// Substantial hop text even when `last_spoken` was not locked (read-only hops).
    last_essay: Option<String>,
    /// Paths successfully `read` this user turn. Re-reads after an answer are not progress.
    read_paths: HashSet<String>,
    /// Paths whose content the live transcript has seen (read/view/write/edit).
    /// `write` to an existing file outside this set is a blind overwrite and is
    /// refused with a re-read fact. Rebuilt from the transcript each turn;
    /// cleared on compact (the content left the window with it).
    observed_paths: HashSet<String>,
    /// Consumed on the first `run_message` so a soak leftover env is visible once.
    window_overlay: Option<WorkingWindowOverlay>,
    /// S1/S2 judgment guards over successful edits.
    edit_guard: guard::EditGuard,
    /// Interactive channels get a one-line narration style card.
    narrate: bool,
    /// Workspace FTS for `search`. Built at session start; refreshed on write/edit.
    code_index: Option<CodeIndex>,
    /// Lossy stutter already received one hidden observation this user turn.
    stutter_nudged: bool,
    /// Physics cap (steps / wall / context / tool budget) already got a wrap-up hop.
    physics_nudged: bool,
    /// Parse-fail cap already got one repair observation.
    parse_nudged: bool,
    /// Empty visible reply (answer parked in reasoning) already got one wrap-up.
    channel_nudged: bool,
    /// Consecutive tool hops with empty `content` this user turn.
    silent_tool_hops: u32,
    /// Flash-Next silent-tool streak already received one style observation.
    silent_tool_nudged: bool,
}

impl<C: Completer> Agent<C> {
    pub fn new(completer: C, opts: RunOpts) -> Result<Self> {
        let workspace = Workspace::open(&opts.workspace, opts.confined)?;
        let tool_set = if !opts.with_tools {
            ToolSet::None
        } else {
            opts.tool_set
        };
        let (mut system, tools, memory, skills, mcp) = bind_periphery(&opts, &workspace, tool_set);
        if opts.agents_md {
            match read_agents_md(
                workspace.root(),
                opts.agents_md_max_tokens,
                opts.agents_md_head,
            ) {
                AgentsMd::Ok(extra) => {
                    system.push_str("\n\n# AGENTS.md\n");
                    system.push_str(&extra);
                }
                AgentsMd::TooLarge => {
                    if opts.print {
                        eprintln!(
                            "q38: AGENTS.md omitted (over {} tok; pass --agents-md-head to clip)",
                            opts.agents_md_max_tokens
                        );
                    }
                }
                AgentsMd::Missing => {}
            }
        }

        let lossy = opts.low_precision;
        let iter = IterationGate::new(opts.max_steps.max(1));
        let mut gates = vec![
            Gate::from(if lossy {
                DoomLoopGate::lossy()
            } else {
                DoomLoopGate::qwen_default()
            }),
            Gate::from(iter),
            Gate::from(TimeoutGate::new(opts.max_wall)),
        ];
        if lossy {
            gates.push(Gate::from(NameStreakGate::new(4)));
            gates.push(Gate::from(PathLoopGate::new(3)));
            gates.push(Gate::from(ToolCallBudgetGate::new(
                Some(LOSSY_TOOL_BUDGET),
                std::collections::HashMap::new(),
            )));
        }
        let handler = StopHandler::with_gates(gates);
        handler.reset_turn(&opts.session_id);

        let coordinator = ToolCoordinator::new(None);
        coordinator.register_hook(
            "bash",
            Some(opts.bash_timeout_secs),
            Some(COORDINATOR_OWNED_EXEC_TIMEOUT_SECS),
        );
        coordinator.register_hook(
            "run_code",
            Some(opts.bash_timeout_secs),
            Some(COORDINATOR_OWNED_EXEC_TIMEOUT_SECS),
        );
        coordinator.set_offload_on_deadline(true);

        let policy = completer
            .policy()
            .unwrap_or_else(ThinkPolicy::agent_default);
        let (messages, log) = bind_session(&opts, &system, &tools, policy.clone());
        let policy = if opts.effort_locked {
            policy
        } else {
            log.as_ref().and_then(|l| l.policy()).unwrap_or(policy)
        };
        let policy = if lossy {
            policy.apply_lossy_think_cap(opts.effort_locked)
        } else {
            policy
        };
        completer.set_policy(policy.clone());
        completer.set_low_precision(lossy);
        let effort = if lossy {
            EffortController::new(policy.clone(), opts.effort_locked).with_parse_upgrade_after(1)
        } else {
            EffortController::new(policy.clone(), opts.effort_locked)
        };
        let media_caps = completer.media_caps();
        let media_max_bytes = opts.media_max_bytes.max(1);

        let blobs = BlobStore::new(opts.blob_dir.clone().unwrap_or_else(|| {
            opts.session_dir
                .as_ref()
                .map(|d| d.join("blobs"))
                .or_else(|| Config::home_dir().ok().map(|h| h.join("blobs")))
                .unwrap_or_else(|| std::env::temp_dir().join("q38-blobs"))
        }));
        completer.pin_session(&opts.session_id);
        if opts.print {
            if let Some(o) = &opts.working_window_overlay {
                eprintln!(
                    "q38: Q38_WORKING_WINDOW={} overlays config.toml working_window={}; compact uses the env value. Unset the env to use the file.",
                    o.from_env, o.from_file
                );
            }
        }
        let code_index = if has_tool(&tools, "search") {
            Some(CodeIndex::build(workspace.root()))
        } else {
            None
        };
        let web = opts
            .web
            .enabled
            .then(|| crate::tools::WebRunner::new(opts.web.clone(), &mcp));
        Ok(Self {
            completer,
            workspace,
            handler,
            coordinator,
            session_id: opts.session_id,
            messages,
            tools,
            pending_stop: None,
            oracle_cmd: None,
            oracle_runs: 0,
            print: opts.print,
            limits: opts.tool_limits,
            inherit_env: opts.inherit_env,
            working_window: opts.working_window,
            generation_reserve: clamp_generation_reserve(
                opts.working_window,
                opts.generation_reserve,
            ),
            compact_ratio: opts.compact_ratio,
            effort,
            log,
            last_policy: policy,
            blobs,
            memory,
            skills,
            mcp,
            web,
            media_caps,
            media_max_bytes,
            media_bins: opts.media_bins,
            cancel: CancelFlag::new(),
            steer: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            emit: None,
            stdio: std::sync::Arc::new(delta::StdioState::default()),
            plan_mode: opts.plan_mode,
            clarify_mode: opts.clarify_mode,
            permit: opts.permit,
            clarify: opts.clarify,
            low_precision: lossy,
            parse_stop_after: if lossy { 2 } else { 3 },
            last_spoken: None,
            last_essay: None,
            read_paths: HashSet::new(),
            observed_paths: HashSet::new(),
            window_overlay: opts.working_window_overlay,
            edit_guard: guard::EditGuard::new(),
            narrate: opts.narrate && !opts.print && interactive_channel(&opts.channel),
            code_index,
            stutter_nudged: false,
            physics_nudged: false,
            parse_nudged: false,
            channel_nudged: false,
            silent_tool_hops: 0,
            silent_tool_nudged: false,
        })
    }

    pub fn load_messages(&mut self, messages: Vec<ChatMessage>) {
        self.messages = messages;
    }

    pub fn set_cancel(&mut self, cancel: CancelFlag) {
        self.cancel = cancel;
    }

    pub fn set_steer(&mut self, steer: SteerSlot) {
        self.steer = steer;
    }

    pub fn set_emit(&mut self, emit: crate::sidecar::EventSink) {
        self.emit = Some(emit);
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    pub async fn run(&mut self, prompt: &str) -> Result<AgentOutcome> {
        self.run_message(ChatMessage::user(prompt)).await
    }

    /// User turn that may carry QwenPaw `content_parts` (image/video/audio).
    pub async fn run_message(&mut self, msg: ChatMessage) -> Result<AgentOutcome> {
        let raw = msg.text().to_string();
        let (forced_skill, text) = sticky::split_skill_prefix(&raw);
        let (forced_mcp, text) = sticky::split_mcp_prefix(&text);
        let mut msg = msg;
        msg.content = Some(text.clone());
        // 用户附带的媒体与 tool 侧同样落盘，否则 resume 重建后丢图。
        let stored: Vec<crate::session::StoredMedia> = msg
            .parts
            .iter()
            .map(|p| crate::session::StoredMedia {
                kind: p.kind.as_str().into(),
                mime: p.mime.clone(),
                url: p.url.clone(),
            })
            .collect();
        let stubbed = sticky::stub_expired_notes(&mut self.messages);
        self.note_stubbed(stubbed);
        self.messages.push(msg);
        self.log_event(SessionEvent::user(text.clone()).with_media(stored));
        self.compact_at_user_turn();
        self.inject_notes(&text, forced_skill.as_deref(), forced_mcp.as_deref());
        self.inject_window_overlay_note();
        self.inject_locate(&text);
        self.inject_web_hint(&text);
        self.inject_numeric_check_hint(&text);
        self.drive().await
    }

    /// Drive an already-hydrated transcript (last message is the live user).
    pub async fn drive(&mut self) -> Result<AgentOutcome> {
        // Per user turn, not per Agent lifetime. Sidecar/CLI construct a new
        // Agent each RPC, but in-process reuse (TUI, soak, channels) must not
        // inherit iteration/timeout/doom from the previous prompt.
        self.handler.reset_turn(&self.session_id);
        self.stutter_nudged = false;
        self.physics_nudged = false;
        self.parse_nudged = false;
        self.channel_nudged = false;
        self.silent_tool_hops = 0;
        self.silent_tool_nudged = false;
        self.last_spoken = None;
        self.last_essay = None;
        self.read_paths.clear();
        self.observed_paths = observed_from_messages(&self.messages, &self.workspace);
        let user = self.last_real_user().to_string();
        self.edit_guard.reset_turn(&user);
        self.oracle_cmd = None;
        self.snapshot_test_baseline().await;
        let mut steps = 0u32;
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut parse_retries = 0u32;

        loop {
            self.drain_background();
            if self.cancel.is_cancelled() {
                return self.finish(String::new(), Some("aborted".into()), steps);
            }
            // QwenPaw: pending gate TERMINATE fires before the next model call.
            if let Some(reason) = self.pending_stop.take() {
                let text = self.last_spoken.clone().unwrap_or_default();
                if reason.is_empty() || is_physics_stop(&reason) {
                    if !reason.is_empty() {
                        self.note(&reason);
                    }
                    return self.finish(text, None, steps);
                }
                self.note(&reason);
                return self.finish(text, Some(reason), steps);
            }

            if let Some(reason) = self.compact_if_needed() {
                self.note(&reason);
                if self.physics_nudged {
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
                self.physics_nudged = true;
                self.push_hidden_user(PHYSICS_WRAP_NOTE);
            }

            let tools_owned = self.tools.clone();
            let tools = if tools_owned.is_empty() {
                None
            } else {
                Some(tools_owned.as_slice())
            };

            let Some(mut turn) = self.complete_or_abort(tools).await? else {
                return self.finish(String::new(), Some("aborted".into()), steps);
            };
            steps += 1;
            prompt_tokens += turn.prompt_tokens;
            completion_tokens += turn.completion_tokens;

            if turn.watchdog_hit {
                // A cap hit is evidence of a runaway trajectory, not evidence
                // that thinking itself should be disabled. Give the model one
                // concise side observation and more room to choose a course.
                self.note("[watchdog] think cap; soft nudge and one roomy retry");
                self.push_hidden_user(THINK_DIVERGENCE_NOTE);
                let widened = self.retry_with_runaway_room(tools).await;
                if self.cancel.is_cancelled() {
                    return self.finish(String::new(), Some("aborted".into()), steps);
                }
                match widened {
                    Some(t) => {
                        steps += 1;
                        prompt_tokens += t.prompt_tokens;
                        completion_tokens += t.completion_tokens;
                        if !t.watchdog_hit || !t.content.is_empty() || !t.tool_calls.is_empty() {
                            turn = t;
                        }
                    }
                    None => {}
                }
                if turn.watchdog_hit && turn.content.is_empty() && turn.tool_calls.is_empty() {
                    self.mark_clean();
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
            }

            if !turn.reasoning.is_empty() && self.print && !self.stdio.think_streamed() {
                eprintln!("[think]\n{}", turn.reasoning.trim());
            }

            if turn.parse_fail {
                self.effort.note_parse_fail();
                self.sync_effort(PolicyReason::Upgrade);
                parse_retries += 1;
                self.note("[parse] retry");
                if parse_retries >= self.parse_stop_after {
                    if !self.parse_nudged {
                        self.parse_nudged = true;
                        self.push_hidden_user(PARSE_REPAIR_NOTE);
                        continue;
                    }
                    return self.finish(self.last_spoken.clone().unwrap_or_default(), None, steps);
                }
                continue;
            }

            if turn.tool_calls.is_empty() {
                turn.content = strip_greeting_echo(self.last_real_user(), &turn.content);
            }
            if self.low_precision && crate::stutter::is_stutter(&turn.content, &turn.reasoning) {
                if !self.stutter_nudged {
                    self.stutter_nudged = true;
                    self.push_hidden_user(crate::stutter::STUTTER_NOTE);
                    continue;
                }
            }
            if let Some(body) = Self::promote_write_reply(&turn) {
                turn.content = body;
            }
            if let Some(body) = Self::promote_reasoning_reply(&turn) {
                turn.content = body;
            }
            if self.needs_channel_rescue(&turn) {
                if !self.channel_nudged {
                    self.channel_nudged = true;
                    self.note("[channel] empty visible reply; one wrap-up");
                    self.push_hidden_user(EMPTY_CHANNEL_NOTE);
                    continue;
                }
                self.mark_clean();
                return self.finish_visible(String::new(), None, steps);
            }
            let mut trajectory_note = None;
            let dump_anchor = self.last_spoken.clone().or_else(|| self.last_essay.clone());
            if let Some(prev) = dump_anchor {
                let quote_dump = turn.tool_calls.is_empty()
                    && crate::stutter::is_blockquote_heavy(&turn.content)
                    && crate::stutter::is_substantial_reply(&prev);
                if quote_dump || self.is_answer_dump_hop(&prev, &turn) {
                    let keep = self.last_spoken.clone().unwrap_or(prev);
                    if turn.tool_calls.is_empty() {
                        // No tools means the model chose to stop. Keep the first
                        // identical bubble without labelling that choice a
                        // harness failure.
                        self.mark_clean();
                        return self.finish(keep, None, steps);
                    }
                    trajectory_note = Some(crate::stutter::DUMP_NOTE);
                }
            }
            self.push_assistant(&turn);
            if crate::stutter::is_substantial_reply(&turn.content)
                && !crate::stutter::is_blockquote_heavy(&turn.content)
            {
                self.last_essay = Some(turn.content.clone());
            }
            if Self::hop_locks_spoken(&turn) {
                self.last_spoken = Some(turn.content.clone());
            }
            let decision = self.gate_decision(&turn, steps, prompt_tokens, completion_tokens);

            if !turn.tool_calls.is_empty() {
                if turn.content.trim().is_empty() {
                    self.silent_tool_hops += 1;
                } else {
                    self.silent_tool_hops = 0;
                }
                // Defer TERMINATE until after this tool batch. A gate Continue
                // with text is a one-shot trajectory observation after the
                // batch's results; later repeats stay silent.
                let mut gate_note = None;
                let mut silent_note = None;
                match &decision {
                    GateDecision::Stop { reason } if is_physics_stop(reason) => {
                        if !self.physics_nudged {
                            self.physics_nudged = true;
                            self.note(reason);
                            gate_note = Some(PHYSICS_WRAP_NOTE.to_string());
                        } else {
                            self.note(reason);
                            self.pending_stop = Some(String::new());
                        }
                    }
                    GateDecision::Stop { reason } if !reason.is_empty() => {
                        self.pending_stop = Some(reason.clone());
                    }
                    GateDecision::Continue { continuation, .. } if !continuation.is_empty() => {
                        gate_note = Some(continuation.clone());
                    }
                    _ => {}
                }
                if self.silent_final_channel()
                    && !self.silent_tool_nudged
                    && self.silent_tool_hops >= SILENT_TOOL_STREAK
                {
                    self.silent_tool_nudged = true;
                    self.note("[channel] silent tool streak; one style observation");
                    silent_note = Some(SILENT_TOOL_NOTE.to_string());
                }
                let calls = std::mem::take(&mut turn.tool_calls);
                if trajectory_note.is_some() {
                    // Do not execute a cleanup/write batch before the model has
                    // seen the divergence observation. Record well-formed tool
                    // results, then give control straight back to the model.
                    self.defer_divergent_tools(calls);
                } else {
                    self.execute_tools(calls).await;
                }
                if let Some(note) = gate_note {
                    self.push_hidden_user(note);
                }
                if let Some(note) = silent_note {
                    self.push_hidden_user(note);
                }
                if let Some(note) = trajectory_note {
                    self.push_hidden_user(note);
                }
                self.flush_steer();
                if self.cancel.is_cancelled() {
                    return self.finish(String::new(), Some("aborted".into()), steps);
                }
                continue;
            }

            match decision {
                GateDecision::Continue { continuation, .. } => {
                    self.mark_clean();
                    let stop_reason = if continuation.is_empty() {
                        None
                    } else {
                        Some(continuation)
                    };
                    return self.finish_visible(turn.content, stop_reason, steps);
                }
                GateDecision::Stop { reason } => {
                    self.mark_clean();
                    if is_physics_stop(&reason) {
                        self.note(&reason);
                        return self.finish_visible(turn.content, None, steps);
                    }
                    let stop_reason = if reason.is_empty() {
                        None
                    } else {
                        Some(reason)
                    };
                    return self.finish_visible(turn.content, stop_reason, steps);
                }
                // Handler swallows per-gate Bypass; keep this arm so a future
                // handler contract change cannot panic the loop.
                GateDecision::Bypass => {
                    self.mark_clean();
                    return self.finish_visible(turn.content, None, steps);
                }
            }
        }
    }

    fn push_assistant(&mut self, turn: &ModelTurn) {
        let reasoning = empty_to_none(&turn.reasoning);
        let tool_calls = if turn.tool_calls.is_empty() {
            None
        } else {
            Some(
                turn.raw_tool_calls
                    .as_ref()
                    .map(|raw| normalize_tool_calls(raw))
                    .unwrap_or_else(|| openai_tool_calls(&turn.tool_calls)),
            )
        };
        let content = if tool_calls.is_none() {
            Some(turn.content.clone())
        } else {
            empty_to_none(&turn.content)
        };
        self.messages
            .push(ChatMessage::assistant_reply(content, reasoning, tool_calls));
        self.log_event(SessionEvent::assistant_usage(
            turn.content.clone(),
            turn.reasoning.clone(),
            if turn.tool_calls.is_empty() {
                None
            } else {
                Some(openai_stored(&turn.tool_calls))
            },
            turn.prompt_tokens,
            turn.completion_tokens,
            turn.cached_tokens,
            turn.decode_tok_s,
        ));
    }

    fn flush_steer(&mut self) {
        for note in take_steer(&self.steer) {
            self.push_hidden_user(format!("Steer: {note}"));
        }
    }

    fn drain_background(&mut self) {
        for (name, response) in self.coordinator.take_finished() {
            let status = match response.state {
                ToolState::Success => "finished",
                ToolState::Error => "failed",
                ToolState::Interrupted => "interrupted",
            };
            self.note(&format!("[background {name} {status}]"));
            let mut body = format!(
                "[background {name} {status} id={}]\n{}",
                response.id,
                response.joined_text()
            );
            if let Some(blob) = &response.blob {
                body.push_str(&format!("\n[blob {blob}]"));
            }
            self.push_hidden_user(body);
        }
    }

    fn push_hidden_user(&mut self, text: impl AsRef<str>) {
        let wrapped = wrap_tool_response(text.as_ref());
        self.messages.push(ChatMessage::user(wrapped.clone()));
        self.log_event(SessionEvent::user(wrapped));
    }

    /// Spoken answer already exists, and this hop is only dump/placeholder/cleanup.
    /// Unique docs, edits, first-time reads, grep/test, and rereads-only still continue.
    fn is_answer_dump_hop(&self, spoken: &str, turn: &ModelTurn) -> bool {
        if !crate::stutter::is_substantial_reply(spoken) {
            return false;
        }
        if turn.tool_calls.is_empty() {
            return crate::stutter::is_restated_reply(spoken, &turn.content);
        }
        let restated = crate::stutter::is_restated_reply(spoken, &turn.content);
        if crate::stutter::is_substantial_reply(&turn.content) && !restated {
            return false;
        }
        let dump = turn
            .tool_calls
            .iter()
            .any(|c| self.is_dump_tool(spoken, &turn.content, c));
        let work = turn
            .tool_calls
            .iter()
            .any(|c| self.is_work_tool(spoken, &turn.content, c));
        dump && !work
    }

    /// Lock the visible answer only when this hop is a delivery, not exploration.
    /// Reads-only narration (the 27B "let me check X" paragraph) must not freeze the turn.
    fn hop_locks_spoken(turn: &ModelTurn) -> bool {
        if !crate::stutter::is_substantial_reply(&turn.content) {
            return false;
        }
        if turn.tool_calls.is_empty() {
            return true;
        }
        let mut dumpish = false;
        for call in &turn.tool_calls {
            match call.name.as_str() {
                "read" | "view" => {}
                "write" => {
                    let (path, body) = write_path_body(call);
                    if crate::stutter::is_placeholder_write(path, body)
                        || crate::stutter::is_restated_reply(&turn.content, body)
                    {
                        dumpish = true;
                    } else {
                        return false;
                    }
                }
                "bash" => {
                    let cmd = bash_cmd(call);
                    if crate::stutter::is_cleanup_bash(cmd) {
                        dumpish = true;
                    } else {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        dumpish
    }

    fn is_dump_tool(&self, spoken: &str, content: &str, call: &ToolCall) -> bool {
        match call.name.as_str() {
            "write" => {
                let (path, body) = write_path_body(call);
                crate::stutter::is_placeholder_write(path, body)
                    || crate::stutter::is_restated_reply(spoken, body)
                    || (!content.trim().is_empty()
                        && crate::stutter::is_restated_reply(content, body))
            }
            "bash" => crate::stutter::is_cleanup_bash(bash_cmd(call)),
            _ => false,
        }
    }

    fn is_work_tool(&self, spoken: &str, content: &str, call: &ToolCall) -> bool {
        match call.name.as_str() {
            "read" | "view" => {
                let Some(path) = fs_tool_path(&call.name, &call.arguments) else {
                    return true;
                };
                !self
                    .read_paths
                    .contains(&canon_ws_path(&self.workspace, &path))
            }
            "write" | "bash" => !self.is_dump_tool(spoken, content, call),
            _ => true,
        }
    }

    fn promote_write_reply(turn: &ModelTurn) -> Option<String> {
        let bodies: Vec<&str> = turn
            .tool_calls
            .iter()
            .filter(|c| c.name == "write")
            .filter_map(|c| c.arguments.get("content").and_then(|v| v.as_str()))
            .collect();
        crate::stutter::promote_dumped_reply(&turn.content, &bodies)
    }

    fn silent_final_channel(&self) -> bool {
        self.completer
            .prefix_meter()
            .is_some_and(|(f, _)| f.silent_final_channel())
    }

    /// Empty visible hop: leftover think, or a naked empty stop with nothing
    /// already spoken. Do not dump unfinished CoT; do not deliver `""`.
    fn needs_channel_rescue(&self, turn: &ModelTurn) -> bool {
        if !turn.tool_calls.is_empty() || !turn.content.trim().is_empty() {
            return false;
        }
        if !turn.reasoning.trim().is_empty() {
            return true;
        }
        self.visible_stop_text().is_empty()
    }

    /// Lift a finished answer that landed only in `reasoning_content`.
    /// Scratch plans and long CoT stay in think and get a wrap-up hop.
    fn promote_reasoning_reply(turn: &ModelTurn) -> Option<String> {
        if !turn.tool_calls.is_empty() || !turn.content.trim().is_empty() {
            return None;
        }
        let r = turn.reasoning.trim();
        if crate::stutter::is_scratch_think(r) {
            return None;
        }
        if !crate::stutter::is_substantial_reply(r) {
            return None;
        }
        if r.chars().count() > PROMOTE_REASONING_MAX {
            return None;
        }
        Some(r.to_string())
    }

    fn visible_stop_text(&self) -> String {
        self.last_spoken
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.last_essay
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    fn finish_visible(
        &mut self,
        content: String,
        stop_reason: Option<String>,
        steps: u32,
    ) -> Result<AgentOutcome> {
        let text = if content.trim().is_empty() {
            let kept = self.visible_stop_text();
            if kept.is_empty() {
                EMPTY_STOP_FALLBACK.to_string()
            } else {
                kept
            }
        } else {
            content
        };
        self.finish(text, stop_reason, steps)
    }

    fn gate_decision(
        &self,
        turn: &ModelTurn,
        steps: u32,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> GateDecision {
        let fingerprints: Vec<ToolFingerprint> = turn
            .tool_calls
            .iter()
            .map(|c| {
                ToolFingerprint::new(&c.name, &c.arguments.to_string())
                    .with_path(fs_tool_path(&c.name, &c.arguments))
            })
            .collect();
        let names: Vec<String> = turn.tool_calls.iter().map(|c| c.name.clone()).collect();
        let mut ctx = GateCtx::new(&self.session_id);
        ctx.iteration = steps;
        ctx.prompt_tokens = turn.prompt_tokens;
        ctx.completion_tokens = turn.completion_tokens;
        ctx.tokens_used = prompt_tokens + completion_tokens;
        ctx.tool_names = &names;
        ctx.fingerprints = &fingerprints;
        ctx.last_tool = fingerprints.last();
        self.handler.run(&ctx)
    }

    async fn execute_tools(&mut self, calls: Vec<ToolCall>) {
        for call in &calls {
            self.note(&format!("[{}] {}", call.name, preview_args(call)));
        }
        let write_priors = self.snapshot_write_priors(&calls);
        let mut responses = if parallel_safe_batch(&calls) {
            self.dispatch_parallel(&calls).await
        } else {
            let mut out = Vec::with_capacity(calls.len());
            for call in &calls {
                out.push(self.dispatch_one(call).await);
            }
            out
        };
        for (call, response) in calls.iter().zip(responses.iter_mut()) {
            self.fold_bash_search(call, response);
        }
        let fail_blob: String = responses
            .iter()
            .map(|r| r.joined_text())
            .collect::<Vec<_>>()
            .join("\n");
        let mut harness = false;
        let mut guard_notes: Vec<guard::GuardNote> = Vec::new();
        for (call, response) in calls.iter().zip(responses) {
            if is_harness_fail(&response) {
                harness = true;
            }
            if matches!(call.name.as_str(), "read" | "view") && response.state == ToolState::Success
            {
                if let Some(path) = fs_tool_path(&call.name, &call.arguments) {
                    self.read_paths
                        .insert(canon_ws_path(&self.workspace, &path));
                }
            }
            if matches!(call.name.as_str(), "read" | "view" | "write" | "edit")
                && response.state == ToolState::Success
            {
                if let Some(path) = fs_tool_path(&call.name, &call.arguments) {
                    self.observed_paths
                        .insert(canon_ws_path(&self.workspace, &path));
                }
            }
            if response.state == ToolState::Success {
                let prior = write_priors.get(&call.id).map(|s| s.as_str());
                guard_notes.extend(self.edit_guard.observe(&call.name, &call.arguments, prior));
                if matches!(call.name.as_str(), "edit" | "write") {
                    if let (Some(idx), Some(path)) = (
                        self.code_index.as_ref(),
                        fs_tool_path(&call.name, &call.arguments),
                    ) {
                        idx.refresh(&self.workspace, &path);
                    }
                }
            }
            if let Some(note) = self
                .edit_guard
                .observe_tool_output(&call.name, &response.joined_text())
            {
                guard_notes.push(note);
            }
            self.commit_tool(&call.name, response);
        }
        if harness {
            let _ = self.effort.note_harness_fail();
            self.sync_effort(PolicyReason::Upgrade);
        } else {
            self.mark_clean();
        }
        let mut thrash = false;
        for note in guard_notes {
            thrash |= note == guard::GuardNote::Thrash;
            self.apply_guard_note(note);
        }
        self.oracle_tests_if_needed().await;
        if thrash && self.effort.note_thrash() {
            self.sync_effort(PolicyReason::Upgrade);
        }
        self.inject_skill_from_tools(&fail_blob);
    }

    fn defer_divergent_tools(&mut self, calls: Vec<ToolCall>) {
        for call in calls {
            self.note(&format!("[{}] deferred low-information batch", call.name));
            self.commit_tool(
                &call.name,
                ToolResponse::text(
                    &call.id,
                    "Deferred: this batch repeated the visible answer or only staged/cleaned a scratch copy. Reassess using the trajectory observation, then choose the next step.",
                    ToolState::Error,
                ),
            );
        }
    }

    fn apply_guard_note(&mut self, note: guard::GuardNote) {
        self.note(&format!("[guard] {}", note.label()));
        self.push_hidden_user(note.text());
    }

    /// Sample a cheap suite before edits in `--print` only, so a later red run
    /// can be told apart from a tree that was already red. Interactive turns
    /// skip this and only run the post-edit scoped oracle.
    async fn snapshot_test_baseline(&mut self) {
        if !self.print || self.plan_mode || self.oracle_cmd.is_some() {
            return;
        }
        if !verify::workspace_has_tests(self.workspace.root()) {
            return;
        }
        let Some(cmd) = verify::workspace_default_test_cmd(self.workspace.root()) else {
            return;
        };
        let started = std::time::Instant::now();
        let out = self.run_oracle(&cmd).await;
        if !guard::is_test_output("bash", &out) {
            return;
        }
        if started.elapsed() > ORACLE_MAX_SUITE {
            self.note("[oracle] suite too slow; baseline only");
        } else {
            self.oracle_cmd = Some(cmd);
        }
        let red = guard::is_test_fail("bash", &out);
        self.edit_guard.set_baseline(red);
        self.push_hidden_user(format!(
            "[baseline] 改动前测试{}。\n{}",
            if red { "已经是红的" } else { "全绿" },
            tail_chars(&out, ORACLE_TAIL_CHARS)
        ));
    }

    /// After a successful code edit, run a scoped test command and feed the
    /// tail back. Not gated on user keywords. Skips office docs, plan mode,
    /// and turns where the model already ran tests.
    async fn oracle_tests_if_needed(&mut self) {
        if self.plan_mode || self.pending_stop.is_some() || !self.edit_guard.wants_oracle() {
            return;
        }
        let cmd = verify::scoped_test_cmd(self.workspace.root(), self.edit_guard.code_paths())
            .or_else(|| self.oracle_cmd.clone());
        let Some(cmd) = cmd else {
            self.edit_guard.mark_oracle_ran();
            return;
        };
        let out = self.run_oracle(&cmd).await;
        if let Some(note) = self.edit_guard.observe_oracle_output(&out) {
            self.apply_guard_note(note);
        }
        let red = guard::is_test_fail("bash", &out);
        if red && self.effort.note_test_fail() {
            self.sync_effort(PolicyReason::Upgrade);
        } else if !red {
            self.effort.note_tests_green();
        }
        self.push_hidden_user(format!("[oracle]\n{}", tail_chars(&out, ORACLE_TAIL_CHARS)));
        if guard::is_test_output("bash", &out) {
            self.oracle_cmd = Some(cmd);
        }
    }

    /// The oracle uses Python's portable `-B` switch. Avoiding bytecode is both
    /// cheaper than managing a throwaway cache and works unchanged in Bash on
    /// macOS/Linux/Git Bash and in the PowerShell fallback.
    async fn run_oracle(&mut self, cmd: &str) -> String {
        self.note(&format!("[oracle] {cmd}"));
        let call = ToolCall {
            id: format!("oracle-{}", self.oracle_runs),
            name: "bash".into(),
            arguments: json!({"command": cmd}),
        };
        self.oracle_runs += 1;
        self.dispatch_one(&call).await.joined_text()
    }

    fn snapshot_write_priors(&self, calls: &[ToolCall]) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for call in calls {
            if call.name != "write" {
                continue;
            }
            let Some(path) = fs_tool_path("write", &call.arguments) else {
                continue;
            };
            let Ok(abs) = self.workspace.resolve(&path) else {
                continue;
            };
            if let Ok(body) = std::fs::read_to_string(abs) {
                out.insert(call.id.clone(), body);
            }
        }
        out
    }

    fn fold_bash_search(&self, call: &ToolCall, response: &mut ToolResponse) {
        if call.name != "bash" {
            return;
        }
        let Some(query) = bash_search_query(bash_cmd(call)) else {
            return;
        };
        if !search_dump_too_big(&response.joined_text()) {
            return;
        }
        let Some(idx) = &self.code_index else {
            return;
        };
        let Some(spans) = idx.render_query(&query, None) else {
            return;
        };
        let full = response.joined_text();
        if !search_fold_shrinks(&full, &spans) {
            return;
        }
        let blob = response
            .blob
            .clone()
            .or_else(|| self.blobs.put(full.as_bytes()).ok());
        let mut folded = ToolResponse::text(
            call.id.clone(),
            search_fold_text(&full, &query, &spans, blob.as_deref()),
            response.state.clone(),
        );
        folded.blob = blob;
        folded.original_chars = full.chars().count();
        *response = folded;
    }

    fn inject_notes(&mut self, user: &str, forced_skill: Option<&str>, forced_mcp: Option<&str>) {
        if !sticky::live_has_memory_note(&self.messages) {
            if let Some(store) = &self.memory {
                if let Some(md) = store.read_memory_md() {
                    if let Some(card) = memory_card(user, &md) {
                        self.push_hidden_user(card);
                    }
                }
            }
        }
        if forced_skill.is_some() {
            let stubbed = sticky::stub_live_skill_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if !sticky::live_has_skill_note(&self.messages) {
            let skill = forced_skill
                .and_then(|n| self.skills.get(n))
                .or_else(|| match_user(&self.skills, user));
            if let Some(sk) = skill {
                match hidden_card(sk) {
                    Some(card) => self.push_hidden_user(card),
                    None => self.note(&format!(
                        "q38: skill {} over {} tok; not injected",
                        sk.name,
                        sticky::SKILL_BODY_MAX_TOKENS
                    )),
                }
            }
        }
        if forced_mcp.is_some() {
            let stubbed = sticky::stub_live_mcp_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if crate::tools_schema::has_tool(&self.tools, "mcp")
            && !sticky::live_has_mcp_note(&self.messages)
        {
            if let Some(card) = mcp_card(&self.mcp, user, forced_mcp) {
                self.push_hidden_user(card);
            }
        }
        if self.plan_mode && !sticky::live_has_plan_note(&self.messages) {
            self.push_hidden_user(PLAN_CARD);
        }
        if (self.plan_mode || self.clarify_mode) && !sticky::live_has_clarify_note(&self.messages) {
            self.push_hidden_user(crate::clarify::CLARIFY_CARD);
        }
        if self.narrate && !sticky::live_has_style_note(&self.messages) {
            self.push_hidden_user(sticky::STYLE_CARD);
        }
        if crate::cron::wants_cron_card(user) && !sticky::live_has_cron_note(&self.messages) {
            self.push_hidden_user(crate::cron::CRON_CARD);
        }
    }

    fn inject_window_overlay_note(&mut self) {
        let Some(o) = self.window_overlay.take() else {
            return;
        };
        self.push_hidden_user(format!(
            "Q38_WORKING_WINDOW={} overlays config.toml working_window={}. Live window is {}. Unset Q38_WORKING_WINDOW to use the file. Page large files with read(path, offset, limit).",
            o.from_env, o.from_file, self.working_window
        ));
    }

    /// Local weights are weakest exactly on post-cutoff facts. When the task
    /// smells like fresh-world knowledge and `web` is armed, one short hidden
    /// fact names the tool — instead of a standing lecture in every prompt.
    fn inject_web_hint(&mut self, user: &str) {
        if !crate::tools_schema::has_tool(&self.tools, "web") {
            return;
        }
        if forbids_tools(user) || !wants_web_check(user) {
            return;
        }
        self.push_hidden_user(WEB_HINT);
    }

    /// Quantitative reasoning gets one short, task-local self-check cue. It is
    /// absent from ordinary chat and coding turns, and does not force another
    /// model hop: the model keeps control over when its answer is ready.
    fn inject_numeric_check_hint(&mut self, user: &str) {
        if wants_numeric_check(user) {
            self.push_hidden_user(NUMERIC_CHECK_HINT);
        }
    }

    fn inject_locate(&mut self, user: &str) {
        if forbids_tools(user) || !wants_auto_locate(user) {
            return;
        }
        let Some(idx) = &self.code_index else {
            return;
        };
        let Some(spans) = idx.render_query(user, None) else {
            return;
        };
        self.push_hidden_user(format!("[locate]\n{spans}"));
    }

    fn inject_skill_from_tools(&mut self, output: &str) {
        let Some(sk) = match_tool_output(&self.skills, output) else {
            return;
        };
        if sticky::live_has_skill_note(&self.messages) {
            let stubbed = sticky::stub_live_skill_notes(&mut self.messages);
            self.note_stubbed(stubbed);
        }
        if let Some(card) = hidden_card(sk) {
            self.push_hidden_user(card);
        }
    }

    async fn gate_tool(&self, call: &ToolCall) -> Option<ToolResponse> {
        let name = call.name.as_str();
        if self.plan_mode && permit::is_mutating(name) {
            return Some(ToolResponse::text(
                &call.id,
                permit::plan_denied(name),
                ToolState::Error,
            ));
        }
        let Some(hub) = &self.permit else {
            return None;
        };
        match hub.check(name, &preview_args(call), &self.cancel).await {
            PermitDecision::Allow => None,
            PermitDecision::Always => {
                hub.remember(name);
                None
            }
            PermitDecision::Deny => Some(ToolResponse::text(
                &call.id,
                permit::user_denied(name),
                ToolState::Error,
            )),
        }
    }

    /// dsh `FS_NOT_OBSERVED`, narrowed to the one destructive case: `write`
    /// replaces the whole file, so overwriting one the transcript never saw
    /// destroys content the model cannot know. `edit` needs no version guard —
    /// its exact `old_string` match is already a content CAS. Costs one `read`
    /// only when it actually fires.
    fn refuse_blind_overwrite(&self, call: &ToolCall) -> Option<ToolResponse> {
        if call.name != "write" {
            return None;
        }
        let raw = fs_tool_path(&call.name, &call.arguments)?;
        if self
            .observed_paths
            .contains(&canon_ws_path(&self.workspace, &raw))
        {
            return None;
        }
        let abs = self.workspace.resolve(&raw).ok()?;
        if !abs.is_file() {
            return None;
        }
        Some(ToolResponse::text(
            &call.id,
            format!(
                "Error: {raw} 已存在，且本会话未读过它。write 会整文件覆盖。先 read(path=\"{raw}\") 确认内容；只改局部就用 edit。"
            ),
            ToolState::Error,
        ))
    }

    async fn complete_or_abort(&self, tools: Option<&[Value]>) -> Result<Option<ModelTurn>> {
        let prev = self.widen_no_tool_think();
        let result = self.complete_resilient(tools).await;
        if let Some(p) = prev {
            self.completer.set_policy(p);
        }
        result
    }

    /// One model hop. Transient endpoint drops retry with backoff so a flaky
    /// path continues the same turn (tools already run stay) instead of erroring.
    async fn complete_resilient(&self, tools: Option<&[Value]>) -> Result<Option<ModelTurn>> {
        let started = std::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            if self.cancel.is_cancelled() {
                return Ok(None);
            }
            self.arm_sink();
            let result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(None),
                turn = self.completer.complete(&self.messages, tools) => turn,
            };
            match result {
                Ok(turn) => return Ok(Some(turn)),
                Err(e) if crate::llm_http::is_transient(&e) => {
                    attempt += 1;
                    if started.elapsed() >= crate::llm_http::RETRY_BUDGET {
                        return Err(e);
                    }
                    let wait = crate::llm_http::retry_delay(attempt);
                    self.signal_net_retry(attempt, wait, &e);
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return Ok(None),
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn signal_net_retry(&self, attempt: u32, wait: Duration, err: &crate::error::Error) {
        let line = crate::llm_http::retry_status_line(attempt, wait);
        self.note(&format!("[net] {line}{err}"));
        if let Some(sink) = self.live_sink() {
            sink.reset();
            sink.reasoning(&line);
        }
    }

    /// A turn that forbids tools spends its whole budget on one answer, so the
    /// runaway cap is the only thing bounding depth. Raise it for this
    /// completion only; restore afterwards so a later coding hop keeps the
    /// session policy. `--think` lock is honored (left alone).
    fn widen_no_tool_think(&self) -> Option<ThinkPolicy> {
        if self.effort.user_locked {
            return None;
        }
        let prev = self.completer.policy()?;
        if !prev.enabled || prev.max_think_tokens >= NO_TOOL_THINK_FLOOR {
            return None;
        }
        if !forbids_tools(self.last_real_user()) {
            return None;
        }
        let mut raised = prev.clone();
        raised.max_think_tokens = NO_TOOL_THINK_FLOOR;
        raised.max_tokens = raised
            .max_tokens
            .max(NO_TOOL_THINK_FLOOR + NO_TOOL_ANSWER_RESERVE);
        self.completer.set_policy(raised);
        Some(prev)
    }

    async fn run_ask(&self, call: &ToolCall) -> ToolResponse {
        let armed = self.plan_mode || self.clarify_mode;
        if !armed {
            return ToolResponse::text(
                &call.id,
                "Error: ask is off. Proceed without asking. User can /clarify or /plan.",
                ToolState::Error,
            );
        }
        let ask = match crate::clarify::parse_ask(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error);
            }
        };
        let yolo = self
            .permit
            .as_ref()
            .map(|p| p.mode() == crate::permit::ApprovalMode::Yolo)
            .unwrap_or(true);
        let decision = if yolo {
            crate::clarify::ClarifyDecision::Skip
        } else if let Some(hub) = &self.clarify {
            hub.ask(ask.clone(), &self.cancel).await
        } else {
            crate::clarify::ClarifyDecision::Skip
        };
        ToolResponse::text(
            &call.id,
            crate::clarify::format_decision(&ask, decision),
            ToolState::Success,
        )
    }

    fn arm_sink(&self) {
        self.completer.set_token_sink(self.live_sink());
    }

    fn live_sink(&self) -> Option<TokenSink> {
        if let Some(emit) = &self.emit {
            Some(TokenSink::events(emit.clone()))
        } else if self.print {
            Some(TokenSink::stdio(self.stdio.clone()))
        } else {
            None
        }
    }

    async fn dispatch_one(&self, call: &ToolCall) -> ToolResponse {
        if let Some(denied) = self.gate_tool(call).await {
            return denied;
        }
        if let Some(refused) = self.refuse_blind_overwrite(call) {
            return refused;
        }
        match call.name.as_str() {
            "ask" => self.run_ask(call).await,
            "recall" => run_recall(self.log.as_ref(), &self.blobs, call, self.limits),
            "memory_search" => match &self.memory {
                Some(store) => run_memory_search(store, call, self.limits),
                None => ToolResponse::text(
                    &call.id,
                    "Error: memory store unavailable.",
                    ToolState::Error,
                ),
            },
            "search" => match &self.code_index {
                Some(idx) => run_search(idx, call, self.limits),
                None => {
                    ToolResponse::text(&call.id, "Error: code index unavailable.", ToolState::Error)
                }
            },
            "web" => {
                let Some(web) = self.web.clone() else {
                    return ToolResponse::text(
                        &call.id,
                        "Error: web 工具未启用（config.toml [web] enabled）。",
                        ToolState::Error,
                    );
                };
                let blobs = self.blobs.clone();
                let limits = self.limits;
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "q38", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(agent_cancel, per_call);
                        let res = tokio::select! {
                            biased;
                            _ = merged.cancelled() => ToolResponse::text(
                                &owned.id,
                                "Error: tool task aborted",
                                ToolState::Interrupted,
                            ),
                            r = web.run(&owned, limits, Some(&blobs)) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            "mcp" => {
                let mcp = self.mcp.clone();
                let blobs = self.blobs.clone();
                let limits = self.limits;
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "q38", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(agent_cancel, per_call);
                        let res = tokio::select! {
                            biased;
                            _ = merged.cancelled() => ToolResponse::text(
                                &owned.id,
                                "Error: tool task aborted",
                                ToolState::Interrupted,
                            ),
                            r = run_mcp(&mcp, &owned, limits, Some(&blobs)) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            // Not in tools[]. XML / hallucinated native calls still need a result.
            "skill" => run_skill(&self.skills, call, self.limits, Some(&self.blobs)),
            "view" => {
                let ws = self.workspace.clone();
                let caps = self.media_caps.clone();
                let bins = self.media_bins.clone();
                let max_bytes = self.media_max_bytes;
                let owned = call.clone();
                let agent_cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "q38", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(agent_cancel, per_call);
                        let res = tokio::select! {
                            biased;
                            _ = merged.cancelled() => ToolResponse::text(
                                &owned.id,
                                "Error: tool task aborted",
                                ToolState::Interrupted,
                            ),
                            r = view(&ws, &owned, &caps, &bins, max_bytes) => r,
                        };
                        link.abort();
                        res
                    })
                    .await
            }
            _ => {
                let ws = self.workspace.clone();
                let limits = self.limits;
                let inherit_env = self.inherit_env;
                let blobs = self.blobs.clone();
                let owned = call.clone();
                let cancel = self.cancel.clone();
                self.coordinator
                    .execute(call.clone(), "q38", None, move |per_call| async move {
                        let (merged, link) = spawn_cancel_bridge(cancel, per_call);
                        let res =
                            run_tool(&ws, &owned, merged, limits, inherit_env, Some(&blobs)).await;
                        link.abort();
                        res
                    })
                    .await
            }
        }
    }

    async fn dispatch_parallel(&self, calls: &[ToolCall]) -> Vec<ToolResponse> {
        // Same handlers as the serial path. `parallel_safe_batch` admits
        // read/view/search/web/ask; mutating tools still run serially.
        futures::future::join_all(calls.iter().map(|c| self.dispatch_one(c))).await
    }

    fn commit_tool(&mut self, name: &str, response: ToolResponse) {
        let stored: Vec<crate::session::StoredMedia> = response
            .media
            .iter()
            .map(|p| crate::session::StoredMedia {
                kind: p.kind.as_str().into(),
                mime: p.mime.clone(),
                url: p.url.clone(),
            })
            .collect();
        if response.media.is_empty() {
            self.messages
                .push(ChatMessage::tool(&response.id, response.joined_text()));
        } else {
            self.messages.push(ChatMessage::tool_media(
                &response.id,
                response.joined_text(),
                response.media.clone(),
            ));
        }
        self.log_event(
            SessionEvent::tool_folded(
                &response.id,
                name,
                response.joined_text(),
                response.blob.clone(),
                response
                    .blob
                    .as_ref()
                    .map(|_| response.original_chars as u64),
            )
            .with_media(stored),
        );
    }

    /// After a true think-cap hit, preserve the model's selected reasoning mode
    /// and give it one wider retry. The hidden trajectory note is injected by
    /// the caller; a second cap hit is a hard resource exhaustion, not a signal
    /// to replace the model's choice with a thinking-off answer.
    async fn retry_with_runaway_room(&self, tools: Option<&[Value]>) -> Option<ModelTurn> {
        let Some(prev) = self.completer.policy() else {
            self.arm_sink();
            let retry = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => None,
                turn = self.completer.complete(&self.messages, tools) => turn.ok(),
            };
            return retry.filter(|t| !t.watchdog_hit && !t.parse_fail);
        };
        if !prev.enabled {
            return None;
        }
        let mut raised = prev.clone();
        raised.max_think_tokens = raised.max_think_tokens.max(NO_TOOL_THINK_FLOOR);
        raised.max_tokens = raised
            .max_tokens
            .max(raised.max_think_tokens + NO_TOOL_ANSWER_RESERVE);
        self.completer.set_policy(raised);
        self.arm_sink();
        let retry = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => None,
            turn = self.completer.complete(&self.messages, tools) => turn.ok(),
        };
        self.completer.set_policy(prev);
        retry.filter(|t| !t.watchdog_hit && !t.parse_fail)
    }

    fn last_real_user(&self) -> &str {
        self.messages
            .iter()
            .rev()
            .filter(|m| m.role == "user")
            .find(|m| !is_hidden_user_text(m.content.as_deref().unwrap_or("")))
            .and_then(|m| m.content.as_deref())
            .unwrap_or("")
    }

    fn mark_clean(&mut self) {
        if self.effort.note_clean_step() {
            self.sync_effort(PolicyReason::Downgrade);
        }
    }

    fn sync_effort(&mut self, reason: PolicyReason) {
        let p = self.effort.policy().clone();
        if self.last_policy == p {
            return;
        }
        self.completer.set_policy(p.clone());
        self.log_event(SessionEvent::policy(p.clone(), reason));
        self.last_policy = p;
    }

    fn log_event(&mut self, event: SessionEvent) {
        if event.is_ephemeral() {
            if let Some(sink) = &self.emit {
                sink.append(event);
            }
            return;
        }
        if let Some(sink) = &self.emit {
            // spawn_turn already notified the live user; hidden skill/MEMORY/MCP
            // cards stay off the TUI bubble list.
            if !matches!(event, SessionEvent::User(_)) {
                sink.append(event.clone());
            }
        }
        if let Some(log) = self.log.as_mut() {
            let _ = log.append(event);
        }
    }

    fn finish(
        &mut self,
        text: String,
        stop_reason: Option<String>,
        steps: u32,
    ) -> Result<AgentOutcome> {
        self.drain_background();
        if stop_reason.as_deref() == Some("aborted") {
            self.coordinator.cancel_background();
        }
        self.mark_clean();
        let reason = stop_reason.clone().unwrap_or_else(|| "stop".into());
        self.log_event(SessionEvent::stop(reason));
        Ok(AgentOutcome {
            text,
            stop_reason,
            steps,
            session_id: self.session_id.clone(),
            pending_steer: take_steer(&self.steer),
            streamed_text: self.stdio.text_streamed(),
        })
    }

    fn compact_if_needed(&mut self) -> Option<String> {
        if self.working_window == 0 {
            return None;
        }
        if !self.over_soft_window() {
            return None;
        }
        for _ in 0..2 {
            if !self.apply_compact_pass() {
                break;
            }
            if !self.over_soft_window() {
                return None;
            }
        }
        if !self.over_hard_window() {
            return None;
        }
        let n = self.prefix_tokens().unwrap_or(0);
        Some(format!(
            "budget:context ({n} prefix + {} reserve > {} window)",
            self.generation_reserve, self.working_window
        ))
    }

    /// Archive previous turns at the start of a follow-up user message so a
    /// finished long turn is not replayed as a cold prefill.
    ///
    /// Do not Jinja+HF-tokenize the fat transcript first. Local llama.cpp
    /// metering is expensive and sat in front of the first hop with no
    /// thinking tokens.
    fn compact_at_user_turn(&mut self) {
        if self.working_window == 0 {
            return;
        }
        if !self.can_apply_compact() {
            return;
        }
        if !should_compact_follow_up(
            self.prefix_tokens_gate(),
            self.generation_reserve,
            self.working_window,
            self.compact_ratio,
            live_tool_count(&self.messages),
            live_image_count(&self.messages),
            self.completer
                .prefix_meter()
                .map(|(f, _)| f.follow_up_compact_tools())
                .unwrap_or(TURN_START_COMPACT_TOOLS),
        ) {
            return;
        }
        self.signal_preparing();
        let _ = self.apply_compact_pass();
    }

    fn signal_preparing(&self) {
        self.note("[compact] preparing follow-up context");
        if let Some(sink) = self.live_sink() {
            sink.reasoning("正在整理上下文…\n");
        }
    }

    fn can_apply_compact(&self) -> bool {
        if let Some(log) = &self.log {
            plan_compact(log.events()).is_some()
        } else {
            compact_messages(&self.messages).is_some()
        }
    }

    fn prefix_tokens_gate(&self) -> u32 {
        estimate_prefix_tokens(&self.messages, &self.tools, true)
    }

    fn apply_compact_pass(&mut self) -> bool {
        if !self.try_compact() {
            return false;
        }
        self.after_compact();
        let tools = self.enable_recall();
        if tools {
            self.note("[compact] cache_invalidated=compact,tools");
        } else {
            self.note("[compact] cache_invalidated=compact");
        }
        true
    }

    fn over_soft_window(&self) -> bool {
        match self.prefix_tokens() {
            Some(n) => over_soft_threshold(
                n,
                self.generation_reserve,
                self.working_window,
                self.compact_ratio,
            ),
            None => false,
        }
    }

    fn over_hard_window(&self) -> bool {
        match self.prefix_tokens() {
            Some(n) => over_hard_threshold(n, self.generation_reserve, self.working_window),
            None => false,
        }
    }

    fn prefix_tokens(&self) -> Option<u32> {
        let (family, kwargs) = self.completer.prefix_meter()?;
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(self.tools.as_slice())
        };
        let rendered = render(&RenderOpts {
            family,
            messages: &self.messages,
            tools,
            add_generation_prompt: true,
            kwargs,
        })
        .ok()?;
        count_tokens(family, &rendered.text).ok()
    }

    fn try_compact(&mut self) -> bool {
        if self.log.is_some() {
            let plan = self.log.as_ref().and_then(|log| plan_compact(log.events()));
            let Some(plan) = plan else {
                return false;
            };
            if let Some(mem) = &self.memory {
                let _ =
                    mem.write_compact_note(&self.session_id, plan.until_seq, &plan.archive_body());
            }
            self.log_event(SessionEvent::compact(plan));
            self.messages = derive_messages(self.log.as_ref().unwrap().events());
            true
        } else if let Some((_plan, msgs)) = compact_messages(&self.messages) {
            self.messages = msgs;
            crate::sticky::stub_expired_notes(&mut self.messages);
            true
        } else {
            false
        }
    }

    fn after_compact(&mut self) {
        self.handler.reset_repeat(&self.session_id);
        self.observed_paths.clear();
        if self.read_paths.is_empty() {
            return;
        }
        let mut paths: Vec<String> = self.read_paths.iter().cloned().collect();
        paths.sort();
        paths.truncate(16);
        let body = paths.join("\n");
        self.push_hidden_user(format!(
            "[compact] prior reads left the live window. Files still on disk — page with read(path, offset, limit); do not repeat the same unpaged read:\n{body}"
        ));
        self.read_paths.clear();
    }

    /// Append `recall` after compact. Returns true when `tools[]` changed
    /// (`cache_invalidated=tools` on top of the compact miss).
    fn enable_recall(&mut self) -> bool {
        if self.tools.is_empty() || has_recall(&self.tools) {
            return false;
        }
        self.tools.push(recall_tool());
        true
    }

    fn note(&self, line: &str) {
        if self.print {
            eprintln!("{line}");
        }
    }

    /// sticky 卡原位替换会击穿前缀缓存，与 compact 路径同风格留一条
    /// 观测线（stderr debug，不进模型上下文）。
    fn note_stubbed(&self, n: usize) {
        if n > 0 {
            self.note(&format!("[sticky] cache_invalidated=stub n={n}"));
        }
    }
}

pub(super) fn openai_tool_calls(calls: &[ToolCall]) -> Vec<Value> {
    calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "type": "function",
                "function": {
                    "name": c.name,
                    "arguments": args_as_object(c.arguments.clone()),
                }
            })
        })
        .collect()
}

fn normalize_tool_calls(calls: &[Value]) -> Vec<Value> {
    calls.iter().cloned().map(normalize_one_tool_call).collect()
}

fn normalize_one_tool_call(mut v: Value) -> Value {
    if let Some(args) = v.pointer_mut("/function/arguments") {
        *args = args_as_object(args.take());
    }
    v
}

fn args_as_object(v: Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    }
}

fn canon_read_path(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

/// 守卫集合的键：先经 `workspace.resolve` 归一，吸收 `./a.rs` vs `a.rs`、
/// 绝对/相对混用（小模型常见）；resolve 失败回退纯字符串归一。
/// insert 与 lookup 两侧必须走同一函数。
fn canon_ws_path(ws: &Workspace, path: &str) -> String {
    match ws.resolve(path) {
        Ok(p) => canon_read_path(&p.to_string_lossy()),
        Err(_) => canon_read_path(path),
    }
}

/// `ToolState` 不落盘，重建只能靠文案判失败。新契约：非 Success 一律
/// "Error:" 开头。旧会话日志里还有三种未带前缀的失败文案，保守兼容；
/// coordinator 中断文案（cancelled/timeout）同样意味着 transcript 没看到内容。
fn tool_text_failed(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("Error:")
        || t.starts_with("plan mode:")
        || t.starts_with("User denied")
        || t.starts_with("tool task aborted")
        || t.trim_end() == "cancelled"
        || t.trim_end() == "timeout"
}

/// Paths whose content the live transcript shows: read/view/write/edit calls
/// whose tool result is not an error. Rebuilt each turn so sidecar re-hydration
/// (new Agent, old transcript) keeps the same `refuse_blind_overwrite` view.
fn observed_from_messages(messages: &[ChatMessage], ws: &Workspace) -> HashSet<String> {
    let mut ok: HashMap<String, bool> = HashMap::new();
    for m in messages {
        if m.role != "tool" {
            continue;
        }
        let Some(id) = m.tool_call_id.as_deref() else {
            continue;
        };
        let failed = tool_text_failed(m.content.as_deref().unwrap_or(""));
        ok.insert(id.to_string(), !failed);
    }
    let mut out = HashSet::new();
    for m in messages {
        if m.role != "assistant" {
            continue;
        }
        let Some(calls) = &m.tool_calls else {
            continue;
        };
        for c in calls {
            let name = c["function"]["name"].as_str().unwrap_or("");
            if !matches!(name, "read" | "view" | "write" | "edit") {
                continue;
            }
            let id = c["id"].as_str().unwrap_or("");
            if !ok.get(id).copied().unwrap_or(false) {
                continue;
            }
            let args = args_as_object(c["function"]["arguments"].clone());
            if let Some(path) = fs_tool_path(name, &args) {
                out.insert(canon_ws_path(ws, &path));
            }
        }
    }
    out
}

fn write_path_body(call: &ToolCall) -> (&str, &str) {
    let path = call
        .arguments
        .get("path")
        .and_then(|v| v.as_str())
        .or_else(|| call.arguments.get("file_path").and_then(|v| v.as_str()))
        .unwrap_or("");
    let body = call
        .arguments
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    (path, body)
}

fn bash_cmd(call: &ToolCall) -> &str {
    call.arguments
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn preview_args(call: &ToolCall) -> String {
    call.arguments
        .get("path")
        .or_else(|| call.arguments.get("file_path"))
        .or_else(|| call.arguments.get("command"))
        .or_else(|| call.arguments.get("code"))
        .or_else(|| call.arguments.get("query"))
        .or_else(|| call.arguments.get("blob"))
        .or_else(|| call.arguments.get("name"))
        .or_else(|| call.arguments.get("method"))
        .or_else(|| call.arguments.get("server"))
        .or_else(|| call.arguments.get("prompt"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| call.arguments.get("seq").map(|v| v.to_string()))
        .unwrap_or_else(|| "…".into())
}

fn openai_stored(calls: &[ToolCall]) -> Vec<OpenAiToolCall> {
    calls
        .iter()
        .map(|c| OpenAiToolCall::function(&c.id, &c.name, c.arguments.to_string()))
        .collect()
}

fn bind_session(
    opts: &RunOpts,
    system: &str,
    tools: &[Value],
    policy: ThinkPolicy,
) -> (Vec<ChatMessage>, Option<SessionLog>) {
    let fresh = vec![ChatMessage::system(system.to_string())];
    if !opts.persist_session || opts.session_id.is_empty() {
        return (fresh, None);
    }
    let open = |id: &str| {
        if let Some(dir) = &opts.session_dir {
            SessionLog::open_in(dir, id)
        } else {
            SessionLog::open(id)
        }
    };
    let create = |start: SessionStart| {
        if let Some(dir) = &opts.session_dir {
            SessionLog::create_in(dir, start)
        } else {
            SessionLog::create(start)
        }
    };
    match open(&opts.session_id) {
        Ok(log) => (log.messages(), Some(log)),
        Err(_) => {
            let mut start = SessionStart::new(
                opts.session_id.clone(),
                opts.workspace.display().to_string(),
                opts.session_mode,
                system,
                tools_hash(tools),
                policy,
            );
            if !opts.channel.is_empty() {
                start.channel = opts.channel.clone();
            }
            match create(start) {
                Ok(log) => (fresh, Some(log)),
                Err(_) => (fresh, None),
            }
        }
    }
}

fn bind_periphery(
    opts: &RunOpts,
    workspace: &Workspace,
    tool_set: ToolSet,
) -> (
    String,
    Vec<Value>,
    Option<MemoryStore>,
    SkillCatalog,
    McpRegistry,
) {
    let home = opts.home.clone().or_else(|| Config::home_dir().ok());
    let extra_tools = opts.peripheral && matches!(tool_set, ToolSet::Agent);
    let memory = if opts.peripheral {
        home.as_ref().and_then(|h| MemoryStore::open(h).ok())
    } else {
        None
    };
    let skills = if opts.peripheral {
        SkillCatalog::load(
            home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
            workspace.root(),
        )
    } else {
        SkillCatalog::default()
    };
    let mcp = if opts.peripheral {
        McpRegistry::load(home.as_deref(), workspace.root(), &opts.mcp)
    } else {
        McpRegistry::default()
    };

    let mut system = session_prompt(
        workspace.root(),
        home.as_deref(),
        &opts.prompt_file,
        opts.coding_identity,
    );
    if extra_tools {
        let skills_md = if opts.skills_auto_catalog {
            skills.catalog_markdown()
        } else {
            String::new()
        };
        let mcp_md = if opts.mcp_auto_catalog {
            mcp.catalog_markdown()
        } else {
            String::new()
        };
        system.push_str(&periphery_section(&skills_md, &mcp_md));
        system.push('\n');
        system.push_str(crate::cron::CRON_SYSTEM_LINE);
        system.push('\n');
    }

    let mut tools = match tool_set {
        ToolSet::None => Vec::new(),
        ToolSet::Agent => {
            let mut t = agent_tools();
            t.push(search_tool());
            t
        }
        ToolSet::Code => code_tools(),
    };
    if extra_tools {
        if memory.is_some() {
            tools.push(memory_search_tool());
        }
        if !mcp.servers.is_empty() {
            tools.push(mcp_tool());
        }
        if opts.web.enabled {
            tools.push(crate::tools_schema::web_tool());
        }
    }
    if opts.media && matches!(tool_set, ToolSet::Agent) {
        tools.push(view_tool());
    }
    if matches!(tool_set, ToolSet::Agent) && (opts.plan_mode || opts.clarify_mode) {
        tools.push(ask_tool());
    }
    (system, tools, memory, skills, mcp)
}

fn empty_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn parallel_safe_batch(calls: &[ToolCall]) -> bool {
    calls.len() > 1
        && calls
            .iter()
            .all(|c| matches!(c.name.as_str(), "read" | "view" | "search" | "web" | "ask"))
}

/// Merge agent-level stop with the coordinator's per-call flag. Native tools
/// take one `CancelFlag`; mcp/view select on the merged flag because they
/// do not take one.
fn spawn_cancel_bridge(
    agent: CancelFlag,
    per_call: CancelFlag,
) -> (CancelFlag, tokio::task::JoinHandle<()>) {
    let merged = CancelFlag::new();
    let link = merged.clone();
    let handle = tokio::spawn(async move {
        tokio::select! {
            _ = agent.cancelled() => link.cancel(),
            _ = per_call.cancelled() => link.cancel(),
        }
    });
    (merged, handle)
}

fn is_harness_fail(response: &ToolResponse) -> bool {
    response.state == ToolState::Interrupted && response.joined_text() == "Error: tool task aborted"
}

/// Live failure at 2048 (M002, 7^222 mod 1000): the derivation overran the cap
/// at temp 1.0. dsh on the same weights finished with more room (~7.7k chars)
/// and was correct. A turn's think length is high-variance; the cap's job is to
/// catch true runaways, then give the model one observed, roomy retry rather
/// than silently replacing its reasoning policy.
const NO_TOOL_THINK_FLOOR: u32 = 8192;

/// Generation room reserved past the think floor for the visible answer.
const NO_TOOL_ANSWER_RESERVE: u32 = 4096;

/// Only injected after the streaming watchdog has actually fired. This keeps
/// the common path free of process rules and lets the model decide how to
/// converge once it has one concrete observation about its trajectory.
const THINK_DIVERGENCE_NOTE: &str = "[trajectory] 本轮思考已触及长度预算，可能开始发散。\
先压缩当前已知事实和未决问题；若证据已足够就作答或执行，若仍缺关键证据只补最小的一步。";

/// One wrap-up hop when a physics cap would otherwise tombstone the turn.
const PHYSICS_WRAP_NOTE: &str = "[trajectory] 本轮已接近步数、时间或上下文上限。\
用已有证据收束成对用户可见的结论，不要再开新的工具循环。";

/// One repair hop after the parse-fail retry budget is spent.
const PARSE_REPAIR_NOTE: &str = "[trajectory] 上一跳工具调用未能解析。\
改用完整原生 tool call，或直接给出可见结论。";

/// Flash-Next (and cousins) sometimes stop with empty `content` — think still
/// running, or both channels blank. One wrap-up hop; never deliver `""`.
const EMPTY_CHANNEL_NOTE: &str = "[trajectory] 上一跳没有给用户可见回复。\
把完整结论写到正常回复；思考通道用户看不见。若任务没做完，调用工具。不要再交空回复。";

/// Interactive 27B narrates tool hops; Flash-Next often does not. One observation.
const SILENT_TOOL_NOTE: &str = "[trajectory] 连续几跳工具调用没有对用户说话。\
接下来动手前用一句中文（≤20字）说明，再调工具；不要把说明只写在思考通道。";

const EMPTY_STOP_FALLBACK: &str = "这一步停在了空回复上。请再说一次，或换个问法。";

/// Consecutive empty-content tool hops before the Flash-Next style observation.
const SILENT_TOOL_STREAK: u32 = 4;

/// Upper bound for promoting think-channel text into the visible reply.
/// Longer blobs are unfinished CoT, not an answer.
const PROMOTE_REASONING_MAX: usize = 800;

fn is_physics_stop(reason: &str) -> bool {
    reason.starts_with("budget:context")
        || reason.contains("Max iterations")
        || reason.contains("time limit")
        || reason.contains("Token budget")
        || reason.contains("call budget")
}

/// Real ripgrep hits kept live when the dump is also folded into index spans.
const SEARCH_HEAD_LINES: usize = 12;

fn search_head(full: &str) -> String {
    full.lines()
        .take(SEARCH_HEAD_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Folding must shrink the context. Spans that are as long as the dump (a query
/// matching one whole file) would cost KV instead of saving it.
fn search_fold_shrinks(full: &str, spans: &str) -> bool {
    search_head(full).len() + spans.len() < full.len()
}

/// Spans supplement the dump, they do not replace it: the model asked ripgrep a
/// precise question, so its own hits stay live and the rest stays recallable.
fn search_fold_text(full: &str, query: &str, spans: &str, blob: Option<&str>) -> String {
    format!(
        "{}\n[{} lines total; head kept{}]\n\n[index spans for `{query}`]\n{spans}",
        search_head(full),
        full.lines().count(),
        blob.map(|b| format!("; full output in blob {b} — recall(blob=…)"))
            .unwrap_or_default(),
    )
}

/// Per-round oracle runs are only worth it on a suite this fast.
const ORACLE_MAX_SUITE: Duration = Duration::from_secs(20);
const ORACLE_TAIL_CHARS: usize = 2000;

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

/// One line, task-scoped, only when the trigger fires. Names the exact call
/// shape so a 27B does not have to invent it.
const WEB_HINT: &str = "[web] 这个问题可能涉及时效信息，训练记忆可能过时。先用 web(query=…) 搜索或 web(url=…) 抓正文核实，回答附来源链接。";

/// One-line arithmetic hygiene for the small subset of prompts whose answer
/// depends on a derived probability, percentage or threshold. This stays out
/// of the frozen system prompt and asks for no extra prose or forced turn.
const NUMERIC_CHECK_HINT: &str = "[verify:numeric] 若结论含由题面推导的概率、百分比或阈值，定稿前内部回代一次，区分百分比、百分点和被求变量；一致就直接作答，不新增复核章节。";

fn wants_numeric_check(user: &str) -> bool {
    let lower = user.to_lowercase();
    const CODE_MARKS: &[&str] = &[
        "代码",
        "函数",
        "源码",
        "编译",
        "单测",
        "测试用例",
        "正则",
        ".py",
        ".rs",
        ".js",
        ".ts",
        " code",
        "function",
        "compile",
        "unit test",
        "regex",
        " bug",
    ];
    if CODE_MARKS.iter().any(|mark| lower.contains(mark)) || has_call_ident(user) {
        return false;
    }
    const QUANTITY_MARKS: &[&str] = &[
        "%",
        "％",
        "概率",
        "准确率",
        "百分",
        "百分点",
        "阈值",
        "临界",
        "期望值",
        "赔率",
        "比率",
        "比例",
        "方差",
        "置信区间",
        "probability",
        "accuracy",
        "percent",
        "percentage point",
        "threshold",
        "expected value",
        "odds",
        "variance",
        "confidence interval",
    ];
    const REASONING_MARKS: &[&str] = &[
        "求",
        "计算",
        "比较",
        "推导",
        "证明",
        "估计",
        "判断",
        "讨论",
        "边界",
        "阈值",
        "临界",
        "期望",
        "calculate",
        "compare",
        "derive",
        "prove",
        "estimate",
        "evaluate",
        "discuss",
        "boundary",
        "threshold",
        "expected",
    ];
    QUANTITY_MARKS.iter().any(|mark| lower.contains(mark))
        && REASONING_MARKS.iter().any(|mark| lower.contains(mark))
}

/// Freshness smell: explicit recency words, a 2025+ year, or a pasted URL.
/// Deliberately narrow — a false fire costs one useless hidden line, a missed
/// fire costs nothing the model did not already lack.
fn wants_web_check(user: &str) -> bool {
    if user.contains("不要联网") || user.contains("不要搜索") || user.contains("别联网")
    {
        return false;
    }
    if user.contains("http://") || user.contains("https://") {
        return true;
    }
    const MARKS: &[&str] = &[
        "最新",
        "近期",
        "最近发布",
        "今天",
        "今年",
        "现在的",
        "目前的",
        "新闻",
        "行情",
        "股价",
        "汇率",
        "多少钱",
        "价格是多少",
        "什么时候发布",
        "发布了吗",
        "上市了吗",
        "latest version",
        "latest release",
        "release date",
        "what's new in",
        "recent news",
        "price of",
    ];
    let lower = user.to_lowercase();
    if MARKS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    mentions_recent_year(user)
}

/// Any literal year 2025–2099 in the task text.
fn mentions_recent_year(user: &str) -> bool {
    let bytes = user.as_bytes();
    for w in bytes.windows(5) {
        if w[0] == b'2' && w[1] == b'0' && w[2].is_ascii_digit() && w[3].is_ascii_digit() {
            // Not part of a longer digit run (e.g. 202611 order ids).
            if w[4].is_ascii_digit() {
                continue;
            }
            let year = 2000 + u32::from(w[2] - b'0') * 10 + u32::from(w[3] - b'0');
            if (2025..=2099).contains(&year) {
                return true;
            }
        }
    }
    if bytes.len() >= 4 {
        let w = &bytes[bytes.len() - 4..];
        if w[0] == b'2' && w[1] == b'0' && w[2].is_ascii_digit() && w[3].is_ascii_digit() {
            let year = 2000 + u32::from(w[2] - b'0') * 10 + u32::from(w[3] - b'0');
            return (2025..=2099).contains(&year);
        }
    }
    false
}

fn forbids_tools(user: &str) -> bool {
    if user.contains("不要调用工具") || user.contains("不要用工具") || user.contains("不要开工具")
    {
        return true;
    }
    let l = user.to_ascii_lowercase();
    l.contains("don't use tools") || l.contains("do not use tools")
}

fn wants_auto_locate(user: &str) -> bool {
    if [
        "修",
        "实现",
        "定位",
        "在哪",
        "哪里",
        "缺陷",
        "崩溃",
        "bug",
        "立刻改",
        "必须改",
    ]
    .iter()
    .any(|p| user.contains(p))
    {
        return true;
    }
    let l = user.to_ascii_lowercase();
    if l.contains("fix ") || l.contains("implement ") {
        return true;
    }
    if has_call_ident(user) {
        return true;
    }
    user.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.')))
        .any(|t| {
            t.len() >= 3
                && (t.contains('_')
                    || t.contains('/')
                    || (t.contains('.') && !t.starts_with('.'))
                    || (t.chars().any(|c| c.is_ascii_uppercase())
                        && t.chars().any(|c| c.is_ascii_lowercase())))
        })
}

fn has_call_ident(user: &str) -> bool {
    let bytes = user.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'(' && i - start >= 3 {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

/// Narrower than `wants_auto_locate`: locating is free, running the suite is
/// not. "这个函数在哪调用" must not cost a test run.
#[cfg(test)]
fn wants_test_baseline(user: &str) -> bool {
    if ["修", "改", "实现", "补", "重构", "优化", "加上"]
        .iter()
        .any(|p| user.contains(p))
    {
        return true;
    }
    let l = user.to_ascii_lowercase();
    ["fix ", "implement ", "refactor ", "rewrite ", "make it "]
        .iter()
        .any(|p| l.contains(p))
}

#[cfg(test)]
fn python_launcher() -> &'static str {
    verify::python_launcher()
}

enum AgentsMd {
    Missing,
    Ok(String),
    TooLarge,
}

fn read_agents_md(root: &std::path::Path, max_tokens: u32, head: bool) -> AgentsMd {
    let path = root.join("AGENTS.md");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return AgentsMd::Missing;
    };
    if raw.trim().is_empty() {
        return AgentsMd::Missing;
    }
    let n = sticky::tokens(&raw);
    if n <= max_tokens {
        return AgentsMd::Ok(raw);
    }
    if head {
        let clipped = sticky::clip_to_tokens(&raw, max_tokens);
        if clipped.is_empty() {
            AgentsMd::TooLarge
        } else {
            AgentsMd::Ok(clipped)
        }
    } else {
        AgentsMd::TooLarge
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::error::Error;
    use crate::policy::{Effort, ThinkPolicy};
    use crate::session::{SessionEvent, SessionLog};
    use crate::tools_schema::has_recall;
    use serde_json::json;

    struct Scripted {
        turns: Mutex<VecDeque<ModelTurn>>,
        meter: bool,
    }

    impl Completer for Scripted {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Value]>,
        ) -> Result<ModelTurn> {
            self.turns
                .lock()
                .expect("script")
                .pop_front()
                .ok_or_else(|| Error::msg("script exhausted"))
        }

        fn prefix_meter(&self) -> Option<(Family, TemplateKwargs)> {
            if !self.meter {
                return None;
            }
            Some((
                Family::Qwen38,
                TemplateKwargs {
                    enable_thinking: Some(false),
                    reasoning_effort: None,
                    preserve_thinking: None,
                },
            ))
        }
    }

    struct ScriptedFamily {
        inner: Scripted,
        family: Family,
    }

    impl Completer for ScriptedFamily {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            tools: Option<&[Value]>,
        ) -> Result<ModelTurn> {
            self.inner.complete(messages, tools).await
        }

        fn prefix_meter(&self) -> Option<(Family, TemplateKwargs)> {
            Some((
                self.family,
                TemplateKwargs {
                    enable_thinking: Some(true),
                    reasoning_effort: Some("medium".into()),
                    preserve_thinking: Some(true),
                },
            ))
        }
    }

    struct Delayed {
        inner: Scripted,
        delay: Duration,
    }

    impl Completer for Delayed {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            tools: Option<&[Value]>,
        ) -> Result<ModelTurn> {
            tokio::time::sleep(self.delay).await;
            self.inner.complete(messages, tools).await
        }
    }

    struct PolicyWatch {
        inner: Scripted,
        policy: Mutex<ThinkPolicy>,
        seen: std::sync::Arc<Mutex<Vec<ThinkPolicy>>>,
    }

    impl Completer for PolicyWatch {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            tools: Option<&[Value]>,
        ) -> Result<ModelTurn> {
            self.inner.complete(messages, tools).await
        }

        fn set_policy(&self, p: ThinkPolicy) {
            self.seen.lock().expect("seen").push(p.clone());
            *self.policy.lock().expect("policy") = p;
        }

        fn policy(&self) -> Option<ThinkPolicy> {
            Some(self.policy.lock().expect("policy").clone())
        }
    }

    fn turn_text(content: &str) -> ModelTurn {
        ModelTurn {
            content: content.into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            raw_tool_calls: None,
            prompt_tokens: 1,
            completion_tokens: 1,
            watchdog_hit: false,
            parse_fail: false,
            cached_tokens: None,
            decode_tok_s: None,
        }
    }

    fn turn_think(reasoning: &str) -> ModelTurn {
        let mut t = turn_text("");
        t.reasoning = reasoning.into();
        t
    }

    fn turn_tool(name: &str, args: Value) -> ModelTurn {
        turn_tools(vec![("call_1", name, args)])
    }

    fn turn_said(content: &str, name: &str, args: Value) -> ModelTurn {
        let mut t = turn_tool(name, args);
        t.content = content.into();
        t
    }

    fn turn_tools(calls: Vec<(&str, &str, Value)>) -> ModelTurn {
        ModelTurn {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: calls
                .into_iter()
                .map(|(id, name, arguments)| ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                })
                .collect(),
            raw_tool_calls: None,
            prompt_tokens: 1,
            completion_tokens: 1,
            watchdog_hit: false,
            parse_fail: false,
            cached_tokens: None,
            decode_tok_s: None,
        }
    }

    fn turn_parse_fail(content: &str) -> ModelTurn {
        ModelTurn {
            content: content.into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            raw_tool_calls: None,
            prompt_tokens: 1,
            completion_tokens: 1,
            watchdog_hit: false,
            parse_fail: true,
            cached_tokens: None,
            decode_tok_s: None,
        }
    }

    fn opts(dir: &std::path::Path) -> RunOpts {
        let mut o = RunOpts::from_config(&Config::default(), dir.to_path_buf());
        o.session_id = "test".into();
        o.print = false;
        o.max_steps = 8;
        o.agents_md = false;
        o.generation_reserve = 0;
        o.home = Some(dir.join(".q38-home"));
        o.peripheral = true;
        o.skills_auto_catalog = false;
        o.mcp_auto_catalog = false;
        o.mcp = McpConfig::default();
        o.media_bins = MediaBins::none();
        // Tests assert minimal message geometry; narration is covered by its
        // own dedicated test below.
        o.narrate = false;
        o
    }

    #[test]
    fn new_jsonl_stamps_channel() {
        let dir = std::env::temp_dir().join(format!("q38-chan-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        let mut o = opts(&dir);
        o.persist_session = true;
        o.session_id = "im1".into();
        o.session_dir = Some(sess.clone());
        o.channel = "qq".into();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("ok")])),
            meter: false,
        };
        let _agent = Agent::new(scripted, o).unwrap();
        let log = SessionLog::open_in(&sess, "im1").unwrap();
        assert_eq!(log.start().unwrap().channel, "qq");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn text_only_stops() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([{
                let mut t = turn_text("done");
                t.reasoning = "brief".into();
                t
            }])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.steps, 1);
        let asst = agent
            .messages
            .iter()
            .find(|m| m.role == "assistant")
            .unwrap();
        assert_eq!(asst.reasoning_content.as_deref(), Some("brief"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn think_only_scratch_gets_one_wrap_up() {
        let dir = std::env::temp_dir().join(format!("q38-chan-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_think(
                    "Need to check the core loop. Let me look at agent/mod.rs next and verify finish(turn.content).",
                ),
                turn_text("核心循环缺空回复兜底，已记下。"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("再查核心循环").await.unwrap();
        assert_eq!(out.text, "核心循环缺空回复兜底，已记下。");
        assert_eq!(out.steps, 2);
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        assert!(
            hidden.iter().any(|c| c.contains(EMPTY_CHANNEL_NOTE)),
            "wrap-up note missing: {hidden:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn think_only_finished_answer_is_promoted() {
        let dir = std::env::temp_dir().join(format!("q38-promo-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let body =
            "修得没问题。复查通过。c2f9bca 跳过 URL 端口里的三位数字，不再误判成 HTTP 状态码。\
网关软上限与空回复路径已经对齐。用户可见结论写在这里，没有未完成的计划。";
        assert!(crate::stutter::is_substantial_reply(body));
        assert!(!crate::stutter::is_scratch_think(body));
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_think(body)])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("复查修复").await.unwrap();
        assert_eq!(out.text, body);
        assert_eq!(out.steps, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn empty_finish_keeps_last_essay() {
        let dir = std::env::temp_dir().join(format!("q38-keep-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests.";
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(
                    essay,
                    "read",
                    json!({"path": "crates/q38-loop/src/agent/mod.rs"}),
                ),
                turn_text(""),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("read the loop").await.unwrap();
        assert_eq!(out.text, essay);
        assert_eq!(out.steps, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn empty_channel_second_hop_uses_fallback() {
        let dir = std::env::temp_dir().join(format!("q38-fb-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_think("Let me check finish() and the gate Continue path."),
                turn_think("Still need to look at inbound.rs for empty IM replies."),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("对照习惯").await.unwrap();
        assert_eq!(out.text, EMPTY_STOP_FALLBACK);
        assert_eq!(out.steps, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn dual_empty_stop_wraps_then_fallback() {
        let dir = std::env::temp_dir().join(format!("q38-bare-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text(""), turn_text("")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, EMPTY_STOP_FALLBACK);
        assert_eq!(out.steps, 2);
        assert!(agent.messages.iter().any(|m| m
            .content
            .as_deref()
            .unwrap_or("")
            .contains(EMPTY_CHANNEL_NOTE)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn flash_next_silent_tools_get_one_note() {
        let dir = std::env::temp_dir().join(format!("q38-sil-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let inner = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "a.rs"})),
                turn_tool("read", json!({"path": "b.rs"})),
                turn_tool("read", json!({"path": "c.rs"})),
                turn_tool("read", json!({"path": "d.rs"})),
                turn_text("读完了。"),
            ])),
            meter: false,
        };
        let mut o = opts(&dir);
        o.peripheral = false;
        let mut agent = Agent::new(
            ScriptedFamily {
                inner,
                family: Family::Qwen38Next,
            },
            o,
        )
        .unwrap();
        std::fs::write(dir.join("a.rs"), "a").unwrap();
        std::fs::write(dir.join("b.rs"), "b").unwrap();
        std::fs::write(dir.join("c.rs"), "c").unwrap();
        std::fs::write(dir.join("d.rs"), "d").unwrap();
        let out = agent.run("read four files").await.unwrap();
        assert_eq!(out.text, "读完了。");
        let notes = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| c.contains(SILENT_TOOL_NOTE))
            .count();
        assert_eq!(notes, 1, "silent-tool observation must land once");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn qwen38_silent_tools_do_not_nudge() {
        let dir = std::env::temp_dir().join(format!("q38-27b-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "a.rs"})),
                turn_tool("read", json!({"path": "b.rs"})),
                turn_tool("read", json!({"path": "c.rs"})),
                turn_tool("read", json!({"path": "d.rs"})),
                turn_text("done"),
            ])),
            meter: true,
        };
        std::fs::write(dir.join("a.rs"), "a").unwrap();
        std::fs::write(dir.join("b.rs"), "b").unwrap();
        std::fs::write(dir.join("c.rs"), "c").unwrap();
        std::fs::write(dir.join("d.rs"), "d").unwrap();
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read four files").await.unwrap();
        assert_eq!(out.text, "done");
        assert!(agent.messages.iter().all(|m| !m
            .content
            .as_deref()
            .unwrap_or("")
            .contains(SILENT_TOOL_NOTE)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn plan_mode_blocks_write() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("nope.txt");
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("write", json!({"path": "nope.txt", "content": "secret"})),
                turn_text("## plan\n- leave the file alone"),
            ])),
            meter: false,
        };
        let mut o = opts(&dir);
        o.plan_mode = true;
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("plan a change").await.unwrap();
        assert!(out.text.contains("plan"));
        assert!(!target.exists(), "write must not land in plan mode");
        let tools: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.clone())
            .collect();
        assert!(
            tools.iter().any(|t| t.contains("plan mode")),
            "denied write: {tools:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stop_before_complete_aborts_without_a_waiter() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let cancel = CancelFlag::new();
        cancel.cancel();
        agent.set_cancel(cancel);
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.stop_reason.as_deref(), Some("aborted"));
        assert_eq!(out.steps, 0);
        assert!(out.text.is_empty(), "{}", out.text);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stop_during_complete_does_not_wait_for_the_model() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let delayed = Delayed {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
                meter: false,
            },
            delay: Duration::from_secs(30),
        };
        let mut agent = Agent::new(delayed, opts(&dir)).unwrap();
        let cancel = CancelFlag::new();
        agent.set_cancel(cancel.clone());
        let h = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            cancel.cancel();
        });
        let t0 = std::time::Instant::now();
        let out = agent.run("hi").await.unwrap();
        let _ = h.await;
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stop waited {:?}",
            t0.elapsed()
        );
        assert_eq!(out.stop_reason.as_deref(), Some("aborted"));
        assert!(out.text.is_empty(), "{}", out.text);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stop_during_bash_reaches_merged_cancel_flag() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("bash", json!({"command": "sleep 30"})),
                turn_text("should-not-run"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let cancel = CancelFlag::new();
        agent.set_cancel(cancel.clone());
        let h = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            cancel.cancel();
        });
        let t0 = std::time::Instant::now();
        let out = agent.run("sleep").await.unwrap();
        let _ = h.await;
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "bash ignore cancel, waited {:?}",
            t0.elapsed()
        );
        assert_eq!(out.stop_reason.as_deref(), Some("aborted"));
        assert_ne!(out.text, "should-not-run");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn greeting_echo_line_is_stripped() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text(
                "你好\n\n有什么我可以帮你的吗？",
            )])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("你好").await.unwrap();
        assert_eq!(out.text, "有什么我可以帮你的吗？");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn tool_then_text() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "abc").unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "note.txt"})),
                turn_text("the file says abc"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("read the note").await.unwrap();
        assert!(out.text.contains("abc"));
        assert_eq!(out.steps, 2);
        let asst = agent
            .messages
            .iter()
            .find(|m| m.role == "assistant" && m.tool_calls.is_some())
            .expect("tool assistant");
        let args = &asst.tool_calls.as_ref().unwrap()[0]["function"]["arguments"];
        assert!(args.is_object(), "{args}");
        assert_eq!(args["path"], "note.txt");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn each_user_turn_resets_iteration_cap() {
        let dir = std::env::temp_dir().join(format!("q38-iter-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "a.txt"})),
                turn_text("one"),
                turn_tool("read", json!({"path": "b.txt"})),
                turn_text("two"),
            ])),
            meter: false,
        };
        let mut o = opts(&dir);
        o.max_steps = 3;
        o.peripheral = false;
        let mut agent = Agent::new(scripted, o).unwrap();
        let first = agent.run("read a.txt").await.unwrap();
        assert_eq!(first.text, "one");
        assert!(
            first.stop_reason.as_deref().unwrap_or("").is_empty(),
            "first turn: {:?}",
            first.stop_reason
        );
        let second = agent.run("read b.txt").await.unwrap();
        assert_eq!(
            second.text, "two",
            "second turn hit {:?}",
            second.stop_reason
        );
        assert!(
            !second
                .stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Max iterations"),
            "iteration cap leaked across user turns: {:?}",
            second.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn physics_step_cap_wraps_then_keeps_spoken_text() {
        let dir = std::env::temp_dir().join(format!("q38-wrap-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 2;
        o.peripheral = false;
        let ping = json!({"path": "ping.txt"});
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_text("wrapped up"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read ping.txt").await.unwrap();
        assert_eq!(out.text, "wrapped up");
        assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        let notes = hidden
            .iter()
            .filter(|c| c.contains(PHYSICS_WRAP_NOTE))
            .count();
        assert_eq!(notes, 1, "physics wrap lands once: {hidden:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn steer_injects_after_tool_round() {
        let dir = std::env::temp_dir().join(format!("q38-steer-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "abc").unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "note.txt"})),
                turn_text("ok focusing auth"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(vec!["focus on auth".into()]));
        agent.set_steer(slot);
        let out = agent.run("read the note").await.unwrap();
        assert!(out.text.contains("auth"));
        assert!(agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("Steer: focus on auth")
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn edit_thrash_injects_guard_note_and_upgrades_effort() {
        let dir =
            std::env::temp_dir().join(format!("q38-thrash-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool(
                    "edit",
                    json!({"path": "a.py", "old_string": "x = 1", "new_string": "x = 2"}),
                ),
                turn_tool(
                    "edit",
                    json!({"path": "a.py", "old_string": "x = 2", "new_string": "x = 1"}),
                ),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("fix a.py").await.unwrap();
        assert_eq!(out.text, "done");
        let guard_notes: Vec<&ChatMessage> = agent
            .messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .unwrap_or("")
                        .contains("[trajectory] 同一位置刚被改回")
            })
            .collect();
        assert_eq!(guard_notes.len(), 1, "exactly one thrash note");
        // The judgment upgrade must survive until the model's next turn; the
        // final clean text turn then drops it back to baseline.
        assert!(!agent.effort.auto_upgraded(), "clean step decays upgrade");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_expectation_edit_injects_guard_note_once() {
        let dir = std::env::temp_dir().join(format!("q38-texp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(
            dir.join("tests/test_x.py"),
            "assertEqual(total, 1932.00)\nassertEqual(count, 3)\n",
        )
        .unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool(
                    "edit",
                    json!({"path": "tests/test_x.py",
                           "old_string": "assertEqual(total, 1932.00)",
                           "new_string": "assertEqual(total, -1957.50)"}),
                ),
                turn_tool(
                    "edit",
                    json!({"path": "tests/test_x.py",
                           "old_string": "assertEqual(count, 3)",
                           "new_string": "assertEqual(count, 4)"}),
                ),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("修一下测试").await.unwrap();
        assert_eq!(out.text, "done");
        let notes = agent
            .messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .unwrap_or("")
                        .contains("[trajectory] 检测到已有测试期望被修改")
            })
            .count();
        assert_eq!(notes, 1, "one-shot per session");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_red_after_prod_edit_injects_guard_note() {
        let dir = std::env::temp_dir().join(format!("q38-tred-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
        std::fs::write(
            dir.join("test_app.py"),
            "import unittest\nfrom app import x\n\
             class T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(x, 1)\n",
        )
        .unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tools(vec![
                    (
                        "e1",
                        "edit",
                        json!({"path": "app.py", "old_string": "x = 1", "new_string": "x = 2"}),
                    ),
                    (
                        "b1",
                        "bash",
                        json!({"command": format!("{} -B -m unittest test_app", python_launcher())}),
                    ),
                ]),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("fix app.py").await.unwrap();
        assert_eq!(out.text, "done");
        let notes = agent
            .messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .unwrap_or("")
                        .contains("[trajectory] 生产代码修改后测试由绿转红")
            })
            .count();
        assert_eq!(notes, 1, "one-shot test-red note");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn print_mode_test_red_is_advisory() {
        let dir = std::env::temp_dir().join(format!("q38-tredp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
        std::fs::write(
            dir.join("test_app.py"),
            "import unittest\nfrom app import x\n\
             class T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(x, 1)\n",
        )
        .unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tools(vec![
                    (
                        "e1",
                        "edit",
                        json!({"path": "app.py", "old_string": "x = 1", "new_string": "x = 2"}),
                    ),
                    (
                        "b1",
                        "bash",
                        json!({"command": format!("{} -B -m unittest test_app", python_launcher())}),
                    ),
                ]),
                turn_text("should not run"),
            ])),
            meter: false,
        };
        let mut o = opts(&dir);
        o.print = true;
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("fix app.py").await.unwrap();
        assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
        assert_eq!(out.text, "should not run");
        assert!(agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] 生产代码修改后测试由绿转红")
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn print_mode_oracle_reports_and_model_continues() {
        let dir = std::env::temp_dir().join(format!("q38-torc-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
        std::fs::write(
            dir.join("test_app.py"),
            "import unittest\nfrom app import x\n\
             class T(unittest.TestCase):\n    def test_x(self):\n        self.assertEqual(x, 1)\n",
        )
        .unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool(
                    "edit",
                    json!({"path": "app.py", "old_string": "x = 1", "new_string": "x = 2"}),
                ),
                turn_text("should not run"),
            ])),
            meter: false,
        };
        let mut o = opts(&dir);
        o.print = true;
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("fix app.py").await.unwrap();
        assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
        assert_eq!(out.text, "should not run");
        assert!(agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("[trajectory] 生产代码修改后测试由绿转红")
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn write_over_existing_test_fires_expectation_note() {
        let dir = std::env::temp_dir().join(format!("q38-twr-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/test_x.py"), "assertEqual(n, 3)\n").unwrap();
        // Read first: the blind-overwrite gate refuses `write` to an existing
        // unobserved file before S1 can ever see it.
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "tests/test_x.py"})),
                turn_tool(
                    "write",
                    json!({"path": "tests/test_x.py", "content": "assertEqual(n, 4)\n"}),
                ),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("修一下测试").await.unwrap();
        assert_eq!(out.text, "done");
        let notes = agent
            .messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .unwrap_or("")
                        .contains("[trajectory] 检测到已有测试期望被修改")
            })
            .count();
        assert_eq!(notes, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn coding_user_turn_injects_locate_spans() {
        let dir = std::env::temp_dir().join(format!("q38-loc-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/paging.py"),
            "def page_bounds(n, size):\n    return (n - 1) * size, n * size\n",
        )
        .unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("ok")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("修 paging.py 的分页边界").await.unwrap();
        assert_eq!(out.text, "ok");
        let locate = agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content.as_deref().unwrap_or("").contains("[locate]")
                && m.content.as_deref().unwrap_or("").contains("page_bounds")
        });
        assert!(locate, "expected hop0 locate card");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_rg_dump_is_folded_to_spans() {
        let dir = std::env::temp_dir().join(format!("q38-rg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for f in 0..30 {
            let mut body = format!("def page_bounds_{f}(n):\n    return n\n");
            for i in 0..6 {
                body.push_str(&format!("# page_bounds note {f}-{i}\n"));
            }
            std::fs::write(dir.join(format!("src/f{f}.py")), &body).unwrap();
        }
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("bash", json!({"command": "grep -rn page_bounds src"})),
                turn_text("ok"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("修 page_bounds").await.unwrap();
        assert_eq!(out.text, "ok");
        let tool_msg = agent
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("tool message");
        let tool_txt = tool_msg.content.as_deref().unwrap_or("");
        assert!(tool_txt.contains("index spans for"), "{tool_txt}");
        // The model's own grep hits must survive: spans supplement, never replace.
        assert!(tool_txt.contains(":1:def page_bounds_"), "{tool_txt}");
        assert!(tool_txt.contains("full output in blob"), "{tool_txt}");
        // grep -rn would emit 30 files x 8 matching lines.
        assert!(
            tool_txt.lines().count() < 240,
            "not shrunk: {} lines",
            tool_txt.lines().count()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_fold_only_when_it_shrinks() {
        let dump: String = (0..80).map(|i| format!("src/f{i}.py:1:hit\n")).collect();
        let spans = "## src/f0.py:1-4\n     1|def f():\n";
        assert!(search_fold_shrinks(&dump, spans));
        // One file matched end to end: spans are the file, folding would grow it.
        let small = "src/a.py:1:hit\nsrc/a.py:2:hit\n";
        let fat: String = (0..60).map(|i| format!("     {i}|line {i}\n")).collect();
        assert!(!search_fold_shrinks(small, &fat));
    }

    #[test]
    fn auto_locate_only_on_coding_asks() {
        assert!(wants_auto_locate("修 paging.py 的分页"));
        assert!(wants_auto_locate("fix upgrade_medium"));
        assert!(wants_auto_locate(
            "cents() 用银行家舍入，财务说必须改成小学四舍五入。立刻改。不要改测试。"
        ));
        assert!(!wants_auto_locate("read the note"));
        assert!(!wants_auto_locate("where is the think cap upgraded"));
    }

    #[test]
    fn test_baseline_only_on_mutation_asks() {
        assert!(wants_test_baseline("修 merge_intervals，补回归"));
        assert!(wants_test_baseline("fix the parser"));
        // Locating is cheap, a suite run is not.
        assert!(!wants_test_baseline("merge_intervals 在哪被调用？"));
        assert!(!wants_test_baseline("where is page_bounds defined"));
    }

    #[test]
    fn unattended_im_uses_hermes_caps() {
        let dir = std::env::temp_dir().join(format!("q38-im-{}", uuid::Uuid::new_v4().simple()));
        let mut o = opts(&dir);
        o.channel = "wechat".into();
        apply_unattended_policy(&mut o, &Config::default());
        assert_eq!(o.max_steps, 500);
        assert!(o.max_wall.is_zero());
        o.channel = "web".into();
        o.max_steps = 80;
        o.max_wall = std::time::Duration::from_secs(1800);
        apply_unattended_policy(&mut o, &Config::default());
        assert_eq!(o.max_steps, 80);
        assert_eq!(o.max_wall, std::time::Duration::from_secs(1800));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn search_returns_function_span() {
        let dir =
            std::env::temp_dir().join(format!("q38-search-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/policy.rs"),
            "pub struct ThinkPolicy { pub max_think_tokens: u32 }\n\n\
             fn upgrade_medium(&mut self) {\n    self.max_think_tokens = 2048;\n}\n",
        )
        .unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("search", json!({"query": "upgrade_medium"})),
                turn_text("found"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        assert!(crate::tools_schema::has_tool(&agent.tools, "search"));
        let out = agent.run("where is think cap upgraded").await.unwrap();
        assert_eq!(out.text, "found");
        let tool_txt = agent
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .and_then(|m| m.content.as_deref())
            .unwrap_or("");
        assert!(tool_txt.contains("upgrade_medium"), "{tool_txt}");
        assert!(tool_txt.contains("src/policy.rs"), "{tool_txt}");
        assert!(
            !tool_txt.contains("struct ThinkPolicy"),
            "search dumped the whole file: {tool_txt}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn no_tool_phrase_widens_think_cap_then_restores() {
        let dir = std::env::temp_dir().join(format!("q38-s6-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let watch = PolicyWatch {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([turn_text("答案")])),
                meter: false,
            },
            policy: Mutex::new(ThinkPolicy::agent_default()),
            seen: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let seen = watch.seen.clone();
        let mut agent = Agent::new(watch, opts(&dir)).unwrap();
        let out = agent.run("不要调用工具。纽科姆悖论怎么选？").await.unwrap();
        assert_eq!(out.text, "答案");
        let seen = seen.lock().expect("seen").clone();
        assert!(
            seen.iter()
                .any(|p| p.max_think_tokens == NO_TOOL_THINK_FLOOR),
            "never widened: {seen:?}"
        );
        let last = seen.last().expect("set_policy");
        assert_eq!(last.max_think_tokens, 512, "must restore session policy");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_tool_policy_only_on_explicit_phrase() {
        assert!(forbids_tools("不要调用工具。什么是自由意志？"));
        assert!(forbids_tools("Please do not use tools. Explain Newcomb."));
        assert!(!forbids_tools("fix the parser in src/a.py"));
        assert!(!forbids_tools("if no tools are listed, skip"));
    }

    #[tokio::test]
    async fn narrate_injects_style_card_once() {
        let dir = std::env::temp_dir().join(format!("q38-style-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("好的"), turn_text("完成")])),
            meter: false,
        };
        let mut o = opts(&dir);
        o.narrate = true;
        let mut agent = Agent::new(scripted, o).unwrap();
        agent.run("你好").await.unwrap();
        agent.run("再来").await.unwrap();
        let style_notes = agent
            .messages
            .iter()
            .filter(|m| m.role == "user" && m.content.as_deref().unwrap_or("").contains("[style]"))
            .count();
        assert_eq!(style_notes, 1, "second turn must not duplicate a live card");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn print_mode_never_narrates() {
        let dir =
            std::env::temp_dir().join(format!("q38-nonarr-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("ok")])),
            meter: false,
        };
        let mut o = opts(&dir);
        o.narrate = true;
        o.print = true;
        let mut agent = Agent::new(scripted, o).unwrap();
        agent.run("hi").await.unwrap();
        assert!(agent.messages.iter().all(|m| !m
            .content
            .as_deref()
            .unwrap_or("")
            .contains("[style]")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn parallel_reads() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.join("b.txt"), "bravo\n").unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tools(vec![
                    ("r1", "read", json!({"path": "a.txt"})),
                    ("r2", "read", json!({"path": "b.txt"})),
                ]),
                turn_text("alpha bravo"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("read both").await.unwrap();
        assert_eq!(out.text, "alpha bravo");
        let tools: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| {
                (
                    m.tool_call_id.clone().unwrap_or_default(),
                    m.content.clone().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(tools.len(), 2, "{tools:?}");
        assert_eq!(tools[0].0, "r1");
        assert_eq!(tools[1].0, "r2");
        assert!(tools[0].1.contains("alpha"), "{}", tools[0].1);
        assert!(tools[1].1.contains("bravo"), "{}", tools[1].1);
        let asst = agent
            .messages
            .iter()
            .find(|m| m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| c.len() == 2))
            .expect("parallel assistant");
        assert_eq!(asst.tool_calls.as_ref().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parallel_safe_batch_only_read_and_view() {
        let call = |id: &str, name: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        };
        let read = call("r", "read");
        let view = call("v", "view");
        assert!(parallel_safe_batch(&[read.clone(), view.clone()]));
        assert!(parallel_safe_batch(&[read.clone(), call("s", "search")]));
        assert!(parallel_safe_batch(&[read.clone(), call("q", "ask")]));
        assert!(!parallel_safe_batch(&[read.clone()]));
        assert!(!parallel_safe_batch(&[read.clone(), call("m", "mcp")]));
        assert!(!parallel_safe_batch(&[
            call("q", "ask"),
            call("w", "write")
        ]));
        assert!(!parallel_safe_batch(&[view, call("s", "skill")]));
        assert!(!parallel_safe_batch(&[
            call("a", "read"),
            call("b", "memory_search")
        ]));
    }

    #[tokio::test]
    async fn prefix_budget_stops() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.working_window = 10;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
            meter: true,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
        assert_eq!(out.text, "should not run");
        assert!(out.steps >= 1);
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        assert!(
            hidden.iter().any(|c| c.contains(PHYSICS_WRAP_NOTE)),
            "context cap should wrap, not tombstone: {hidden:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compact_ratio_soft_limit_clamped() {
        assert_eq!(compact_soft_limit(1000, 0.70), 700);
        assert_eq!(compact_soft_limit(1000, 0.05), 100);
        assert_eq!(compact_soft_limit(1000, 2.0), 1000);
        assert_eq!(compact_soft_limit(262_144, 0.70), (262_144.0 * 0.70) as u32);
        assert_eq!(clamp_compact_ratio(f64::NAN), DEFAULT_COMPACT_RATIO);
    }

    #[test]
    fn compact_soft_does_not_hard_fail() {
        assert!(over_soft_threshold(800, 0, 1000, 0.70));
        assert!(!over_hard_threshold(800, 0, 1000));
        assert!(over_hard_threshold(1001, 0, 1000));
        assert!(!over_soft_threshold(500, 0, 1000, 0.70));
        assert!(!over_soft_threshold(200_000, 0, 0, 0.70));
        assert!(!over_hard_threshold(200_000, 0, 0));
    }

    #[test]
    fn turn_start_compact_at_120k_or_soft() {
        // 160k is under 262144 * 0.70 ≈ 183k but must compact on a follow-up.
        assert!(!over_soft_threshold(160_000, 0, 262_144, 0.70));
        assert!(should_compact_at_user_turn(160_000, 0, 262_144, 0.70));
        assert!(!should_compact_at_user_turn(100_000, 0, 262_144, 0.70));
        // Small-window tests hit the soft path, not a 120k fixture.
        assert!(should_compact_at_user_turn(800, 0, 1000, 0.70));
        assert!(!should_compact_at_user_turn(200_000, 0, 0, 0.70));
    }

    #[test]
    fn follow_up_compacts_tool_heavy_even_under_120k() {
        assert!(!should_compact_at_user_turn(1_000, 0, 500_000, 0.70));
        assert!(should_compact_follow_up(1_000, 0, 500_000, 0.70, 8, 0, 8));
        assert!(!should_compact_follow_up(1_000, 0, 500_000, 0.70, 7, 0, 8));
        assert!(should_compact_follow_up(1_000, 0, 500_000, 0.70, 6, 0, 6));
        assert!(should_compact_follow_up(1_000, 0, 500_000, 0.70, 0, 5, 8));
        assert!(!should_compact_follow_up(1_000, 0, 0, 0.70, 8, 5, 8));
    }

    #[test]
    fn estimate_skips_data_uri_payload() {
        let mut msg = ChatMessage::tool("1", "screenshot ok");
        msg.parts = vec![crate::media::MediaPart::image_url(format!(
            "data:image/png;base64,{}",
            "A".repeat(2_000_000)
        ))];
        let n = estimate_prefix_tokens(std::slice::from_ref(&msg), &[], false);
        assert!(
            n < 2_000,
            "data URI must not dominate the compact gate: {n}"
        );
    }

    #[tokio::test]
    async fn prefix_hard_window_still_budgets() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.working_window = 10;
        o.compact_ratio = 0.10;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("should not run")])),
            meter: true,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
        assert_eq!(out.text, "should not run");
        assert!(out.steps >= 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn follow_up_archives_tool_heavy_turn_without_tokenizer() {
        let dir = std::env::temp_dir().join(format!("q38-fu-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        for i in 0..8 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let mut o = opts(&dir);
        o.persist_session = true;
        o.session_id = "fu1".into();
        o.session_dir = Some(sess.clone());
        o.max_steps = 20;
        let mut turns = VecDeque::new();
        for i in 0..8 {
            turns.push_back(turn_tool_id(
                &format!("c{i}"),
                "read",
                json!({"path": format!("f{i}.txt")}),
            ));
        }
        turns.push_back(turn_text("first done"));
        turns.push_back(turn_text("second done"));
        let scripted = Scripted {
            turns: Mutex::new(turns),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let first = agent.run("read the eight files").await.unwrap();
        assert_eq!(first.text, "first done", "{:?}", first.stop_reason);
        let live_tools = agent.messages.iter().filter(|m| m.role == "tool").count();
        assert!(
            live_tools >= 8,
            "first turn should still hold its tools: {live_tools}"
        );
        let second = agent.run("what did you find").await.unwrap();
        assert_eq!(second.text, "second done", "{:?}", second.stop_reason);
        let live_tools = agent.messages.iter().filter(|m| m.role == "tool").count();
        assert_eq!(
            live_tools, 0,
            "follow-up must archive the previous tool turn, not replay it: {live_tools}"
        );
        let log = SessionLog::open_in(&sess, "fu1").unwrap();
        assert!(
            log.events()
                .iter()
                .any(|e| matches!(e, SessionEvent::Compact(_))),
            "tool-heavy follow-up should compact without a prefix meter: {:?}",
            log.events()
                .iter()
                .map(|e| e.type_name())
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn prefix_soft_ratio_compacts_without_hard_error() {
        let dir = std::env::temp_dir().join(format!("q38-soft-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        let mut o = opts(&dir);
        o.persist_session = true;
        o.session_id = "soft1".into();
        o.session_dir = Some(sess.clone());
        o.working_window = 8000;
        o.compact_ratio = 0.10;
        o.generation_reserve = 0;
        o.max_steps = 8;
        let blob = "W".repeat(8000);
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool_id("c1", "write", json!({"path": "fat.txt", "content": blob})),
                turn_tool_id("c2", "read", json!({"path": "fat.txt"})),
                turn_text("first done"),
                turn_text("second done"),
            ])),
            meter: true,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let pad = "token ".repeat(400);
        let first = agent
            .run(&format!("task one then keep going {pad}"))
            .await
            .unwrap();
        assert!(
            !first
                .stop_reason
                .as_deref()
                .unwrap_or("")
                .starts_with("budget:context"),
            "soft threshold must not budget:context: {:?}",
            first.stop_reason
        );
        let second = agent.run("follow up please").await.unwrap();
        assert_eq!(
            second.text, "second done",
            "follow-up hit {:?}",
            second.stop_reason
        );
        assert!(
            !second
                .stop_reason
                .as_deref()
                .unwrap_or("")
                .starts_with("budget:context"),
            "soft compact must not fail the hard window: {:?}",
            second.stop_reason
        );
        let log = SessionLog::open_in(&sess, "soft1").unwrap();
        assert!(
            log.events()
                .iter()
                .any(|e| matches!(e, SessionEvent::Compact(_))),
            "follow-up over soft should archive previous turns: {:?}",
            log.events()
                .iter()
                .map(|e| e.type_name())
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn malformed_then_valid_still_works() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_parse_fail("<tool_call>nope</tool_call>"),
                turn_text("fixed it"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "fixed it");
        assert_eq!(out.steps, 2);
        assert!(
            !agent.messages.iter().any(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.contains("malformed") || c.contains("Resend a valid"))
            }),
            "parse retry must not lecture the model"
        );
        assert_eq!(
            agent
                .messages
                .iter()
                .filter(|m| m.role == "assistant")
                .count(),
            1,
            "malformed step is dropped, not kept as an assistant turn"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn two_parse_fails_upgrade_then_clean_drops_back() {
        let dir =
            std::env::temp_dir().join(format!("q38-effort-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let watch = PolicyWatch {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([
                    turn_parse_fail("<tool_call>nope</tool_call>"),
                    turn_parse_fail("<tool_call>still-bad</tool_call>"),
                    turn_text("ok"),
                ])),
                meter: false,
            },
            policy: Mutex::new(ThinkPolicy::agent_default()),
            seen: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let seen = watch.seen.clone();
        let mut agent = Agent::new(watch, opts(&dir)).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "ok");
        let seen = seen.lock().expect("seen").clone();
        assert!(
            seen.iter()
                .any(|p| p.effort == Some(Effort::Medium) && p.max_think_tokens == 2048),
            "never upgraded: {seen:?}"
        );
        let last = seen.last().expect("set_policy");
        assert_eq!(last.effort, Some(Effort::Low));
        assert_eq!(last.max_think_tokens, 512);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn watchdog_soft_nudge_recovers_without_policy_control() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                ModelTurn::watchdog(),
                turn_text("recovered"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "recovered");
        assert_eq!(out.steps, 2);
        assert!(agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.contains(THINK_DIVERGENCE_NOTE))
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn watchdog_second_cap_stops_without_disabling_thinking() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let start = ThinkPolicy::effort_with(&crate::policy::ThinkBudget::default(), Effort::Low);
        assert_eq!(start.max_tokens, 8192);
        // 两连 watchdog：已经提醒并给过空间，按硬资源上限停止，不再用
        // thinking-off 答案替换模型自己的推理策略。
        let watch = PolicyWatch {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([
                    ModelTurn::watchdog(),
                    ModelTurn::watchdog(),
                    turn_text("should-not-run"),
                ])),
                meter: false,
            },
            policy: Mutex::new(start),
            seen: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let seen = watch.seen.clone();
        let mut agent = Agent::new(watch, opts(&dir)).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert!(out.text.is_empty(), "{}", out.text);
        assert_eq!(out.stop_reason, None);
        let seen = seen.lock().expect("seen").clone();
        assert_eq!(
            seen.iter().filter(|p| !p.enabled).count(),
            0,
            "watchdog must not replace model policy with thinking-off: {seen:?}"
        );
        assert!(
            seen.iter().any(|p| {
                p.enabled
                    && p.max_think_tokens == NO_TOOL_THINK_FLOOR
                    && p.max_tokens >= NO_TOOL_THINK_FLOOR + NO_TOOL_ANSWER_RESERVE
            }),
            "roomy retry must retain answer reserve: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn watchdog_widens_thinking_and_keeps_it_enabled() {
        // M002 类：无工具轮 watchdog 命中，先按 NO_TOOL_THINK_FLOOR 升档重试，
        // 成功则全程不关思考。
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let start = ThinkPolicy::effort_with(&crate::policy::ThinkBudget::default(), Effort::Low);
        assert!(start.enabled && start.max_think_tokens < NO_TOOL_THINK_FLOOR);
        let watch = PolicyWatch {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([
                    ModelTurn::watchdog(),
                    turn_text("recovered"),
                ])),
                meter: false,
            },
            policy: Mutex::new(start),
            seen: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let seen = watch.seen.clone();
        let mut agent = Agent::new(watch, opts(&dir)).unwrap();
        let out = agent.run("7^222 mod 1000 等于多少？").await.unwrap();
        assert_eq!(out.text, "recovered");
        let seen = seen.lock().expect("seen").clone();
        assert!(
            seen.iter()
                .any(|p| p.enabled && p.max_think_tokens == NO_TOOL_THINK_FLOOR),
            "watchdog must retry with the widened think floor first: {seen:?}"
        );
        assert!(
            seen.iter().all(|p| p.enabled),
            "successful widened retry must keep the model-selected thinking mode: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn watchdog_after_tool_round_also_keeps_model_policy() {
        // 用过工具也不改变原则：事实提醒 + 原推理模式下的一次宽预算重试。
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.peripheral = false;
        let start = ThinkPolicy::effort_with(&crate::policy::ThinkBudget::default(), Effort::Low);
        let watch = PolicyWatch {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([
                    turn_tool("read", json!({"path": "ping.txt"})),
                    ModelTurn::watchdog(),
                    turn_text("recovered"),
                ])),
                meter: false,
            },
            policy: Mutex::new(start),
            seen: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let seen = watch.seen.clone();
        let mut agent = Agent::new(watch, o).unwrap();
        let out = agent.run("read ping.txt then answer").await.unwrap();
        assert_eq!(out.text, "recovered");
        let seen = seen.lock().expect("seen").clone();
        assert!(
            seen.iter().all(|p| p.enabled),
            "tool-using retry must not disable the model's thinking: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|p| p.enabled && p.max_think_tokens == NO_TOOL_THINK_FLOOR),
            "tool-using turn should get the same roomy retry: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn watchdog_second_empty_cap_ends_quietly() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                ModelTurn::watchdog(),
                ModelTurn::watchdog(),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.stop_reason, None);
        assert!(out.text.is_empty(), "{}", out.text);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn doom_warns_once_then_lets_model_stop() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        let ping = json!({"path": "ping.txt"});
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_text("wrapped up"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read ping.txt").await.unwrap();
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        assert_eq!(out.text, "wrapped up");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Doom loop"),
            "{:?}",
            out.stop_reason
        );
        let warns = hidden
            .iter()
            .filter(|c| c.contains(crate::paw_loop::REPEAT_NOTE))
            .count();
        assert_eq!(warns, 1, "repeat fact lands exactly once: {hidden:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn doom_warned_model_that_pivots_is_not_halted() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        std::fs::write(dir.join("pong.txt"), "ping\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "ping.txt"})),
                turn_tool("read", json!({"path": "ping.txt"})),
                turn_tool("read", json!({"path": "ping.txt"})),
                turn_tool("read", json!({"path": "pong.txt"})),
                turn_text("pivoted"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read the files").await.unwrap();
        assert_eq!(out.text, "pivoted");
        assert!(
            !out.stop_reason.as_deref().unwrap_or("").contains("Doom"),
            "{:?}",
            out.stop_reason
        );
        let warned = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .filter_map(|m| m.content.as_deref())
            .any(|c| c.contains(crate::paw_loop::REPEAT_NOTE));
        assert!(warned, "warn fact must land at the 3rd identical call");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn blind_overwrite_refused_until_read() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.md"), "precious original\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("write", json!({"path": "notes.md", "content": "blind"})),
                turn_tool("read", json!({"path": "notes.md"})),
                turn_tool("write", json!({"path": "notes.md", "content": "informed"})),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("rewrite notes.md").await.unwrap();
        assert_eq!(out.text, "done");
        let veto = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|c| c.contains("已存在") && c.contains("notes.md"));
        assert!(veto, "first write must be refused with a re-read fact");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "informed",
            "post-read write must land"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn denied_writes_are_not_marked_observed_on_rebuild() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        let write_call = |id: &str, path: &str| {
            json!({
                "id": id,
                "type": "function",
                "function": {"name": "write", "arguments": {"path": path, "content": "x"}}
            })
        };
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("task"),
            // 新契约文案（Error: 前缀）与三种旧失败文案都不得算已观察。
            ChatMessage::assistant_tools(None, vec![write_call("c1", "a.md")]),
            ChatMessage::tool("c1", permit::plan_denied("write")),
            ChatMessage::assistant_tools(None, vec![write_call("c2", "b.md")]),
            ChatMessage::tool(
                "c2",
                "plan mode: `write` blocked. Stay read-only and put the plan in your reply.",
            ),
            ChatMessage::assistant_tools(None, vec![write_call("c3", "c.md")]),
            ChatMessage::tool("c3", "User denied `write`. Continue without that call."),
            ChatMessage::assistant_tools(None, vec![write_call("c4", "d.md")]),
            ChatMessage::tool("c4", "tool task aborted"),
            // 成功的 write 仍然算已观察。
            ChatMessage::assistant_tools(None, vec![write_call("c5", "ok.md")]),
            ChatMessage::tool("c5", "Wrote 1 lines to ok.md"),
        ];
        let observed = observed_from_messages(&msgs, &ws);
        for denied in ["a.md", "b.md", "c.md", "d.md"] {
            assert!(
                !observed.contains(&canon_ws_path(&ws, denied)),
                "denied `{denied}` must not be observed: {observed:?}"
            );
        }
        assert!(observed.contains(&canon_ws_path(&ws, "ok.md")));
        // coordinator 中断文案同样不算。
        msgs.push(ChatMessage::assistant_tools(
            None,
            vec![write_call("c6", "e.md")],
        ));
        msgs.push(ChatMessage::tool("c6", "cancelled"));
        let observed = observed_from_messages(&msgs, &ws);
        assert!(!observed.contains(&canon_ws_path(&ws, "e.md")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn plan_denied_write_stays_guarded_after_plan_go() {
        // P0 回归：plan 模式拒绝一次 write 后 /plan go，重建的 observed_paths
        // 不得把该路径当已观察 —— 盲覆写守卫必须仍然拦截。
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.md"), "precious original\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        o.plan_mode = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("write", json!({"path": "notes.md", "content": "blind"})),
                turn_text("## plan\n- rewrite notes.md"),
                // /plan go 后的第二轮：仍未 read 就 write，必须再次被守卫拦下。
                turn_tool(
                    "write",
                    json!({"path": "notes.md", "content": "still blind"}),
                ),
                turn_text("blocked again"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let _ = agent.run("plan a rewrite of notes.md").await.unwrap();
        agent.plan_mode = false;
        let out = agent.run("go implement").await.unwrap();
        assert_eq!(out.text, "blocked again");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "precious original\n",
            "blind overwrite must stay refused after plan-denied write"
        );
        let veto = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|c| c.contains("已存在") && c.contains("notes.md"));
        assert!(veto, "second write must hit the blind-overwrite guard");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn dot_slash_read_then_plain_write_passes_guard() {
        // canon_ws_path 回归：read("./a.rs") 后 write("a.rs") 不得被误拒。
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn old() {}\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "./a.rs"})),
                turn_tool("write", json!({"path": "a.rs", "content": "fn new() {}\n"})),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("rewrite a.rs").await.unwrap();
        assert_eq!(out.text, "done");
        let veto = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|c| c.contains("已存在"));
        assert!(!veto, "./a.rs read must cover a.rs write");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).unwrap(),
            "fn new() {}\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn write_to_new_file_needs_no_read() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("write", json!({"path": "fresh.md", "content": "hello"})),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("create fresh.md").await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(
            std::fs::read_to_string(dir.join("fresh.md")).unwrap(),
            "hello"
        );
        let veto = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.content.as_deref())
            .any(|c| c.contains("已存在"));
        assert!(!veto, "fresh files must not pay the read tax");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn web_check_trigger_is_narrow() {
        assert!(wants_web_check("2026年最新的 Rust 版本是多少？"));
        assert!(wants_web_check("苹果 M5 芯片什么时候发布？"));
        assert!(wants_web_check("看下 https://example.com/post 说了什么"));
        assert!(wants_web_check("what is the latest release of tokio?"));
        assert!(!wants_web_check("fix the parser in src/a.py"));
        assert!(!wants_web_check("把这个函数的价格字段改成 f64"));
        assert!(!wants_web_check("最新版本是多少？不要联网"));
        assert!(!wants_web_check("订单号 2026110234 查一下状态"));
    }

    #[test]
    fn numeric_check_trigger_is_quantitative_and_non_code() {
        assert!(wants_numeric_check(
            "预测者准确率为 99%。比较两种决策论，并讨论错误率是否消除争议。"
        ));
        assert!(wants_numeric_check(
            "Calculate the probability threshold and distinguish percent from percentage points."
        ));
        assert!(!wants_numeric_check("这个模型准确率 99%，挺不错。"));
        assert!(!wants_numeric_check(
            "修复 accuracy.py 中把 99% 写成 0.99 的函数和测试用例。"
        ));
        assert!(!wants_numeric_check("比较两个哲学家的自由意志观点。"));
    }

    #[tokio::test]
    async fn numeric_check_hint_is_one_short_task_local_card() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("one box"), turn_text("hello")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        agent
            .run("预测者准确率为 99%。比较两种理论，并讨论概率错误。")
            .await
            .unwrap();
        agent.run("你好").await.unwrap();
        let hints = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .filter_map(|m| m.content.as_deref())
            .filter(|c| c.contains("[verify:numeric]"))
            .count();
        assert_eq!(hints, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn web_hint_lands_only_on_fresh_questions() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.peripheral = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_text("答：见来源。"),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        assert!(
            crate::tools_schema::has_tool(&agent.tools, "web"),
            "default config arms web"
        );
        let _ = agent.run("2026年最新的 Rust 稳定版是多少？").await.unwrap();
        let hinted = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .filter_map(|m| m.content.as_deref())
            .any(|c| c.contains("[web]"));
        assert!(hinted, "fresh question must carry the web hint");

        let _ = agent.run("refactor the loop in main.rs").await.unwrap();
        let hints = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .filter_map(|m| m.content.as_deref())
            .filter(|c| c.contains("[web]"))
            .count();
        assert_eq!(hints, 1, "code task must not re-fire the hint");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn overlay_window_injects_one_hidden_note() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.working_window = 8000;
        o.working_window_overlay = Some(WorkingWindowOverlay {
            from_file: crate::config::CODING_CTX_TOKENS,
            from_env: 8000,
        });
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("ok")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let _ = agent.run("hi").await.unwrap();
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        assert!(
            hidden
                .iter()
                .any(|c| c.contains("Q38_WORKING_WINDOW=8000") && c.contains("262144")),
            "{hidden:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn doom_text_reply_before_halt_is_not_halt() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        let ping = json!({"path": "ping.txt"});
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_text("obsidian-compact"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read ping.txt then stop").await.unwrap();
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        assert!(
            hidden.iter().all(|c| !c.contains("Repetitive pattern")),
            "no doom lecture: {hidden:?}"
        );
        assert_eq!(out.text, "obsidian-compact");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Doom loop"),
            "text reply must not inherit the tool-loop halt: {:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn restated_dump_is_deferred_then_model_decides() {
        let dir =
            std::env::temp_dir().join(format!("q38-restate-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
        let again = essay.replace("in detail", "carefully");
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(essay, "write", json!({"path": "a.md", "content": essay})),
                turn_said(&again, "write", json!({"path": "b.md", "content": again})),
                turn_text("should-not-run"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("how well does this fit the model").await.unwrap();
        assert_eq!(out.stop_reason, None);
        assert_eq!(out.text, "should-not-run");
        assert!(dir.join("a.md").is_file(), "first write should run");
        assert!(
            !dir.join("b.md").is_file(),
            "restated dump write is deferred until the model reassesses"
        );
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        assert_eq!(
            hidden
                .iter()
                .filter(|c| c.contains(crate::stutter::DUMP_NOTE))
                .count(),
            1,
            "one concise side observation: {hidden:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn spoken_then_cleanup_is_deferred_not_hard_stopped() {
        let dir =
            std::env::temp_dir().join(format!("q38-keeprm-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(
                    essay,
                    "write",
                    json!({"path": "report.md", "content": essay}),
                ),
                turn_tool("bash", json!({"command": "rm -f report.md; echo done"})),
                turn_text("should-not-run"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("write a report").await.unwrap();
        assert_eq!(out.stop_reason, None);
        assert_eq!(out.text, "should-not-run");
        assert!(
            dir.join("report.md").is_file(),
            "cleanup waits for reassessment"
        );
        assert!(agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.contains(crate::stutter::DUMP_NOTE))
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn restated_plus_unique_doc_write_continues() {
        let dir = std::env::temp_dir().join(format!("q38-docs-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
        let again = essay.replace("in detail", "carefully");
        let doc =
            "CONTRIBUTING\n\nFork the repo, open a PR against main, run cargo test -p q38-loop \
--lib before you push. Do not bump the frozen tools array. Add a live scene only when the \
public llama.cpp endpoint is up. Name the branch after the ticket. Ask for review from William.";
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(
                    essay,
                    "write",
                    json!({"path": "notes.md", "content": essay}),
                ),
                turn_said(
                    &again,
                    "write",
                    json!({"path": "CONTRIBUTING.md", "content": doc}),
                ),
                turn_text("docs-done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent
            .run("write notes and a contributing guide")
            .await
            .unwrap();
        assert_eq!(out.text, "docs-done");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("budget:repeat"),
            "{:?}",
            out.stop_reason
        );
        assert!(dir.join("notes.md").is_file());
        assert!(dir.join("CONTRIBUTING.md").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn restated_plus_edit_continues() {
        let dir = std::env::temp_dir().join(format!("q38-edit-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() { println!(\"a\"); }\n").unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
        let again = essay.replace("in detail", "carefully");
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(essay, "read", json!({"path": "main.rs"})),
                turn_said(
                    &again,
                    "edit",
                    json!({
                        "path": "main.rs",
                        "old_string": "println!(\"a\")",
                        "new_string": "println!(\"b\")"
                    }),
                ),
                turn_text("code-done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("fix the banner").await.unwrap();
        assert_eq!(out.text, "code-done");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("budget:repeat"),
            "{:?}",
            out.stop_reason
        );
        let body = std::fs::read_to_string(dir.join("main.rs")).unwrap();
        assert!(body.contains("println!(\"b\")"), "{body}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn placeholder_write_then_cleanup_gets_soft_reassessment() {
        let dir =
            std::env::temp_dir().join(format!("q38-ellipsis-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("loop.rs"), "fn drive() {}\n").unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let junk = dir.join("...").display().to_string();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "loop.rs"})),
                {
                    let mut t = turn_tools(vec![
                        ("c1", "write", json!({"path": "...", "content": "..."})),
                        ("c2", "read", json!({"path": "loop.rs", "offset": 1})),
                    ]);
                    t.content = essay.into();
                    t
                },
                {
                    let mut t = turn_tools(vec![
                        (
                            "c3",
                            "bash",
                            json!({"command": format!("ls -la {junk} 2>/dev/null && cat {junk} && rm {junk} && echo REMOVED")}),
                        ),
                        ("c4", "read", json!({"path": "loop.rs"})),
                    ]);
                    t.content = "先清理一个误操作产生的杂散文件，然后继续读核心 loop。".into();
                    t
                },
                turn_text("should-not-run"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("look at the core loop").await.unwrap();
        assert_eq!(out.stop_reason, None);
        assert_eq!(out.text, "should-not-run");
        assert!(
            !std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name() == "..."),
            "placeholder write must not land"
        );
        assert!(agent.messages.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.contains(crate::stutter::DUMP_NOTE))
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn after_answer_new_file_read_continues() {
        let dir =
            std::env::temp_dir().join(format!("q38-newread-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn b() {}\n").unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(essay, "read", json!({"path": "a.rs"})),
                turn_said("再看一个文件。", "read", json!({"path": "b.rs"})),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read both files").await.unwrap();
        assert_eq!(out.text, "done");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("budget:repeat"),
            "{:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn restated_plus_send_bash_continues() {
        let dir = std::env::temp_dir().join(format!("q38-send-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";
        let again = essay.replace("in detail", "carefully");
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(essay, "write", json!({"path": "out.md", "content": essay})),
                turn_said(
                    &again,
                    "bash",
                    json!({"command": "cp out.md sent.md && echo sent"}),
                ),
                turn_text("sent-done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("write and copy the report").await.unwrap();
        assert_eq!(out.text, "sent-done");
        assert!(dir.join("sent.md").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn truncated_intro_promotes_write_body_into_reply() {
        let dir =
            std::env::temp_dir().join(format!("q38-promote-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template.";
        let body = format!(
            "{stub} Adapter builds OpenAI-compat requests. Sticky notes hold skill and MCP cards. \
This is a strong fit for the 27B local model because the prefix is byte-stable."
        );
        let mut o = opts(&dir);
        o.max_steps = 4;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(stub, "write", json!({"path": "report.md", "content": body})),
                turn_text("stop-here"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("analyze the harness").await.unwrap();
        assert_eq!(out.text, "stop-here");
        let spoken = agent
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .filter_map(|m| m.content.clone())
            .find(|c| c.contains("byte-stable"))
            .unwrap_or_default();
        assert!(
            spoken.contains("byte-stable"),
            "chat bubble should hold the write body, not the stub: {spoken:?}"
        );
        assert!(dir.join("report.md").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn short_status_plus_tools_is_not_a_restate() {
        let dir =
            std::env::temp_dir().join(format!("q38-status-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let ping = json!({"path": "ping.txt"});
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said("好的，我继续读。", "read", ping.clone()),
                turn_said(
                    "好的，我继续读。",
                    "read",
                    json!({"path": "ping.txt", "offset": 1}),
                ),
                turn_text("obsidian-compact"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read ping.txt then stop").await.unwrap();
        assert_eq!(out.text, "obsidian-compact");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("budget:repeat"),
            "{:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn long_narration_plus_reread_is_not_a_dump() {
        let dir =
            std::env::temp_dir().join(format!("q38-narrate-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("loop.rs"), "fn drive() {}\n").unwrap();
        let talk = "`parallel_safe_batch` restricts parallel to read and view only, so the \
dispatch asymmetry is safe in practice. Let me check run, tool surface building, \
and the system prompt assembly next.";
        assert!(
            crate::stutter::is_substantial_reply(talk),
            "fixture must be long enough to have tripped the old lock"
        );
        let mut o = opts(&dir);
        o.max_steps = 8;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(talk, "read", json!({"path": "loop.rs"})),
                turn_said(talk, "read", json!({"path": "loop.rs", "offset": 1})),
                turn_tool("bash", json!({"command": "grep -n drive loop.rs"})),
                turn_text("wiring looks sound. unique reads and grep are real work."),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("review the wiring").await.unwrap();
        assert!(out.text.contains("wiring looks sound"), "{}", out.text);
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("budget:repeat"),
            "{:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn checkpoint_then_expanded_answer_continues() {
        let dir =
            std::env::temp_dir().join(format!("q38-expand-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let essay = "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen. Wiring of skills and mcp is a hidden-card overlay.";
        let checkpoint: String = essay.chars().take(220).collect();
        assert!(crate::stutter::is_substantial_reply(&checkpoint));
        let mut o = opts(&dir);
        o.max_steps = 6;
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_said(
                    &checkpoint,
                    "write",
                    json!({"path": "notes.md", "content": checkpoint}),
                ),
                turn_text(essay),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("review the loop").await.unwrap();
        assert_eq!(out.text, essay);
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("budget:repeat"),
            "{:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_doom_notes_once_then_lets_model_stop() {
        let dir = std::env::temp_dir().join(format!("q38-lossy-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        o.low_precision = true;
        let ping = json!({"path": "ping.txt"});
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", ping.clone()),
                turn_tool("read", ping.clone()),
                turn_text("wrapped up"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read ping.txt").await.unwrap();
        assert_eq!(out.text, "wrapped up");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Doom loop"),
            "{:?}",
            out.stop_reason
        );
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        let warns = hidden
            .iter()
            .filter(|c| c.contains(crate::paw_loop::REPEAT_NOTE))
            .count();
        assert_eq!(warns, 1, "repeat fact lands exactly once: {hidden:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_name_streak_allows_distinct_bash_commands() {
        let dir =
            std::env::temp_dir().join(format!("q38-streak2-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        o.low_precision = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("bash", json!({"command": "true"})),
                turn_tool("bash", json!({"command": "echo a"})),
                turn_tool("bash", json!({"command": "echo b"})),
                turn_tool("bash", json!({"command": "echo c"})),
                turn_text("explored"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("run commands").await.unwrap();
        assert_eq!(out.text, "explored");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Name streak"),
            "{:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_read_edit_read_is_not_name_streak() {
        let dir = std::env::temp_dir().join(format!("q38-rer-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        o.low_precision = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "ping.txt"})),
                turn_tool(
                    "edit",
                    json!({"path": "ping.txt", "old_string": "pong", "new_string": "pong"}),
                ),
                turn_tool("read", json!({"path": "other.txt"})),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("tweak ping").await.unwrap();
        assert_eq!(out.text, "done");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Name streak"),
            "{:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_read_edit_read_same_path_is_not_path_loop() {
        let dir = std::env::temp_dir().join(format!("q38-rer2-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        o.low_precision = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "ping.txt"})),
                turn_tool(
                    "edit",
                    json!({"path": "ping.txt", "old_string": "pong", "new_string": "ping"}),
                ),
                turn_tool("read", json!({"path": "ping.txt"})),
                turn_text("done"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("tweak ping").await.unwrap();
        assert_eq!(out.text, "done");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Path loop"),
            "{:?}",
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_same_path_edits_note_once_then_continue() {
        // 分页 read 不算 Path loop；一字不差的重读由 doom 观察。
        // 同路径连续 edit/write 只注入一次轨迹观察，然后交给模型。
        let dir = std::env::temp_dir().join(format!("q38-path-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        o.low_precision = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "ping.txt"})),
                turn_tool(
                    "edit",
                    json!({"path": "ping.txt", "old_string": "pong", "new_string": "p1"}),
                ),
                turn_tool(
                    "edit",
                    json!({"path": "ping.txt", "old_string": "p1", "new_string": "p2"}),
                ),
                turn_tool(
                    "edit",
                    json!({"path": "ping.txt", "old_string": "p2", "new_string": "p3"}),
                ),
                turn_text("wrapped up"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("edit ping").await.unwrap();
        assert_eq!(out.text, "wrapped up");
        assert!(
            !out.stop_reason
                .as_deref()
                .unwrap_or("")
                .contains("Path loop"),
            "{:?}",
            out.stop_reason
        );
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        let notes = hidden
            .iter()
            .filter(|c| c.contains(crate::paw_loop::PATH_NOTE))
            .count();
        assert_eq!(notes, 1, "path fact lands exactly once: {hidden:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_paged_reads_same_path_do_not_halt() {
        // 压缩注记教模型用 offset 翻页，翻页不得被 PathLoopGate 斩断。
        let dir = std::env::temp_dir().join(format!("q38-page-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n".repeat(50)).unwrap();
        let mut o = opts(&dir);
        o.max_steps = 12;
        o.peripheral = false;
        o.low_precision = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "ping.txt", "offset": 0})),
                turn_tool("read", json!({"path": "ping.txt", "offset": 10})),
                turn_tool("read", json!({"path": "ping.txt", "offset": 20})),
                turn_text("paged"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("read ping").await.unwrap();
        assert_eq!(out.text, "paged", "{:?}", out.stop_reason);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_stutter_notes_once_then_model_continues() {
        let dir =
            std::env::temp_dir().join(format!("q38-stutter-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.peripheral = false;
        o.low_precision = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("x\nx\nx\nx\n"), turn_text("ok")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(out.stop_reason, None);
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        let notes = hidden
            .iter()
            .filter(|c| c.contains(crate::stutter::STUTTER_NOTE))
            .count();
        assert_eq!(notes, 1, "stutter fact lands exactly once: {hidden:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_two_parse_fails_stop() {
        let dir = std::env::temp_dir().join(format!("q38-parse-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.peripheral = false;
        o.low_precision = true;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_parse_fail("<tool_call>nope</tool_call>"),
                turn_parse_fail("<tool_call>still-bad</tool_call>"),
                turn_text("ok"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(out.stop_reason, None, "{:?}", out.stop_reason);
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        let notes = hidden
            .iter()
            .filter(|c| c.contains(PARSE_REPAIR_NOTE))
            .count();
        assert_eq!(notes, 1, "parse repair lands once: {hidden:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lossy_think_cap_on_default_low() {
        let dir = std::env::temp_dir().join(format!("q38-cap-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let watch = PolicyWatch {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([turn_text("ok")])),
                meter: false,
            },
            policy: Mutex::new(ThinkPolicy::agent_default()),
            seen: std::sync::Arc::new(Mutex::new(Vec::new())),
        };
        let seen = watch.seen.clone();
        let mut o = opts(&dir);
        o.low_precision = true;
        let mut agent = Agent::new(watch, o).unwrap();
        let _ = agent.run("hi").await.unwrap();
        let seen = seen.lock().expect("seen").clone();
        assert!(
            seen.iter()
                .any(|p| p.max_think_tokens == crate::policy::LOSSY_THINK_CAP),
            "never applied 384 cap: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn turn_tool_id(id: &str, name: &str, args: Value) -> ModelTurn {
        let mut t = turn_tool(name, args);
        t.tool_calls[0].id = id.into();
        t
    }

    #[tokio::test]
    async fn prefix_budget_compacts_then_runs() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        let mut o = opts(&dir);
        o.persist_session = true;
        o.session_id = "c2".into();
        o.session_dir = Some(sess.clone());
        o.working_window = 2800;
        o.generation_reserve = 0;
        o.max_steps = 12;
        let home = o.home.clone();
        let blob = "W".repeat(8000);
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool_id("c1", "write", json!({"path": "fat.txt", "content": blob})),
                turn_tool_id("c2", "read", json!({"path": "fat.txt"})),
                turn_tool_id(
                    "c3",
                    "bash",
                    json!({"command": "python3 -c \"print('Y'*8000)\""}),
                ),
                turn_text("done after compact"),
            ])),
            meter: true,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("task one then keep going").await.unwrap();
        let reason = out.stop_reason.clone().unwrap_or_default();
        assert!(
            out.text.contains("done after compact") || reason.starts_with("budget:context"),
            "text={} reason={reason}",
            out.text
        );
        let log = SessionLog::open_in(&sess, "c2").unwrap();
        let kinds: Vec<_> = log.events().iter().map(|e| e.type_name()).collect();
        if kinds.iter().any(|k| *k == "session/compact") {
            assert!(has_recall(&agent.tools), "recall appended after compact");
            let live_users: Vec<_> = agent
                .messages
                .iter()
                .filter(|m| m.role == "user")
                .map(|m| m.content.clone().unwrap_or_default())
                .collect();
            assert!(
                live_users
                    .iter()
                    .any(|c| crate::template::is_hidden_user_text(c)),
                "archive must be hidden: {live_users:?}"
            );
            assert!(
                log.events()
                    .iter()
                    .any(|e| matches!(e, SessionEvent::User(u) if u.text.contains("task one"))),
                "JSONL still has the original user"
            );
            let notes = std::fs::read_dir(home.as_ref().unwrap().join("memory"))
                .map(|rd| rd.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
            assert!(notes > 0, "compact should write a daily note under memory/");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn persist_session_writes_jsonl() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        let mut o = opts(&dir);
        o.persist_session = true;
        o.session_id = "p1".into();
        o.session_dir = Some(sess.clone());
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("done")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.session_id, "p1");
        let log = SessionLog::open_in(&sess, "p1").unwrap();
        let kinds: Vec<_> = log.events().iter().map(|e| e.type_name()).collect();
        assert_eq!(kinds, ["session/start", "user", "assistant", "stop"]);
        assert_eq!(log.messages().len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn emit_sink_streams_tool_then_assistant_not_user() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "abc\n").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("read", json!({"path": "note.txt"})),
                turn_text("abc"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, opts(&dir)).unwrap();
        agent.set_emit(crate::sidecar::EventSink { tx });
        let out = agent.run("read the note").await.unwrap();
        assert_eq!(out.text, "abc");
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() {
            kinds.push(e.type_name().to_string());
        }
        assert!(
            !kinds.iter().any(|k| k == "user"),
            "live user/skill cards stay off the TUI stream: {kinds:?}"
        );
        assert!(kinds.iter().any(|k| k == "tool"), "{kinds:?}");
        assert!(kinds.iter().any(|k| k == "assistant"), "{kinds:?}");
        assert_eq!(kinds.last().map(String::as_str), Some("stop"), "{kinds:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn token_deltas_reach_sink_before_assistant_and_skip_jsonl() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        struct StreamingScripted {
            turn: Mutex<Option<ModelTurn>>,
            sink: Mutex<Option<TokenSink>>,
        }
        impl Completer for StreamingScripted {
            async fn complete(
                &self,
                _messages: &[ChatMessage],
                _tools: Option<&[Value]>,
            ) -> Result<ModelTurn> {
                let turn = self
                    .turn
                    .lock()
                    .expect("turn")
                    .take()
                    .ok_or_else(|| Error::msg("script exhausted"))?;
                if let Some(sink) = self.sink.lock().expect("sink").clone() {
                    sink.reset();
                    for ch in turn.reasoning.chars() {
                        sink.reasoning(&ch.to_string());
                    }
                    for ch in turn.content.chars() {
                        sink.content(&ch.to_string());
                    }
                }
                Ok(turn)
            }

            fn set_token_sink(&self, sink: Option<TokenSink>) {
                *self.sink.lock().expect("sink") = sink;
            }
        }

        let mut reasoned = turn_text("hello");
        reasoned.reasoning = "hmm".into();
        let mut o = opts(&dir);
        o.persist_session = true;
        o.session_id = "delta1".into();
        o.session_dir = Some(sess.clone());
        let mut agent = Agent::new(
            StreamingScripted {
                turn: Mutex::new(Some(reasoned)),
                sink: Mutex::new(None),
            },
            o,
        )
        .unwrap();
        agent.set_emit(crate::sidecar::EventSink { tx });
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.text, "hello");
        let mut kinds = Vec::new();
        let mut reasoning = String::new();
        let mut content = String::new();
        let mut saw_reset = false;
        while let Ok(e) = rx.try_recv() {
            kinds.push(e.type_name().to_string());
            if let SessionEvent::Delta(d) = e {
                if d.reset {
                    saw_reset = true;
                } else if d.channel == crate::session::DeltaChannel::Reasoning {
                    reasoning.push_str(&d.text);
                } else {
                    content.push_str(&d.text);
                }
            }
        }
        let delta_at = kinds.iter().position(|k| k == "delta").expect("delta");
        let assistant_at = kinds
            .iter()
            .position(|k| k == "assistant")
            .expect("assistant");
        assert!(delta_at < assistant_at, "{kinds:?}");
        assert!(saw_reset);
        assert_eq!(reasoning, "hmm");
        assert_eq!(content, "hello");
        let log = SessionLog::open_in(&sess, "delta1").unwrap();
        let persisted: Vec<_> = log.events().iter().map(|e| e.type_name()).collect();
        assert_eq!(persisted, ["session/start", "user", "assistant", "stop"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn peripheral_off_keeps_frozen_four() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut o = opts(&dir);
        o.peripheral = false;
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("done")])),
            meter: false,
        };
        let agent = Agent::new(scripted, o).unwrap();
        let names: Vec<_> = agent
            .tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(&names[..4], ["read", "write", "edit", "bash"]);
        assert!(names.contains(&"search"));
        assert!(names.contains(&"view"));
        assert!(!names.contains(&"memory_search"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn memory_search_and_skill_dispatch() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join(".q38-home");
        let skill_dir = home.join("skills").join("pdf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: Extract text from PDFs\n---\nUse pdftotext on the file.\n",
        )
        .unwrap();
        let mut o = opts(&dir);
        o.home = Some(home.clone());
        let store = crate::memory::MemoryStore::open(&home).unwrap();
        store
            .write_compact_note("s", 1, "read crates/foo.rs linker rewrite")
            .unwrap();

        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("memory_search", json!({"query": "linker"})),
                turn_text("ok"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        assert!(crate::tools_schema::has_tool(&agent.tools, "memory_search"));
        assert!(!crate::tools_schema::has_tool(&agent.tools, "skill"));
        assert!(!crate::tools_schema::has_tool(&agent.tools, "mcp"));
        let sys = agent.messages[0].content.clone().unwrap_or_default();
        assert!(!sys.contains("MEMORY.md"));
        assert!(!sys.contains("pdf"));
        assert!(
            !sys.contains("Use pdftotext on the file"),
            "SKILL.md body must not be in system"
        );
        let out = agent.run("search then pdf").await.unwrap();
        assert_eq!(out.text, "ok");
        let tools: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.content.clone().unwrap_or_default())
            .collect();
        assert!(
            tools
                .iter()
                .any(|t| t.contains("linker") || t.contains("foo.rs")),
            "{tools:?}"
        );
        let hidden: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .collect();
        assert!(
            hidden.iter().any(|t| t.contains("pdftotext")),
            "skill body must be a hidden user, got {hidden:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn emitted_skill_call_runs_without_tools_entry() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join(".q38-home");
        let skill_dir = home.join("skills").join("pdf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: Extract text from PDFs\n---\nUse pdftotext on the file.\n",
        )
        .unwrap();
        let mut o = opts(&dir);
        o.home = Some(home);
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("skill", json!({"name": "pdf"})),
                turn_text("ok"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        assert!(!crate::tools_schema::has_tool(&agent.tools, "skill"));
        let out = agent.run("extract the pdf").await.unwrap();
        assert_eq!(out.text, "ok");
        let tools: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.content.clone().unwrap_or_default())
            .collect();
        assert!(
            tools
                .iter()
                .any(|t| t.contains("Use pdftotext on the file")),
            "skill call must return the body, got {tools:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn empty_script() -> Scripted {
        Scripted {
            turns: Mutex::new(VecDeque::new()),
            meter: false,
        }
    }

    fn hidden_texts(agent: &Agent<Scripted>) -> Vec<String> {
        agent
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn agent_md_line_is_frozen_system() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENT.md"), "客官来了。得嘞。不列清单。\n").unwrap();
        let agent = Agent::new(empty_script(), opts(&dir)).unwrap();
        let sys = agent.messages[0].content.clone().unwrap_or_default();
        assert!(sys.contains("客官来了。得嘞。不列清单。"), "{sys}");
        assert!(!sys.contains("MEMORY.md"), "{sys}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agents_md_omitted_when_over_cap() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::from("HEAD_UNIQUE_AGENTS\n");
        for i in 0..200 {
            body.push_str(&format!(
                "workspace convention number {i} must be followed by all edits.\n"
            ));
        }
        body.push_str("TAIL_UNIQUE_AGENTS\n");
        std::fs::write(dir.join("AGENTS.md"), &body).unwrap();
        let mut o = opts(&dir);
        o.agents_md = true;
        o.agents_md_max_tokens = 80;
        o.agents_md_head = false;
        let agent = Agent::new(empty_script(), o).unwrap();
        let sys = agent.messages[0].content.clone().unwrap_or_default();
        assert!(!sys.contains("HEAD_UNIQUE_AGENTS"), "{sys}");
        assert!(!sys.contains("TAIL_UNIQUE_AGENTS"), "{sys}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agents_md_head_clips_instead_of_omitting() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::from("HEAD_UNIQUE_AGENTS\n");
        for i in 0..200 {
            body.push_str(&format!(
                "workspace convention number {i} must be followed by all edits.\n"
            ));
        }
        body.push_str("TAIL_UNIQUE_AGENTS\n");
        std::fs::write(dir.join("AGENTS.md"), &body).unwrap();
        let mut o = opts(&dir);
        o.agents_md = true;
        o.agents_md_max_tokens = 80;
        o.agents_md_head = true;
        let agent = Agent::new(empty_script(), o).unwrap();
        let sys = agent.messages[0].content.clone().unwrap_or_default();
        assert!(sys.contains("HEAD_UNIQUE_AGENTS"), "{sys}");
        assert!(!sys.contains("TAIL_UNIQUE_AGENTS"), "{sys}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn memory_hot_card_on_commit_not_on_yesno() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join(".q38-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("MEMORY.md"),
            "# Prefs\n- 回复中文\n- commit: conv\n# Hosts\n- ssh = ops@192.0.2.8\n",
        )
        .unwrap();
        let mut o = opts(&dir);
        o.home = Some(home);
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("是"), turn_text("fix: foo")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        let sys = agent.messages[0].content.clone().unwrap_or_default();
        assert!(!sys.contains("MEMORY.md"));
        assert!(!sys.contains("ops@192.0.2.8"));
        agent
            .run("这个函数 off-by-one 吗？只答是或否。")
            .await
            .unwrap();
        let after_yes = hidden_texts(&agent);
        assert!(
            !after_yes.iter().any(|t| t.contains("MEMORY")),
            "{after_yes:?}"
        );
        agent.run("写一条 commit 标题").await.unwrap();
        let after_commit = hidden_texts(&agent);
        assert!(
            after_commit
                .iter()
                .any(|t| t.contains("MEMORY hot") && t.contains("回复中文")),
            "{after_commit:?}"
        );
        assert!(
            !after_commit.iter().any(|t| t.contains("192.0.2.8")),
            "{after_commit:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn testhook_skill_injects_after_failed_tool() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join(".q38-home");
        let skill_dir = home.join("skills").join("testhook");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: testhook\n---\nRerun the failing file only.\n",
        )
        .unwrap();
        let mut o = opts(&dir);
        o.home = Some(home);
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("bash", json!({"command": "echo FAILED"})),
                turn_text("ok"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        assert!(!crate::tools_schema::has_tool(&agent.tools, "skill"));
        let out = agent.run("run the tests").await.unwrap();
        assert_eq!(out.text, "ok");
        let hidden = hidden_texts(&agent);
        assert!(
            hidden.iter().any(|t| t.contains("Rerun the failing file")),
            "{hidden:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn forced_skill_replaces_an_active_skill_note() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join(".q38-home");
        for name in ["testhook", "modgen"] {
            let p = home.join("skills").join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(
                p.join("SKILL.md"),
                format!("---\nname: {name}\n---\nbody of {name}\n"),
            )
            .unwrap();
        }
        let mut o = opts(&dir);
        o.home = Some(home);
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([
                turn_tool("bash", json!({"command": "echo FAILED"})),
                turn_text("hooked"),
                turn_text("switched"),
            ])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        agent.run("run the tests").await.unwrap();
        let after_fail = hidden_texts(&agent);
        assert!(
            after_fail.iter().any(|t| t.contains("body of testhook")),
            "{after_fail:?}"
        );
        agent.run("[skill:modgen]\nemit pack").await.unwrap();
        let after = hidden_texts(&agent);
        assert!(
            after.iter().any(|t| t.contains("body of modgen")),
            "forced skill must inject immediately: {after:?}"
        );
        assert!(
            after
                .iter()
                .any(|t| t.contains("[skill: testhook]") && t.contains("applied")),
            "previous skill must stub: {after:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn mcp_mounts_when_configured_and_injects_on_mention() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(".q38")).unwrap();
        std::fs::write(
            dir.join(".q38").join("mcp.toml"),
            "[[servers]]\nname=\"docs\"\ncommand=\"python3\"\nargs=[\"x.py\"]\ndescription=\"Lantern docs\"\nmethods=[\"search\"]\n",
        )
        .unwrap();
        let o = opts(&dir);
        let scripted = Scripted {
            turns: Mutex::new(VecDeque::from([turn_text("ok-docs"), turn_text("ok-mcp")])),
            meter: false,
        };
        let mut agent = Agent::new(scripted, o).unwrap();
        assert!(crate::tools_schema::has_tool(&agent.tools, "mcp"));
        let sys = agent.messages[0].content.clone().unwrap_or_default();
        assert!(!sys.contains("mcp:"), "{sys}");
        assert!(!sys.contains("Lantern docs"), "{sys}");
        agent
            .run("Write docs/ARCHITECTURE.md then stop.")
            .await
            .unwrap();
        let after_docs = hidden_texts(&agent);
        assert!(
            !after_docs.iter().any(|t| t.contains("[mcp")),
            "{after_docs:?}"
        );
        agent
            .run("Use mcp with server docs and method search.")
            .await
            .unwrap();
        let after_mcp = hidden_texts(&agent);
        assert!(
            after_mcp.iter().any(|t| t.contains("[mcp: docs]")
                && t.contains("search")
                && !t.contains("python3")),
            "{after_mcp:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn view_dispatch_attaches_when_caps_allow() {
        let dir = std::env::temp_dir().join(format!("q38-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            crate::media::PROBE_IMAGE_B64,
        )
        .unwrap();
        std::fs::write(dir.join("red.png"), png).unwrap();

        struct Seeing {
            inner: Scripted,
            caps: crate::media::MediaCaps,
        }
        impl Completer for Seeing {
            async fn complete(
                &self,
                messages: &[ChatMessage],
                tools: Option<&[Value]>,
            ) -> Result<ModelTurn> {
                self.inner.complete(messages, tools).await
            }
            fn media_caps(&self) -> crate::media::MediaCaps {
                self.caps.clone()
            }
        }

        let mut caps = crate::media::MediaCaps::default();
        caps.image = Some(true);
        let seeing = Seeing {
            inner: Scripted {
                turns: Mutex::new(VecDeque::from([
                    turn_tool("view", json!({"path": "red.png"})),
                    turn_text("red"),
                ])),
                meter: false,
            },
            caps,
        };
        let mut agent = Agent::new(seeing, opts(&dir)).unwrap();
        assert!(crate::tools_schema::has_tool(&agent.tools, "view"));
        let sys = agent.messages[0].content.clone().unwrap_or_default();
        assert!(!sys.contains("view(path)"));
        let out = agent.run("what color is red.png").await.unwrap();
        assert_eq!(out.text, "red");
        let viewed = agent
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("view tool message");
        assert!(
            viewed.text().contains("Image loaded: red.png"),
            "{}",
            viewed.text()
        );
        assert_eq!(viewed.parts.len(), 1);
        assert!(viewed.parts[0].url.starts_with("data:image/png;base64,"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
