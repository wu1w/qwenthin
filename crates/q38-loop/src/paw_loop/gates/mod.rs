mod budget;
mod doom;
mod iteration;
mod lossy;
mod timeout;
mod token_budget;
mod tool_budget;

pub use budget::BudgetGate;
pub use doom::{DoomLoopGate, DoomStage, REPEAT_NOTE};
pub use iteration::IterationGate;
pub use lossy::{fs_tool_path, NameStreakGate, PathLoopGate, NAME_NOTE, PATH_NOTE};
pub use timeout::TimeoutGate;
pub use token_budget::TokenBudgetGate;
pub use tool_budget::{ToolCallBudgetGate, LOSSY_TOOL_BUDGET};

use super::{GateCtx, GateDecision};

/// Closed set of stop gates. Dispatch is exhaustive — no `dyn StopGate`.
pub enum Gate {
    Iteration(IterationGate),
    Budget(BudgetGate),
    DoomLoop(DoomLoopGate),
    Timeout(TimeoutGate),
    TokenBudget(TokenBudgetGate),
    ToolCallBudget(ToolCallBudgetGate),
    NameStreak(NameStreakGate),
    PathLoop(PathLoopGate),
}

impl Gate {
    pub fn iteration(max: u32) -> Self {
        Self::Iteration(IterationGate::new(max))
    }

    pub fn budget(max_tokens: u64) -> Self {
        Self::Budget(BudgetGate::new(max_tokens))
    }

    pub fn doom_loop(window_size: usize, threshold: f64, stages: Vec<DoomStage>) -> Self {
        Self::DoomLoop(DoomLoopGate::new(window_size, threshold, stages))
    }

    pub fn timeout(max: std::time::Duration) -> Self {
        Self::Timeout(TimeoutGate::new(max))
    }

    pub fn token_budget(
        max_total: Option<u64>,
        max_prompt: Option<u64>,
        max_completion: Option<u64>,
    ) -> Self {
        Self::TokenBudget(TokenBudgetGate::new(max_total, max_prompt, max_completion))
    }

    pub fn tool_call_budget(
        max_calls: Option<u32>,
        per_tool: std::collections::HashMap<String, u32>,
    ) -> Self {
        Self::ToolCallBudget(ToolCallBudgetGate::new(max_calls, per_tool))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Iteration(_) => "iteration",
            Self::Budget(_) => "budget",
            Self::DoomLoop(_) => "doom-loop",
            Self::Timeout(_) => "timeout",
            Self::TokenBudget(_) => "token-budget",
            Self::ToolCallBudget(_) => "tool-call-budget",
            Self::NameStreak(_) => "name-streak",
            Self::PathLoop(_) => "path-loop",
        }
    }

    pub fn priority(&self) -> i32 {
        match self {
            Self::DoomLoop(_) => 5,
            Self::NameStreak(_) => 6,
            Self::PathLoop(_) => 7,
            Self::Iteration(_) => 10,
            Self::Budget(_) | Self::TokenBudget(_) => 20,
            Self::Timeout(_) => 30,
            Self::ToolCallBudget(_) => 40,
        }
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        match self {
            Self::Iteration(g) => g.check(ctx),
            Self::Budget(g) => g.check(ctx),
            Self::DoomLoop(g) => g.check(ctx),
            Self::Timeout(g) => g.check(ctx),
            Self::TokenBudget(g) => g.check(ctx),
            Self::ToolCallBudget(g) => g.check(ctx),
            Self::NameStreak(g) => g.check(ctx),
            Self::PathLoop(g) => g.check(ctx),
        }
    }

    pub fn continuation(&self, session_id: &str) -> String {
        match self {
            Self::DoomLoop(g) => g.continuation(session_id),
            Self::NameStreak(g) => g.continuation(session_id),
            Self::PathLoop(g) => g.continuation(session_id),
            _ => String::new(),
        }
    }

    pub fn reset_turn(&self, session_id: &str) {
        match self {
            Self::Iteration(g) => g.reset_turn(session_id),
            Self::Budget(_) => {}
            Self::DoomLoop(g) => g.reset_turn(session_id),
            Self::Timeout(g) => g.reset_turn(session_id),
            Self::TokenBudget(g) => g.reset_turn(session_id),
            Self::ToolCallBudget(g) => g.reset_turn(session_id),
            Self::NameStreak(g) => g.reset_turn(session_id),
            Self::PathLoop(g) => g.reset_turn(session_id),
        }
    }

    /// Compact / other real progress: clear stutter detectors only.
    /// Must not reset iteration, timeout, or token budgets.
    pub fn reset_repeat(&self, session_id: &str) {
        match self {
            Self::DoomLoop(g) => g.reset_turn(session_id),
            Self::NameStreak(g) => g.reset_turn(session_id),
            Self::PathLoop(g) => g.reset_turn(session_id),
            _ => {}
        }
    }

    pub fn reset_session(&self, session_id: &str) {
        match self {
            Self::Iteration(g) => g.reset_session(session_id),
            Self::Budget(g) => g.reset_session(session_id),
            Self::DoomLoop(g) => g.reset_session(session_id),
            Self::Timeout(g) => g.reset_session(session_id),
            Self::TokenBudget(g) => g.reset_session(session_id),
            Self::ToolCallBudget(g) => g.reset_session(session_id),
            Self::NameStreak(g) => g.reset_session(session_id),
            Self::PathLoop(g) => g.reset_session(session_id),
        }
    }
}

impl From<IterationGate> for Gate {
    fn from(g: IterationGate) -> Self {
        Self::Iteration(g)
    }
}
impl From<BudgetGate> for Gate {
    fn from(g: BudgetGate) -> Self {
        Self::Budget(g)
    }
}
impl From<DoomLoopGate> for Gate {
    fn from(g: DoomLoopGate) -> Self {
        Self::DoomLoop(g)
    }
}
impl From<TimeoutGate> for Gate {
    fn from(g: TimeoutGate) -> Self {
        Self::Timeout(g)
    }
}
impl From<TokenBudgetGate> for Gate {
    fn from(g: TokenBudgetGate) -> Self {
        Self::TokenBudget(g)
    }
}
impl From<ToolCallBudgetGate> for Gate {
    fn from(g: ToolCallBudgetGate) -> Self {
        Self::ToolCallBudget(g)
    }
}
impl From<NameStreakGate> for Gate {
    fn from(g: NameStreakGate) -> Self {
        Self::NameStreak(g)
    }
}
impl From<PathLoopGate> for Gate {
    fn from(g: PathLoopGate) -> Self {
        Self::PathLoop(g)
    }
}
