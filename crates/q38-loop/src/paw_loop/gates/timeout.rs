//! Wall-clock cap. QwenPaw `gates/limits.py` TimeoutGate.

use std::time::{Duration, Instant};

use crate::paw_loop::store::SessionMap;
use crate::paw_loop::{GateCtx, GateDecision};

pub struct TimeoutGate {
    sessions: SessionMap<Instant>,
    max: Duration,
}

impl TimeoutGate {
    pub fn new(max: Duration) -> Self {
        Self {
            sessions: SessionMap::new(),
            max,
        }
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        self.sessions
            .get_or_insert_with(ctx.session_id, Instant::now, |started| {
                if started.elapsed() < self.max {
                    GateDecision::Bypass
                } else {
                    GateDecision::Stop {
                        reason: format!("Loop time limit reached ({}s)", self.max.as_secs_f64()),
                    }
                }
            })
    }

    pub fn reset_turn(&self, session_id: &str) {
        self.sessions.insert(session_id, Instant::now());
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}
