//! JSON-RPC types, turn snapshot, and event sink for `q38 --sidecar`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::channel::{BusyPolicy, ChannelsConfig, SteerSlot};
use crate::config::CODING_CTX_TOKENS;
use crate::family::Family;
use crate::media::MediaPart;
use crate::policy::ThinkPolicy;
use crate::session::{SessionEvent, SessionMode};
use crate::tool_calls::CancelFlag;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

pub(crate) const JSONRPC: &str = "2.0";

#[derive(Clone, Debug, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn parse(message: impl Into<String>) -> Self {
        Self {
            code: PARSE_ERROR,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: INVALID_REQUEST,
            message: message.into(),
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Dispatch {
    Result {
        result: Value,
        events: Vec<SessionEvent>,
    },
    Error(RpcError),
    /// Caller runs the agent loop, then [`SidecarSession::finish_turn`].
    TurnStart {
        prompt: String,
        parts: Vec<MediaPart>,
    },
    Abort,
    /// `/stop`: abort live turn and drop queued prompts.
    AbortClear {
        cleared: usize,
    },
}

impl Dispatch {
    pub fn turn(prompt: impl Into<String>) -> Self {
        Self::turn_parts(prompt, Vec::new())
    }

    pub fn turn_parts(prompt: impl Into<String>, parts: Vec<MediaPart>) -> Self {
        Self::TurnStart {
            prompt: prompt.into(),
            parts,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PolicyCaps {
    pub max_tokens: u32,
    pub think_mode_max_tokens: u32,
    pub max_think_low: u32,
    pub max_think_medium: u32,
    pub max_think_xhigh: u32,
}

impl Default for PolicyCaps {
    fn default() -> Self {
        let p = crate::config::PolicyConfig::default();
        Self {
            max_tokens: p.max_tokens,
            think_mode_max_tokens: p.think_mode_max_tokens,
            max_think_low: p.max_think_tokens_low,
            max_think_medium: p.max_think_tokens_medium,
            max_think_xhigh: p.max_think_tokens_xhigh,
        }
    }
}

impl PolicyCaps {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            max_tokens: cfg.policy.max_tokens,
            think_mode_max_tokens: cfg.policy.think_mode_max_tokens,
            max_think_low: cfg.policy.max_think_tokens_low,
            max_think_medium: cfg.policy.max_think_tokens_medium,
            max_think_xhigh: cfg.policy.max_think_tokens_xhigh,
        }
    }

    pub fn think_budget(&self) -> crate::policy::ThinkBudget {
        crate::policy::ThinkBudget {
            max_tokens: self.max_tokens,
            think_mode_max_tokens: self.think_mode_max_tokens,
            max_think_low: self.max_think_low,
            max_think_medium: self.max_think_medium,
            max_think_xhigh: self.max_think_xhigh,
            default_effort: crate::policy::Effort::Medium,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SidecarOpts {
    pub session_id: String,
    pub workspace: PathBuf,
    pub mode: SessionMode,
    pub policy: ThinkPolicy,
    pub caps: PolicyCaps,
    /// When false, events stay in memory (unit tests). CLI sets true.
    pub persist: bool,
    pub effort_locked: bool,
    pub model: String,
    pub family: Family,
    pub window: u32,
    pub busy: BusyPolicy,
    pub channels: ChannelsConfig,
    pub channel: String,
    pub low_precision: bool,
    /// From `config.toml` `[features].approvals`. Default Ask is only for tests.
    pub approvals: crate::permit::ApprovalMode,
}

impl Default for SidecarOpts {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            workspace: PathBuf::from("."),
            mode: SessionMode::Agent,
            policy: ThinkPolicy::agent_default(),
            caps: PolicyCaps::default(),
            persist: false,
            effort_locked: false,
            model: String::new(),
            family: Family::Qwen38,
            window: CODING_CTX_TOKENS,
            busy: BusyPolicy::Interrupt,
            channels: ChannelsConfig::default(),
            channel: "sidecar".into(),
            low_precision: false,
            approvals: crate::permit::ApprovalMode::Ask,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TurnSnapshot {
    pub session_id: String,
    pub workspace: PathBuf,
    pub mode: SessionMode,
    pub policy: ThinkPolicy,
    pub effort_locked: bool,
    pub model: String,
    pub plan_mode: bool,
    pub approvals: crate::permit::ApprovalMode,
    pub low_precision: bool,
}

pub struct TurnRequest {
    pub prompt: String,
    /// Vision / audio parts for `Agent::run_message`. Empty = text-only `run`.
    pub parts: Vec<MediaPart>,
    pub snapshot: TurnSnapshot,
    pub cancel: CancelFlag,
    pub emit: EventSink,
    pub messages: Vec<crate::template::ChatMessage>,
    pub steer: SteerSlot,
    pub persist: bool,
    /// TUI permission hub. None for `--print` / sidecar (YOLO).
    pub permit: Option<crate::permit::PermitHub>,
}

#[derive(Clone, Debug, Default)]
pub struct TurnResult {
    pub text: String,
    pub stop_reason: Option<String>,
    pub aborted: bool,
    pub error: Option<String>,
    pub events: Vec<SessionEvent>,
    pub pending_steer: Vec<String>,
    /// Agent already pushed mid-turn events through [`EventSink`].
    pub streamed: bool,
}

impl TurnResult {
    pub fn aborted() -> Self {
        Self {
            stop_reason: Some("aborted".into()),
            aborted: true,
            ..Self::default()
        }
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }
}

/// Push mid-turn JSONL events (`tool`, etc.) while `on_turn` runs.
#[derive(Clone)]
pub struct EventSink {
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<SessionEvent>,
}

impl EventSink {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self { tx }
    }

    pub fn append(&self, event: SessionEvent) {
        let _ = self.tx.send(event);
    }
}
