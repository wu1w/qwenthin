//! Composable stop handler. Behavior from QwenPaw `gates/handler.py` + `runner.py`.

use super::gates::Gate;
use super::{GateCtx, GateDecision};

/// One named handler (a mode owns one). Scope replaces Python's
/// `StopHandlerRegistration.scope` + `is_active` callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerScope {
    /// Always runs.
    Always,
    /// Default ReAct loop. Skipped when a named mode is active.
    Default,
    /// Named mode (`goal`, `mission`, …). If any such handler is active,
    /// default-scoped handlers are skipped.
    Mode(&'static str),
}

pub struct StopHandler {
    gates: Vec<Gate>,
}

impl Default for StopHandler {
    fn default() -> Self {
        Self { gates: Vec::new() }
    }
}

impl StopHandler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_gates(mut gates: Vec<Gate>) -> Self {
        gates.sort_by_key(|g| g.priority());
        Self { gates }
    }

    pub fn register(&mut self, gate: Gate) {
        self.gates.push(gate);
        self.gates.sort_by_key(|g| g.priority());
    }

    pub fn unregister(&mut self, name: &str) {
        self.gates.retain(|g| g.name() != name);
    }

    pub fn gates(&self) -> &[Gate] {
        &self.gates
    }

    pub fn reset_turn(&self, session_id: &str) {
        for gate in &self.gates {
            gate.reset_turn(session_id);
        }
    }

    /// After compact: the model may re-read files that just left the live
    /// window. That is progress, not a doom/name/path stutter.
    pub fn reset_repeat(&self, session_id: &str) {
        for gate in &self.gates {
            gate.reset_repeat(session_id);
        }
    }

    pub fn reset_session(&self, session_id: &str) {
        for gate in &self.gates {
            gate.reset_session(session_id);
        }
    }

    /// Run gates in priority order.
    ///
    /// Later `Stop` still wins over an earlier `Continue`. Among continues,
    /// the first one supplies the continuation text.
    pub fn run(&self, ctx: &GateCtx<'_>) -> GateDecision {
        if self.gates.is_empty() {
            return GateDecision::Stop {
                reason: String::new(),
            };
        }

        let mut first_continue: Option<(usize, GateDecision)> = None;

        for (i, gate) in self.gates.iter().enumerate() {
            match gate.check(ctx) {
                GateDecision::Bypass => {}
                stop @ GateDecision::Stop { .. } => return stop,
                cont @ GateDecision::Continue { .. } => {
                    if first_continue.is_none() {
                        first_continue = Some((i, cont));
                    }
                }
            }
        }

        let Some((idx, decision)) = first_continue else {
            return GateDecision::Stop {
                reason: String::new(),
            };
        };

        if let GateDecision::Continue {
            reset_peers: true, ..
        } = &decision
        {
            for (i, gate) in self.gates.iter().enumerate() {
                if i != idx {
                    gate.reset_turn(ctx.session_id);
                }
            }
        }

        let continuation = self.gates[idx].continuation(ctx.session_id);
        match decision {
            GateDecision::Continue {
                reason,
                reset_peers,
                metadata,
                ..
            } => GateDecision::Continue {
                reason,
                reset_peers,
                continuation,
                metadata,
            },
            other => other,
        }
    }
}

pub struct StopHandlerSet {
    entries: Vec<HandlerEntry>,
}

struct HandlerEntry {
    name: String,
    scope: HandlerScope,
    handler: StopHandler,
    active: bool,
    priority: i32,
}

impl Default for StopHandlerSet {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl StopHandlerSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, name: impl Into<String>, scope: HandlerScope, handler: StopHandler) {
        self.push_with_priority(name, scope, 100, handler);
    }

    pub fn push_with_priority(
        &mut self,
        name: impl Into<String>,
        scope: HandlerScope,
        priority: i32,
        handler: StopHandler,
    ) {
        self.entries.push(HandlerEntry {
            name: name.into(),
            scope,
            handler,
            active: matches!(scope, HandlerScope::Always | HandlerScope::Default),
            priority,
        });
        self.entries.sort_by_key(|e| e.priority);
    }

    pub fn set_active(&mut self, name: &str, active: bool) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.name == name) {
            e.active = active;
        }
    }

    pub fn run(&self, ctx: &GateCtx<'_>) -> GateDecision {
        for entry in self.filtered() {
            match entry.handler.run(ctx) {
                GateDecision::Bypass => {}
                decided => return decided,
            }
        }
        GateDecision::Stop {
            reason: String::new(),
        }
    }

    fn filtered(&self) -> Vec<&HandlerEntry> {
        let active_mode = self.entries.iter().find_map(|e| match (e.scope, e.active) {
            (HandlerScope::Mode(name), true) => Some(name),
            _ => None,
        });

        self.entries
            .iter()
            .filter(|e| match e.scope {
                HandlerScope::Always => true,
                HandlerScope::Mode(name) => e.active && active_mode == Some(name),
                HandlerScope::Default => active_mode.is_none() && e.active,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paw_loop::{Gate, GateCtx, IterationGate};

    #[test]
    fn handler_collapses_gate_bypass_to_empty_stop() {
        let h = StopHandler::with_gates(vec![Gate::from(IterationGate::new(20))]);
        h.reset_turn("s");
        let ctx = GateCtx::new("s");
        match h.run(&ctx) {
            GateDecision::Bypass => panic!("handler must not return Bypass"),
            GateDecision::Stop { reason } => assert!(reason.is_empty(), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reset_repeat_clears_doom_so_third_after_reset_does_not_halt() {
        use crate::paw_loop::{DoomLoopGate, ToolFingerprint};
        let h = StopHandler::with_gates(vec![Gate::from(DoomLoopGate::qwen_default())]);
        h.reset_turn("s");
        let a = ToolFingerprint::new("read", r#"{"path":"a.rs"}"#);
        for i in 1..=2 {
            let fps = [a.clone()];
            let mut ctx = GateCtx::new("s");
            ctx.iteration = i;
            ctx.fingerprints = &fps;
            ctx.last_tool = fps.last();
            match h.run(&ctx) {
                GateDecision::Stop { reason } if reason.contains("Doom") => {
                    panic!("too early: {reason}")
                }
                _ => {}
            }
        }
        h.reset_repeat("s");
        let fps = [a];
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 3;
        ctx.fingerprints = &fps;
        ctx.last_tool = fps.last();
        match h.run(&ctx) {
            GateDecision::Stop { reason } if reason.contains("Doom") => {
                panic!("compact reset must clear doom: {reason}")
            }
            _ => {}
        }
    }

    #[test]
    fn reset_repeat_does_not_reset_iteration_budget() {
        let h = StopHandler::with_gates(vec![Gate::from(IterationGate::new(3))]);
        h.reset_turn("s");
        for i in 1..=2 {
            let mut ctx = GateCtx::new("s");
            ctx.iteration = i;
            let _ = h.run(&ctx);
        }
        h.reset_repeat("s");
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 3;
        match h.run(&ctx) {
            GateDecision::Stop { reason } => {
                assert!(reason.contains("Max iterations"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }
}
