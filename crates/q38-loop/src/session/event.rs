use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::PolicyConfig;
use crate::error::{Error, Result};
use crate::policy::{Effort, ThinkPolicy};

/// Session product mode. `/mode` forks; depth slashes do not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    Chat,
    #[default]
    Agent,
    Think,
    Code,
}

impl SessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Agent => "agent",
            Self::Think => "think",
            Self::Code => "code",
        }
    }

    /// Default ThinkPolicy for a fresh start in this mode.
    /// Daily modes preserve=false; `/mode think` preserve=true.
    pub fn default_policy(self) -> ThinkPolicy {
        self.default_policy_with(&PolicyConfig::default())
    }

    pub fn default_policy_with(self, caps: &PolicyConfig) -> ThinkPolicy {
        self.default_policy_on(&caps.think_budget())
    }

    pub fn default_policy_on(self, b: &crate::policy::ThinkBudget) -> ThinkPolicy {
        match self {
            Self::Chat => ThinkPolicy::off_with(b),
            Self::Agent | Self::Code => ThinkPolicy::native_with(b),
            Self::Think => ThinkPolicy::think_mode_with(b),
        }
    }
}

impl fmt::Display for SessionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "chat" => Ok(Self::Chat),
            "agent" => Ok(Self::Agent),
            "think" => Ok(Self::Think),
            "code" => Ok(Self::Code),
            other => Err(Error::msg(format!("unknown mode: {other}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyReason {
    Slash,
    Upgrade,
    Downgrade,
    Watchdog,
    Cli,
}

/// OpenAI-shaped tool call stored on `assistant` events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiFunction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiFunction {
    pub name: String,
    pub arguments: String,
}

impl OpenAiToolCall {
    pub fn function(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: "function".into(),
            function: OpenAiFunction {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    /// Chat-template form: `function.arguments` is a JSON object so Qwen Jinja
    /// `arguments|items` renders `<parameter=…>` instead of iterating a string.
    pub fn to_value(&self) -> serde_json::Value {
        let arguments = serde_json::from_str(&self.function.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(self.function.arguments.clone()));
        serde_json::json!({
            "id": self.id,
            "type": self.kind,
            "function": {
                "name": self.function.name,
                "arguments": arguments,
            }
        })
    }
}

fn default_channel() -> String {
    "cli".into()
}

/// Snapshot written as `events[0]`. Not the live policy source after later `policy` events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionStart {
    pub id: String,
    pub workspace: String,
    pub mode: SessionMode,
    pub system: String,
    pub tools_hash: String,
    pub policy: ThinkPolicy,
    /// Inbound surface: `cli`, `sidecar`, `console`, or a configured IM kind.
    #[serde(default = "default_channel")]
    pub channel: String,
}

impl SessionStart {
    pub fn new(
        id: impl Into<String>,
        workspace: impl Into<String>,
        mode: SessionMode,
        system: impl Into<String>,
        tools_hash: impl Into<String>,
        policy: ThinkPolicy,
    ) -> Self {
        Self {
            id: id.into(),
            workspace: workspace.into(),
            mode,
            system: system.into(),
            tools_hash: tools_hash.into(),
            policy,
            channel: default_channel(),
        }
    }

    /// Replacement `session/start` for a `/mode` fork. Workspace is copied; policy is the new mode default.
    pub fn for_fork(
        &self,
        new_id: impl Into<String>,
        mode: SessionMode,
        system: impl Into<String>,
        tools_hash: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id.into(),
            workspace: self.workspace.clone(),
            mode,
            system: system.into(),
            tools_hash: tools_hash.into(),
            policy: mode.default_policy(),
            channel: self.channel.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserEvent {
    pub text: String,
    /// 用户消息随附的图/音/视频。serde default 保旧日志后向兼容。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<StoredMedia>,
}

fn default_true() -> bool {
    true
}

/// QwenPaw `TextContent.delta` channel. Matches `assistant.reasoning` / `content`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeltaChannel {
    Reasoning,
    Content,
}

impl DeltaChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reasoning => "reasoning",
            Self::Content => "content",
        }
    }
}

/// Ephemeral token chunk. Sidecar notification only — never JSONL / derive / compact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaEvent {
    pub channel: DeltaChannel,
    #[serde(default)]
    pub text: String,
    /// QwenPaw `TextContent.delta`. Always true on the wire for live chunks.
    #[serde(default = "default_true")]
    pub delta: bool,
    /// New model step / watchdog retry: drop the in-progress bubble (both channels).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantEvent {
    pub content: String,
    /// Thought text only. Never stuff `<think>` tags into `content`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub prompt_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub completion_tokens: u64,
    /// Prefix-cache hits from the engine (`usage.prompt_tokens_details.cached_tokens`
    /// or llama.cpp `timings.cache_n`). `None` = the endpoint did not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// llama.cpp `timings.predicted_per_second`. Weighted into the turn footer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_tok_s: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvent {
    pub tool_call_id: String,
    pub name: String,
    /// Live window text (already folded). Full dump is `blob` when set.
    pub output: String,
    /// SHA-256 of the original tool output in `~/.q38-agent/blobs/{blob}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// Character count of the original output, before fold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<StoredMedia>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMedia {
    pub kind: String,
    pub mime: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyEvent {
    #[serde(flatten)]
    pub policy: ThinkPolicy,
    pub reason: PolicyReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkEvent {
    pub from_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopEvent {
    pub reason: String,
}

/// Tombstone for `/undo`. JSONL is never rewritten; `derive_messages` skips
/// `from_seq..=until_seq` (inclusive).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoEvent {
    pub from_seq: u64,
    pub until_seq: u64,
}

/// Live-window rewrite marker. JSONL rows before this event are not deleted.
///
/// `until_seq` is the last evicted event index (inclusive). `keep_user_seq` is
/// the last real user: if it sits at or before `until_seq` (intra-turn compact),
/// `derive_messages` re-injects that user so Jinja still has a query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactEvent {
    pub until_seq: u64,
    pub keep_user_seq: u64,
    pub summary: String,
    pub index: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    #[serde(rename = "session/start")]
    Start(SessionStart),
    #[serde(rename = "user")]
    User(UserEvent),
    #[serde(rename = "assistant")]
    Assistant(AssistantEvent),
    #[serde(rename = "tool")]
    Tool(ToolEvent),
    #[serde(rename = "policy")]
    Policy(PolicyEvent),
    #[serde(rename = "session/fork")]
    Fork(ForkEvent),
    #[serde(rename = "session/compact")]
    Compact(CompactEvent),
    #[serde(rename = "stop")]
    Stop(StopEvent),
    #[serde(rename = "session/undo")]
    Undo(UndoEvent),
    #[serde(rename = "delta")]
    Delta(DeltaEvent),
}

impl SessionEvent {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User(UserEvent {
            text: text.into(),
            media: Vec::new(),
        })
    }

    /// `reasoning` is appended thought only; do not put `<think>` in `content`.
    pub fn assistant(
        content: impl Into<String>,
        reasoning: impl Into<String>,
        tool_calls: Option<Vec<OpenAiToolCall>>,
    ) -> Self {
        let tool_calls = tool_calls.filter(|c| !c.is_empty());
        Self::Assistant(AssistantEvent {
            content: content.into(),
            reasoning: reasoning.into(),
            tool_calls,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: None,
            decode_tok_s: None,
        })
    }

    pub fn assistant_usage(
        content: impl Into<String>,
        reasoning: impl Into<String>,
        tool_calls: Option<Vec<OpenAiToolCall>>,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: Option<u64>,
        decode_tok_s: Option<f64>,
    ) -> Self {
        let mut event = Self::assistant(content, reasoning, tool_calls);
        if let Self::Assistant(a) = &mut event {
            a.prompt_tokens = prompt_tokens;
            a.completion_tokens = completion_tokens;
            a.cached_tokens = cached_tokens;
            a.decode_tok_s = decode_tok_s;
        }
        event
    }

    pub fn tool(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self::tool_folded(tool_call_id, name, output, None, None)
    }

    pub fn tool_folded(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        output: impl Into<String>,
        blob: Option<String>,
        original_chars: Option<u64>,
    ) -> Self {
        Self::Tool(ToolEvent {
            tool_call_id: tool_call_id.into(),
            name: name.into(),
            output: output.into(),
            blob,
            original_chars,
            media: Vec::new(),
        })
    }

    pub fn with_media(self, media: Vec<StoredMedia>) -> Self {
        match self {
            Self::Tool(mut t) => {
                t.media = media;
                Self::Tool(t)
            }
            Self::User(mut u) => {
                u.media = media;
                Self::User(u)
            }
            other => other,
        }
    }

    pub fn policy(policy: ThinkPolicy, reason: PolicyReason) -> Self {
        Self::Policy(PolicyEvent { policy, reason })
    }

    pub fn fork(from_id: impl Into<String>) -> Self {
        Self::Fork(ForkEvent {
            from_id: from_id.into(),
        })
    }

    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop(StopEvent {
            reason: reason.into(),
        })
    }

    pub fn compact(event: CompactEvent) -> Self {
        Self::Compact(event)
    }

    pub fn undo(from_seq: u64, until_seq: u64) -> Self {
        Self::Undo(UndoEvent {
            from_seq,
            until_seq,
        })
    }

    /// Start a fresh in-progress assistant (think panel + answer bubble).
    pub fn delta_reset() -> Self {
        Self::Delta(DeltaEvent {
            channel: DeltaChannel::Content,
            text: String::new(),
            delta: true,
            reset: true,
        })
    }

    pub fn delta_chunk(channel: DeltaChannel, text: impl Into<String>) -> Self {
        Self::Delta(DeltaEvent {
            channel,
            text: text.into(),
            delta: true,
            reset: false,
        })
    }

    /// Token deltas are sidecar-only; compact / derive / JSONL must ignore them.
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, Self::Delta(_))
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Start(_) => "session/start",
            Self::User(_) => "user",
            Self::Assistant(_) => "assistant",
            Self::Tool(_) => "tool",
            Self::Policy(_) => "policy",
            Self::Fork(_) => "session/fork",
            Self::Compact(_) => "session/compact",
            Self::Stop(_) => "stop",
            Self::Undo(_) => "session/undo",
            Self::Delta(_) => "delta",
        }
    }
}

pub fn policy_for_effort(effort: Effort) -> ThinkPolicy {
    ThinkPolicy::effort_with(&PolicyConfig::default().think_budget(), effort)
}
