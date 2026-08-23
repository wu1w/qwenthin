//! QwenPaw loop engineering, rewritten for Rust.
//!
//! Python source: `third_party/QwenPaw/src/qwenpaw/loop/`
//! Behavior kept: priority-ordered gates, TERMINATE wins over CONTINUE,
//! first CONTINUE is the continuation source, `reset_peers` clears other
//! gates' turn state.
//!
//! Design changes vs Python:
//! - `GateCtx` is a typed struct (no `dict` / `getattr`)
//! - `Gate` is an enum (no ABC / `dyn` / boxed futures)
//! - `check` is synchronous (Python gates were async only for AgentScope)
//! - session id is an argument, not a contextvar

mod ctx;
mod gates;
mod handler;
mod store;

pub use ctx::{hash_args, GateCtx, ToolFingerprint};
pub use gates::{
    fs_tool_path, BudgetGate, DoomLoopGate, DoomStage, Gate, IterationGate, NameStreakGate,
    PathLoopGate, TimeoutGate, TokenBudgetGate, ToolCallBudgetGate, LOSSY_TOOL_BUDGET, NAME_NOTE,
    PATH_NOTE, REPEAT_NOTE,
};
pub use handler::{HandlerScope, StopHandler, StopHandlerSet};
pub use store::SessionMap;

use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopAction {
    Bypass,
    Continue,
    Stop,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GateDecision {
    Bypass,
    Continue {
        reason: String,
        reset_peers: bool,
        continuation: String,
        metadata: Option<Map<String, Value>>,
    },
    Stop {
        reason: String,
    },
}

impl GateDecision {
    pub fn action(&self) -> StopAction {
        match self {
            Self::Bypass => StopAction::Bypass,
            Self::Continue { .. } => StopAction::Continue,
            Self::Stop { .. } => StopAction::Stop,
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Bypass => "",
            Self::Continue { reason, .. } | Self::Stop { reason } => reason,
        }
    }
}

impl Default for GateDecision {
    fn default() -> Self {
        Self::Stop {
            reason: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ctx(session: &str) -> GateCtx<'_> {
        GateCtx::new(session)
    }

    #[test]
    fn empty_handler_stops() {
        let handler = StopHandler::new();
        match handler.run(&ctx("s")) {
            GateDecision::Stop { reason } => assert!(reason.is_empty()),
            other => panic!("expected stop, got {other:?}"),
        }
    }

    #[test]
    fn iteration_gate_bypasses_until_cap() {
        let gate = IterationGate::new(3);
        gate.activate("s", None);
        let ctx = ctx("s");
        assert!(matches!(gate.check(&ctx), GateDecision::Bypass));
        assert!(matches!(gate.check(&ctx), GateDecision::Bypass));
        match gate.check(&ctx) {
            GateDecision::Stop { reason } => {
                assert!(reason.contains("Max iterations (3)"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unactivated_gates_all_bypass_become_stop() {
        let handler = StopHandler::with_gates(vec![Gate::iteration(2)]);
        match handler.run(&ctx("s")) {
            GateDecision::Stop { reason } => assert!(reason.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn later_stop_wins_over_continue() {
        let doom = DoomLoopGate::new(
            2,
            1.0,
            vec![DoomStage {
                after: 2,
                stop: false,
                prompt: "slow down".into(),
            }],
        );
        let iter = IterationGate::new(3);
        iter.activate("s", None);
        let handler = StopHandler::with_gates(vec![Gate::from(doom), Gate::from(iter)]);
        let tool = ToolFingerprint::new("bash", r#"{"command":"ls"}"#);
        let mut ctx = GateCtx::new("s");
        ctx.last_tool = Some(&tool);

        ctx.iteration = 1;
        match handler.run(&ctx) {
            GateDecision::Stop { reason } => assert!(reason.is_empty()),
            other => panic!("iter 1: {other:?}"),
        }
        ctx.iteration = 2;
        match handler.run(&ctx) {
            GateDecision::Continue { continuation, .. } => {
                assert_eq!(continuation, "slow down");
            }
            other => panic!("iter 2: {other:?}"),
        }
        ctx.iteration = 3;
        match handler.run(&ctx) {
            GateDecision::Stop { reason } => assert!(reason.contains("Max iterations")),
            other => panic!("iter 3: {other:?}"),
        }
    }

    #[test]
    fn doom_continues_with_stage_prompt() {
        let doom = DoomLoopGate::new(
            3,
            1.0,
            vec![DoomStage {
                after: 3,
                stop: false,
                prompt: "you are repeating tools".into(),
            }],
        );
        let handler = StopHandler::with_gates(vec![Gate::from(doom)]);
        let tool = ToolFingerprint::new("read", r#"{"path":"a.rs"}"#);
        let mut ctx = GateCtx::new("s");
        ctx.last_tool = Some(&tool);

        for i in 1..=2 {
            ctx.iteration = i;
            match handler.run(&ctx) {
                GateDecision::Stop { reason } => assert!(reason.is_empty()),
                other => panic!("iter {i}: {other:?}"),
            }
        }
        ctx.iteration = 3;
        match handler.run(&ctx) {
            GateDecision::Continue { continuation, .. } => {
                assert_eq!(continuation, "you are repeating tools");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn budget_reads_tokens_from_ctx() {
        let gate = BudgetGate::new(100);
        gate.activate("s", None);
        let mut ctx = GateCtx::new("s");
        ctx.tokens_used = 50;
        assert!(matches!(gate.check(&ctx), GateDecision::Bypass));
        ctx.tokens_used = 100;
        match gate.check(&ctx) {
            GateDecision::Stop { reason } => assert_eq!(reason, "Token budget exceeded"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn timeout_zero_stops() {
        let handler = StopHandler::with_gates(vec![Gate::timeout(Duration::ZERO)]);
        match handler.run(&ctx("s")) {
            GateDecision::Stop { reason } => assert!(reason.contains("time limit")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn token_budget_accumulates_per_iteration() {
        let gate = TokenBudgetGate::new(Some(10), None, None);
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 1;
        ctx.prompt_tokens = 4;
        ctx.completion_tokens = 4;
        assert!(matches!(gate.check(&ctx), GateDecision::Bypass));
        ctx.iteration = 2;
        ctx.prompt_tokens = 2;
        ctx.completion_tokens = 1;
        match gate.check(&ctx) {
            GateDecision::Stop { reason } => assert!(reason.contains("Token budget reached")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_call_budget_counts_names() {
        let handler =
            StopHandler::with_gates(vec![Gate::tool_call_budget(Some(2), Default::default())]);
        let names = vec!["read".into(), "bash".into()];
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 1;
        ctx.tool_names = &names;
        match handler.run(&ctx) {
            GateDecision::Stop { reason } => assert!(reason.contains("Tool call budget")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn named_mode_skips_default() {
        let def_gate = IterationGate::new(1);
        def_gate.activate("s", None);
        let mode_gate = BudgetGate::new(1);
        mode_gate.activate("s", Some(1));

        let mut set = StopHandlerSet::new();
        set.push(
            "default",
            HandlerScope::Default,
            StopHandler::with_gates(vec![Gate::from(def_gate)]),
        );
        set.push(
            "goal",
            HandlerScope::Mode("goal"),
            StopHandler::with_gates(vec![Gate::from(mode_gate)]),
        );
        set.set_active("goal", true);

        let mut ctx = GateCtx::new("s");
        ctx.tokens_used = 1;
        match set.run(&ctx) {
            GateDecision::Stop { reason } => assert_eq!(reason, "Token budget exceeded"),
            other => panic!("expected mode budget stop, got {other:?}"),
        }
    }
}
