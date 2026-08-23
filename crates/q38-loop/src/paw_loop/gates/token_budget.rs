//! Accumulated prompt/completion tokens. QwenPaw `TokenBudgetGate`.
//! Usage is read from `GateCtx` (Python pulled a process-global wrapper).

use crate::paw_loop::store::SessionMap;
use crate::paw_loop::{GateCtx, GateDecision};

struct Acc {
    prompt_tokens: u64,
    completion_tokens: u64,
    last_iteration: i64,
}

pub struct TokenBudgetGate {
    sessions: SessionMap<Acc>,
    max_total: Option<u64>,
    max_prompt: Option<u64>,
    max_completion: Option<u64>,
}

impl TokenBudgetGate {
    pub fn new(
        max_total: Option<u64>,
        max_prompt: Option<u64>,
        max_completion: Option<u64>,
    ) -> Self {
        Self {
            sessions: SessionMap::new(),
            max_total,
            max_prompt,
            max_completion,
        }
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        self.sessions.get_or_insert_with(
            ctx.session_id,
            || Acc {
                prompt_tokens: 0,
                completion_tokens: 0,
                last_iteration: -1,
            },
            |state| {
                let iter = i64::from(ctx.iteration);
                if state.last_iteration != iter {
                    state.prompt_tokens += ctx.prompt_tokens;
                    state.completion_tokens += ctx.completion_tokens;
                    state.last_iteration = iter;
                }
                let total = state.prompt_tokens + state.completion_tokens;
                if reached(total, self.max_total)
                    || reached(state.prompt_tokens, self.max_prompt)
                    || reached(state.completion_tokens, self.max_completion)
                {
                    GateDecision::Stop {
                        reason: format!("Token budget reached ({total} tokens used)"),
                    }
                } else {
                    GateDecision::Bypass
                }
            },
        )
    }

    pub fn reset_turn(&self, session_id: &str) {
        self.sessions.insert(
            session_id,
            Acc {
                prompt_tokens: 0,
                completion_tokens: 0,
                last_iteration: -1,
            },
        );
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}

fn reached(value: u64, limit: Option<u64>) -> bool {
    limit.is_some_and(|lim| value >= lim)
}
