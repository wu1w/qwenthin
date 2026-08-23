//! Token-usage cap. QwenPaw `gates/budget.py`. Usage comes from `GateCtx`.

use crate::paw_loop::store::SessionMap;
use crate::paw_loop::{GateCtx, GateDecision};

struct BudgetState {
    max_tokens: u64,
}

pub struct BudgetGate {
    sessions: SessionMap<BudgetState>,
    default_max: u64,
}

impl BudgetGate {
    pub fn new(max_tokens: u64) -> Self {
        Self {
            sessions: SessionMap::new(),
            default_max: max_tokens,
        }
    }

    pub fn activate(&self, session_id: &str, max_tokens: Option<u64>) {
        self.sessions.insert(
            session_id,
            BudgetState {
                max_tokens: max_tokens.unwrap_or(self.default_max),
            },
        );
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        let (decision, drop) = self.sessions.modify(ctx.session_id, |state| {
            let Some(state) = state else {
                return (GateDecision::Bypass, false);
            };
            if ctx.tokens_used >= state.max_tokens {
                (
                    GateDecision::Stop {
                        reason: "Token budget exceeded".into(),
                    },
                    true,
                )
            } else {
                (GateDecision::Bypass, false)
            }
        });
        if drop {
            self.sessions.remove(ctx.session_id);
        }
        decision
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}

impl Default for BudgetGate {
    fn default() -> Self {
        Self::new(300_000)
    }
}
