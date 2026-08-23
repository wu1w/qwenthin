//! Iteration cap. QwenPaw `gates/iteration.py`.

use crate::paw_loop::store::SessionMap;
use crate::paw_loop::{GateCtx, GateDecision};

struct IterState {
    iteration: u32,
    max_iterations: u32,
}

pub struct IterationGate {
    sessions: SessionMap<IterState>,
    default_max: u32,
}

impl IterationGate {
    pub fn new(max_iterations: u32) -> Self {
        Self {
            sessions: SessionMap::new(),
            default_max: max_iterations.max(1),
        }
    }

    pub fn activate(&self, session_id: &str, max_iterations: Option<u32>) {
        let limit = max_iterations.unwrap_or(self.default_max);
        self.sessions.insert(
            session_id,
            IterState {
                iteration: 0,
                max_iterations: limit,
            },
        );
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        let decision = self.sessions.modify(ctx.session_id, |state| {
            let Some(state) = state else {
                return GateDecision::Bypass;
            };
            state.iteration = state.iteration.saturating_add(1);
            if state.iteration >= state.max_iterations {
                GateDecision::Stop {
                    reason: format!("Max iterations ({}) reached", state.max_iterations),
                }
            } else {
                GateDecision::Bypass
            }
        });
        // Keep the cap armed so a wrap-up hop still sees it. The next user
        // turn calls `reset_turn`, which zeroes the counter.
        decision
    }

    pub fn reset_turn(&self, session_id: &str) {
        let missing = self.sessions.modify(session_id, |state| match state {
            Some(state) => {
                state.iteration = 0;
                false
            }
            None => true,
        });
        if missing {
            self.activate(session_id, None);
        }
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}

impl Default for IterationGate {
    fn default() -> Self {
        Self::new(20)
    }
}
