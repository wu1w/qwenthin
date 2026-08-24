//! q38-loop: QwenPaw-shaped harness core + Qwen3.8 family adapters.
//!
//! Loop / schemas / tool trajectory are a Rust rewrite of QwenPaw behavior
//! (see `paw_loop`, `schemas`, `tool_calls`). Family request builders and
//! probe remain original q-harness code.

pub mod adapter;
pub mod agent;
pub mod channel;
pub mod clarify;
pub mod config;
pub mod cron;
pub mod echo;
pub mod error;
pub mod family;
pub mod mcp;
pub mod media;
pub mod memory;
pub mod paw_loop;
pub mod permit;
pub mod policy;
pub mod prefix_cache;
pub mod probe;
pub mod prompt;
pub mod schemas;
pub mod session;
pub mod sidecar;
pub mod skills;
pub mod slash;
pub mod sticky;
pub mod stutter;
pub mod template;
pub mod tokenize;
pub mod tool_calls;
pub mod tools;
pub mod tools_schema;
pub mod vendor;

#[cfg(test)]
mod live;
#[cfg(test)]
mod live_agent_tour;
#[cfg(test)]
mod live_identity;
#[cfg(test)]
mod live_memory_recall;
#[cfg(test)]
mod live_mix;
#[cfg(test)]
mod live_native_think;
#[cfg(test)]
mod live_scenes;

pub use agent::{Agent, AgentOutcome, HttpCompleter, RunOpts, ToolSet};

/// Recover a [`Mutex`] after a previous holder panicked.
///
/// Loop-critical locks (coordinator `live` / `hooks` / `per_agent`, mailbox
/// steer, gate `SessionMap`) use this so one panicking thread cannot wedge
/// later `lock()` calls. Inner data is still the last consistent value.
/// Non-critical stores (history FTS) may instead return a `Result` error.
pub fn lock_unpoison<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub use channel::{
    run_channels, BusyPolicy, ChannelEndpoint, ChannelsConfig, ContentPart, Mailbox, NativePayload,
    SessionRouter,
};
pub use config::CODING_CTX_TOKENS;
pub use error::{Error, Result};
pub use family::{EndpointCaps, EngineProfile, Family};
pub use media::{MediaBins, MediaCaps, MediaKind, MediaPart};
pub use paw_loop::{
    Gate, GateCtx, GateDecision, HandlerScope, StopHandler, StopHandlerSet, ToolFingerprint,
};
pub use clarify::{ClarifyAsk, ClarifyDecision, ClarifyHub, ClarifyRequest};
pub use permit::{ApprovalMode, PermitAsk, PermitDecision, PermitHub, PlanAction, PLAN_IMPLEMENT};
pub use policy::{Effort, Sampling, ThinkBudget, ThinkPolicy, XHIGH_WARN};
pub use probe::{run_probe, ProbeReport};
pub use prompt::{coding_prompt, session_prompt, CODING_SYSTEM_PROMPT, DEFAULT_AGENT_MD};
pub use session::{
    derive_messages, live_policy, new_session_id, parse_slash, policy_for_effort, tools_hash,
    CompactEvent, DeltaChannel, DeltaEvent, Hit, OpenAiToolCall, PolicyReason, SessionEvent,
    SessionLog, SessionMode, SessionStart, SlashCmd, UndoEvent,
};
pub use slash::{parse_slash_with_skills, UsageRecap};
pub use template::{
    is_hidden_user_text, wrap_tool_response, ChatMessage, RenderOpts, RenderedPrompt,
};
pub use tool_calls::{CancelFlag, ToolCall, ToolCoordinator, ToolResponse, ToolState};
pub use tools::{BlobStore, Workspace};
