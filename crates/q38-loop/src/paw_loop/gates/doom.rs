//! Sliding-window tool-call repetition detector. QwenPaw `gates/doom_loop.py`.

use std::collections::{HashSet, VecDeque};

use crate::paw_loop::store::SessionMap;
use crate::paw_loop::{GateCtx, GateDecision, ToolFingerprint};

/// A low-information trajectory observation, not an order. It is intentionally
/// delayed: polling, flaky processes and changing files can make identical
/// calls useful for a few rounds.
pub const REPEAT_NOTE: &str =
    "[trajectory] 同一工具和参数已连续出现三次，信息增量可能已经很低。检查上一结果；需要新证据就换目标或参数，否则按当前证据继续或收尾。";

#[derive(Clone, Debug)]
pub struct DoomStage {
    pub after: u32,
    pub stop: bool,
    pub prompt: String,
}

impl DoomStage {
    pub fn warn(after: u32, prompt: impl Into<String>) -> Self {
        Self {
            after,
            stop: false,
            prompt: prompt.into(),
        }
    }

    pub fn halt(after: u32, prompt: impl Into<String>) -> Self {
        Self {
            after,
            stop: true,
            prompt: prompt.into(),
        }
    }
}

struct DoomState {
    history: VecDeque<ToolFingerprint>,
    consecutive_hits: u32,
    prompt: String,
    last_recorded_iter: i64,
}

pub struct DoomLoopGate {
    sessions: SessionMap<DoomState>,
    window_size: usize,
    threshold: f64,
    stages: Vec<DoomStage>,
}

impl DoomLoopGate {
    pub fn new(window_size: usize, threshold: f64, mut stages: Vec<DoomStage>) -> Self {
        stages.sort_by_key(|s| s.after);
        Self {
            sessions: SessionMap::new(),
            window_size: window_size.max(2),
            threshold,
            stages,
        }
    }

    /// Let the model lead. A third identical call gets one factual nudge; only
    /// six consecutive identical calls count as a genuine no-progress loop.
    /// Stateful shell calls get one additional round before the hard stop.
    pub fn qwen_default() -> Self {
        Self::new(
            2,
            1.0,
            vec![
                DoomStage::warn(3, REPEAT_NOTE),
                DoomStage::halt(
                    6,
                    "Doom loop: six identical tool calls without a course change",
                ),
            ],
        )
    }

    /// Low-precision overlay: window 2, halt@2. Still no Continue lecture.
    pub fn lossy() -> Self {
        Self::new(
            2,
            1.0,
            vec![DoomStage::halt(2, "Doom loop: repeated the same tool")],
        )
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        self.sessions.get_or_insert_with(
            ctx.session_id,
            || DoomState {
                history: VecDeque::with_capacity(self.window_size * 2),
                consecutive_hits: 0,
                prompt: String::new(),
                last_recorded_iter: -1,
            },
            |state| {
                if ctx.fingerprints.is_empty() && ctx.last_tool.is_none() {
                    // A text reply is progress. Do not count it as another
                    // repetition of the previous tool window.
                    state.history.clear();
                    state.consecutive_hits = 0;
                    state.prompt.clear();
                    state.last_recorded_iter = -1;
                    return GateDecision::Bypass;
                }
                let iter = i64::from(ctx.iteration);
                if iter > state.last_recorded_iter {
                    state.last_recorded_iter = iter;
                    // Record every call in the hop. Using only `last_tool`
                    // made a parallel `read a, read b, read c` look like
                    // "the same tool" three hops in a row (the last path).
                    if ctx.fingerprints.is_empty() {
                        if let Some(fp) = ctx.last_tool {
                            push_fp(&mut state.history, fp, self.window_size);
                        }
                    } else {
                        for fp in ctx.fingerprints {
                            push_fp(&mut state.history, fp, self.window_size);
                        }
                    }
                }

                if !self.is_repeating(state) {
                    state.consecutive_hits = 0;
                    state.prompt.clear();
                    return GateDecision::Bypass;
                }

                if state.consecutive_hits == 0 {
                    state.consecutive_hits = self.window_size as u32;
                } else {
                    state.consecutive_hits += 1;
                }

                // bash 是有状态命令（git status、tail 日志），同参重放结果
                // 可变：只把 hard halt 再放宽一步，soft note 阈值不变。
                let stateful = state.history.back().is_some_and(|fp| fp.name == "bash");
                let Some(stage) = self.stages.iter().rev().find(|s| {
                    let after = if stateful && s.stop {
                        s.after + 1
                    } else {
                        s.after
                    };
                    state.consecutive_hits >= after
                }) else {
                    return GateDecision::Bypass;
                };

                if stage.stop {
                    return GateDecision::Stop {
                        reason: stage.prompt.clone(),
                    };
                }
                // 警告只在恰好到阈值那一跳发一次；bash 在 halt 放宽出的
                // 额外一跳里保持沉默（空延续），不重复注入同一条 note。
                if state.consecutive_hits == stage.after {
                    state.prompt = stage.prompt.clone();
                } else {
                    state.prompt.clear();
                }
                GateDecision::Continue {
                    reason: "doom_loop repetition warning".into(),
                    reset_peers: false,
                    continuation: String::new(),
                    metadata: None,
                }
            },
        )
    }

    pub fn continuation(&self, session_id: &str) -> String {
        self.sessions.modify(session_id, |state| {
            state.map(|s| s.prompt.clone()).unwrap_or_default()
        })
    }

    pub fn reset_turn(&self, session_id: &str) {
        self.sessions.modify(session_id, |state| {
            if let Some(state) = state {
                state.history.clear();
                state.consecutive_hits = 0;
                state.prompt.clear();
                state.last_recorded_iter = -1;
            }
        });
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    fn is_repeating(&self, state: &DoomState) -> bool {
        if state.history.len() < self.window_size {
            return false;
        }
        let window: Vec<&ToolFingerprint> =
            state.history.iter().rev().take(self.window_size).collect();
        similarity(&window) >= self.threshold
    }
}

/// `1 - (unique - 1) / (total - 1)` for a window of len >= 2.
fn similarity(window: &[&ToolFingerprint]) -> f64 {
    if window.len() <= 1 {
        return 0.0;
    }
    let total = window.len() as f64;
    let unique = window
        .iter()
        .map(|r| (r.name.as_str(), r.args_hash.as_str()))
        .collect::<HashSet<_>>()
        .len() as f64;
    1.0 - (unique - 1.0) / (total - 1.0)
}

fn push_fp(history: &mut VecDeque<ToolFingerprint>, fp: &ToolFingerprint, window_size: usize) {
    if history.len() == window_size * 2 {
        history.pop_front();
    }
    history.push_back(fp.clone());
}

impl Default for DoomLoopGate {
    fn default() -> Self {
        Self::new(3, 1.0, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paw_loop::GateCtx;

    fn hop(gate: &DoomLoopGate, iter: u32, fps: &[ToolFingerprint]) -> GateDecision {
        let mut ctx = GateCtx::new("s");
        ctx.iteration = iter;
        ctx.fingerprints = fps;
        ctx.last_tool = fps.last();
        gate.check(&ctx)
    }

    #[test]
    fn third_repeat_warns_sixth_halts() {
        let gate = DoomLoopGate::qwen_default();
        let a = ToolFingerprint::new("read", r#"{"path":"a.rs"}"#);
        assert!(matches!(hop(&gate, 1, &[a.clone()]), GateDecision::Bypass));
        assert!(matches!(hop(&gate, 2, &[a.clone()]), GateDecision::Bypass));
        assert_eq!(gate.continuation("s"), "");
        match hop(&gate, 3, &[a.clone()]) {
            GateDecision::Continue { .. } => {
                assert_eq!(gate.continuation("s"), REPEAT_NOTE);
            }
            other => panic!("expected warn at 3rd repeat: {other:?}"),
        }
        for iter in 4..=5 {
            assert!(matches!(
                hop(&gate, iter, &[a.clone()]),
                GateDecision::Continue { .. }
            ));
            assert_eq!(gate.continuation("s"), "");
        }
        match hop(&gate, 6, &[a]) {
            GateDecision::Stop { reason } => assert!(reason.contains("Doom loop"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bash_repeat_warns_at_three_halts_at_seven() {
        // 有状态命令只多放宽一次 hard-stop：第 3 次提醒，第 7 次才 halt。
        let gate = DoomLoopGate::qwen_default();
        let b = ToolFingerprint::new("bash", r#"{"command":"git status"}"#);
        assert!(matches!(hop(&gate, 1, &[b.clone()]), GateDecision::Bypass));
        assert!(matches!(hop(&gate, 2, &[b.clone()]), GateDecision::Bypass));
        assert_eq!(gate.continuation("s"), "");
        match hop(&gate, 3, &[b.clone()]) {
            GateDecision::Continue { .. } => {
                assert_eq!(gate.continuation("s"), REPEAT_NOTE);
            }
            other => panic!("expected warn at 3rd repeat: {other:?}"),
        }
        for iter in 4..=6 {
            assert!(matches!(
                hop(&gate, iter, &[b.clone()]),
                GateDecision::Continue { .. }
            ));
            assert_eq!(
                gate.continuation("s"),
                "",
                "no repeated note at hits={iter}"
            );
        }
        match hop(&gate, 7, &[b]) {
            GateDecision::Stop { reason } => assert!(reason.contains("Doom loop"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn warned_model_that_changes_course_is_not_halted() {
        let gate = DoomLoopGate::qwen_default();
        let a = ToolFingerprint::new("read", r#"{"path":"a.rs"}"#);
        let b = ToolFingerprint::new("edit", r#"{"path":"a.rs","old_string":"x"}"#);
        assert!(matches!(hop(&gate, 1, &[a.clone()]), GateDecision::Bypass));
        assert!(matches!(hop(&gate, 2, &[a]), GateDecision::Bypass));
        assert!(matches!(hop(&gate, 3, &[b]), GateDecision::Bypass));
    }

    #[test]
    fn three_different_paths_do_not_halt() {
        let gate = DoomLoopGate::qwen_default();
        let files = ["a.rs", "b.rs", "c.rs"];
        for (i, path) in files.iter().enumerate() {
            let fp = ToolFingerprint::new("read", &format!(r#"{{"path":"{path}"}}"#));
            assert!(
                matches!(hop(&gate, (i as u32) + 1, &[fp]), GateDecision::Bypass),
                "path={path}"
            );
        }
    }

    #[test]
    fn parallel_distinct_reads_repeated_do_not_halt_on_last_path() {
        // Overnight false-positive: each hop ended on the same last file.
        let gate = DoomLoopGate::qwen_default();
        let batch = vec![
            ToolFingerprint::new("read", r#"{"path":"a.rs"}"#),
            ToolFingerprint::new("read", r#"{"path":"b.rs"}"#),
            ToolFingerprint::new("read", r#"{"path":"c.rs"}"#),
        ];
        for i in 1..=4 {
            assert!(
                matches!(hop(&gate, i, &batch), GateDecision::Bypass),
                "iter={i}"
            );
        }
    }

    #[test]
    fn compact_reset_forgets_pre_compact_repeats() {
        let gate = DoomLoopGate::qwen_default();
        let a = ToolFingerprint::new("read", r#"{"path":"a.rs"}"#);
        assert!(matches!(hop(&gate, 1, &[a.clone()]), GateDecision::Bypass));
        assert!(matches!(hop(&gate, 2, &[a.clone()]), GateDecision::Bypass));
        gate.reset_turn("s");
        assert!(matches!(hop(&gate, 3, &[a.clone()]), GateDecision::Bypass));
        assert!(matches!(hop(&gate, 4, &[a.clone()]), GateDecision::Bypass));
        assert!(matches!(
            hop(&gate, 5, &[a.clone()]),
            GateDecision::Continue { .. }
        ));
        assert_eq!(gate.continuation("s"), REPEAT_NOTE);
        assert!(matches!(
            hop(&gate, 6, &[a.clone()]),
            GateDecision::Continue { .. }
        ));
        assert!(matches!(
            hop(&gate, 7, &[a.clone()]),
            GateDecision::Continue { .. }
        ));
        match hop(&gate, 8, &[a]) {
            GateDecision::Stop { reason } => assert!(reason.contains("Doom loop"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }
}
