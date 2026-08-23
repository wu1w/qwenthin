//! Grok-shaped permission + plan overlays, without extra OpenAI tools.
//!
//! `tools[]` stays frozen. Plan is a hidden-user card + mutating-tool deny.
//! Ask/auto is a TUI oneshot in front of write/edit/bash/run_code/mcp.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::tool_calls::CancelFlag;

// 文案必须与 is_mutating 的放行面一致：只读工具全部可用。
pub const PLAN_CARD: &str = "\
PLAN MODE (read-only). Allowed: read, view, search, web, recall. Do not call \
write, edit, bash, run_code, or mcp. Inspect the repo and write a markdown \
plan: files to change, steps, risks. Do not implement yet.";

pub const PLAN_IMPLEMENT: &str = "Implement the approved plan.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Prompt on mutating tools (TUI). `--print` has no prompt and stays YOLO.
    #[default]
    Ask,
    /// Workspace edits pass; bash / run_code / mcp still prompt.
    Auto,
    /// Never prompt.
    Yolo,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" | "default" | "on" => Some(Self::Ask),
            "auto" | "acceptedits" | "edits" => Some(Self::Auto),
            "yolo" | "bypass" | "off" | "bypasspermissions" => Some(Self::Yolo),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanAction {
    On,
    Off,
    Go,
}

#[derive(Clone, Debug)]
pub struct PermitAsk {
    pub tool: String,
    pub preview: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermitDecision {
    Allow,
    Always,
    Deny,
}

pub struct PermitRequest {
    pub ask: PermitAsk,
    pub reply: oneshot::Sender<PermitDecision>,
}

/// TUI owns the receiver. Agent clones the hub and `check()`s before mutating tools.
#[derive(Clone)]
pub struct PermitHub {
    tx: mpsc::UnboundedSender<PermitRequest>,
    mode: Arc<Mutex<ApprovalMode>>,
    always: Arc<Mutex<HashSet<String>>>,
}

impl fmt::Debug for PermitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermitHub")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl PermitHub {
    pub fn pair(mode: ApprovalMode) -> (Self, mpsc::UnboundedReceiver<PermitRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                mode: Arc::new(Mutex::new(mode)),
                always: Arc::new(Mutex::new(HashSet::new())),
            },
            rx,
        )
    }

    pub fn set_mode(&self, mode: ApprovalMode) {
        if let Ok(mut g) = self.mode.lock() {
            *g = mode;
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        self.mode.lock().map(|g| *g).unwrap_or(ApprovalMode::Ask)
    }

    pub fn remember(&self, tool: &str) {
        if let Ok(mut g) = self.always.lock() {
            g.insert(tool.to_string());
        }
    }

    pub fn needs_prompt(mode: ApprovalMode, tool: &str) -> bool {
        if !is_mutating(tool) {
            return false;
        }
        match mode {
            ApprovalMode::Yolo => false,
            ApprovalMode::Ask => true,
            ApprovalMode::Auto => !matches!(tool, "write" | "edit"),
        }
    }

    pub async fn check(&self, tool: &str, preview: &str, cancel: &CancelFlag) -> PermitDecision {
        if !Self::needs_prompt(self.mode(), tool) {
            return PermitDecision::Allow;
        }
        if self
            .always
            .lock()
            .map(|g| g.contains(tool))
            .unwrap_or(false)
        {
            return PermitDecision::Allow;
        }
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(PermitRequest {
                ask: PermitAsk {
                    tool: tool.to_string(),
                    preview: preview.to_string(),
                },
                reply,
            })
            .is_err()
        {
            return PermitDecision::Deny;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => PermitDecision::Deny,
            dec = rx => dec.unwrap_or(PermitDecision::Deny),
        }
    }
}

pub fn is_mutating(tool: &str) -> bool {
    matches!(tool, "write" | "edit" | "bash" | "run_code" | "mcp")
}

// 失败文案契约：ToolState 不落盘，`observed_from_messages` 靠 "Error:" 前缀
// 判失败重建盲覆写守卫。所有非 Success 文案必须以 "Error:" 开头。
pub fn plan_denied(tool: &str) -> String {
    format!(
        "Error: plan mode: `{tool}` blocked. Stay read-only and put the plan in your reply. \
         The user will /plan go when they want you to implement."
    )
}

pub fn user_denied(tool: &str) -> String {
    format!("Error: User denied `{tool}`. Continue without that call, or ask a different way.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_allows_edits_asks_bash() {
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "write"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Auto, "edit"));
        assert!(PermitHub::needs_prompt(ApprovalMode::Auto, "bash"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Ask, "read"));
        assert!(PermitHub::needs_prompt(ApprovalMode::Ask, "write"));
        assert!(!PermitHub::needs_prompt(ApprovalMode::Yolo, "bash"));
    }
}
