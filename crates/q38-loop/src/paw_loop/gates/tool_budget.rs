//! Per-turn tool-call counts. QwenPaw `ToolCallBudgetGate`.

/// Lossy overlay only. Q8 / high-precision still relies on `max_steps`.
pub const LOSSY_TOOL_BUDGET: u32 = 48;

use std::collections::HashMap;

use crate::paw_loop::store::SessionMap;
use crate::paw_loop::{GateCtx, GateDecision};

struct Acc {
    total: u32,
    by_tool: HashMap<String, u32>,
    last_iteration: i64,
}

pub struct ToolCallBudgetGate {
    sessions: SessionMap<Acc>,
    max_calls: Option<u32>,
    per_tool: HashMap<String, u32>,
}

impl ToolCallBudgetGate {
    pub fn new(max_calls: Option<u32>, per_tool: HashMap<String, u32>) -> Self {
        Self {
            sessions: SessionMap::new(),
            max_calls,
            per_tool,
        }
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        self.sessions.get_or_insert_with(
            ctx.session_id,
            || Acc {
                total: 0,
                by_tool: HashMap::new(),
                last_iteration: -1,
            },
            |state| {
                let iter = i64::from(ctx.iteration);
                if iter != state.last_iteration {
                    state.total += ctx.tool_names.len() as u32;
                    for name in ctx.tool_names {
                        *state.by_tool.entry(name.clone()).or_insert(0) += 1;
                    }
                    state.last_iteration = iter;
                }
                if let Some(max) = self.max_calls {
                    if state.total >= max {
                        return GateDecision::Stop {
                            reason: format!("Tool call budget reached ({} calls)", state.total),
                        };
                    }
                }
                for (name, limit) in &self.per_tool {
                    if state.by_tool.get(name).copied().unwrap_or(0) >= *limit {
                        return GateDecision::Stop {
                            reason: format!("Tool '{name}' call budget reached ({limit})"),
                        };
                    }
                }
                GateDecision::Bypass
            },
        )
    }

    pub fn reset_turn(&self, session_id: &str) {
        self.sessions.insert(
            session_id,
            Acc {
                total: 0,
                by_tool: HashMap::new(),
                last_iteration: -1,
            },
        );
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}
