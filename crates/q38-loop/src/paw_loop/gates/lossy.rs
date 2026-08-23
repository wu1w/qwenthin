//! Lossy-quant overlay gates. The model never sees these.

use serde_json::Value;

use crate::paw_loop::store::SessionMap;
use crate::paw_loop::{GateCtx, GateDecision};

struct NameState {
    last_name: String,
    last_args: String,
    count: u32,
    last_iter: i64,
}

pub struct NameStreakGate {
    sessions: SessionMap<NameState>,
    halt_after: u32,
}

impl NameStreakGate {
    pub fn new(halt_after: u32) -> Self {
        Self {
            sessions: SessionMap::new(),
            halt_after: halt_after.max(2),
        }
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        self.sessions.get_or_insert_with(
            ctx.session_id,
            || NameState {
                last_name: String::new(),
                last_args: String::new(),
                count: 0,
                last_iter: -1,
            },
            |state| {
                if ctx.tool_names.is_empty() && ctx.fingerprints.is_empty() {
                    state.last_name.clear();
                    state.last_args.clear();
                    state.count = 0;
                    state.last_iter = -1;
                    return GateDecision::Bypass;
                }
                let iter = i64::from(ctx.iteration);
                if iter == state.last_iter {
                    return halt_name(state.count, self.halt_after);
                }
                state.last_iter = iter;
                if !ctx.fingerprints.is_empty() {
                    for fp in ctx.fingerprints {
                        bump_name(state, &fp.name, &fp.args_hash);
                        if state.count >= self.halt_after {
                            return GateDecision::Stop {
                                reason: "Name streak: repeated the same tool".into(),
                            };
                        }
                    }
                } else {
                    for name in ctx.tool_names {
                        bump_name(state, name, "");
                        if state.count >= self.halt_after {
                            return GateDecision::Stop {
                                reason: "Name streak: repeated the same tool".into(),
                            };
                        }
                    }
                }
                GateDecision::Bypass
            },
        )
    }

    pub fn reset_turn(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}

fn bump_name(state: &mut NameState, name: &str, args_hash: &str) {
    if !state.last_name.is_empty() && name == state.last_name && args_hash == state.last_args {
        state.count = state.count.saturating_add(1);
    } else {
        state.last_name = name.to_string();
        state.last_args = args_hash.to_string();
        state.count = 1;
    }
}

fn halt_name(count: u32, halt_after: u32) -> GateDecision {
    if count >= halt_after {
        GateDecision::Stop {
            reason: "Name streak: repeated the same tool".into(),
        }
    } else {
        GateDecision::Bypass
    }
}

struct PathState {
    last_name: String,
    last_path: String,
    last_args: String,
    count: u32,
    last_iter: i64,
}

pub struct PathLoopGate {
    sessions: SessionMap<PathState>,
    halt_after: u32,
}

impl PathLoopGate {
    pub fn new(halt_after: u32) -> Self {
        Self {
            sessions: SessionMap::new(),
            halt_after: halt_after.max(2),
        }
    }

    pub fn check(&self, ctx: &GateCtx<'_>) -> GateDecision {
        self.sessions.get_or_insert_with(
            ctx.session_id,
            || PathState {
                last_name: String::new(),
                last_path: String::new(),
                last_args: String::new(),
                count: 0,
                last_iter: -1,
            },
            |state| {
                if ctx.fingerprints.is_empty() {
                    state.last_name.clear();
                    state.last_path.clear();
                    state.last_args.clear();
                    state.count = 0;
                    state.last_iter = -1;
                    return GateDecision::Bypass;
                }
                let iter = i64::from(ctx.iteration);
                if iter == state.last_iter {
                    return halt_path(state.count, self.halt_after);
                }
                state.last_iter = iter;
                for fp in ctx.fingerprints {
                    let Some(path) = fp.path.as_deref() else {
                        state.last_name.clear();
                        state.last_path.clear();
                        state.last_args.clear();
                        state.count = 0;
                        continue;
                    };
                    // 只读分页不算循环：压缩注记教模型用 offset/limit 翻页，
                    // read/view 换了 args（args_hash 不同）视为新 streak 起点，
                    // 只有一字不差的重读才累计。edit/write 保持原语义。
                    let paging = matches!(fp.name.as_str(), "read" | "view")
                        && fp.args_hash != state.last_args;
                    if !paging
                        && !state.last_path.is_empty()
                        && path == state.last_path
                        && fp.name == state.last_name
                    {
                        state.count = state.count.saturating_add(1);
                    } else {
                        state.last_name = fp.name.clone();
                        state.last_path = path.to_string();
                        state.count = 1;
                    }
                    state.last_args = fp.args_hash.clone();
                    if state.count >= self.halt_after {
                        return GateDecision::Stop {
                            reason: "Path loop: repeated the same tool on the same path".into(),
                        };
                    }
                }
                GateDecision::Bypass
            },
        )
    }

    pub fn reset_turn(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    pub fn reset_session(&self, session_id: &str) {
        self.sessions.remove(session_id);
    }
}

fn halt_path(count: u32, halt_after: u32) -> GateDecision {
    if count >= halt_after {
        GateDecision::Stop {
            reason: "Path loop: repeated the same tool on the same path".into(),
        }
    } else {
        GateDecision::Bypass
    }
}

pub fn fs_tool_path(name: &str, args: &Value) -> Option<String> {
    match name {
        "read" | "edit" | "write" => args
            .get("path")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("file_path").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paw_loop::ToolFingerprint;

    fn run_names(gate: &NameStreakGate, names: &[&str]) -> Option<String> {
        let owned: Vec<String> = names.iter().map(|s| (*s).to_string()).collect();
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 1;
        ctx.tool_names = &owned;
        match gate.check(&ctx) {
            GateDecision::Stop { reason } => Some(reason),
            _ => None,
        }
    }

    #[test]
    fn name_streak_halts_at_four_same() {
        let gate = NameStreakGate::new(4);
        for (i, name) in ["bash", "bash", "bash"].iter().enumerate() {
            let owned = vec![(*name).to_string()];
            let mut ctx = GateCtx::new("s");
            ctx.iteration = (i as u32) + 1;
            ctx.tool_names = &owned;
            assert!(matches!(gate.check(&ctx), GateDecision::Bypass));
        }
        assert!(run_names(&gate, &["bash"]).is_some());
    }

    #[test]
    fn name_streak_read_edit_read_does_not_halt() {
        let gate = NameStreakGate::new(4);
        for (i, name) in ["read", "edit", "read", "edit"].iter().enumerate() {
            let owned = vec![(*name).to_string()];
            let mut ctx = GateCtx::new("s");
            ctx.iteration = (i as u32) + 1;
            ctx.tool_names = &owned;
            assert!(
                matches!(gate.check(&ctx), GateDecision::Bypass),
                "name={name}"
            );
        }
    }

    #[test]
    fn name_streak_same_name_different_args_does_not_halt() {
        let gate = NameStreakGate::new(4);
        for i in 0..6 {
            let fps = vec![ToolFingerprint::new(
                "bash",
                &format!(r#"{{"command":"echo {i}"}}"#),
            )];
            let names = vec!["bash".to_string()];
            let mut ctx = GateCtx::new("s");
            ctx.iteration = (i as u32) + 1;
            ctx.tool_names = &names;
            ctx.fingerprints = &fps;
            assert!(matches!(gate.check(&ctx), GateDecision::Bypass), "i={i}");
        }
    }

    #[test]
    fn name_streak_same_name_and_args_still_halts() {
        let gate = NameStreakGate::new(4);
        let fps = vec![ToolFingerprint::new("bash", r#"{"command":"pwd"}"#)];
        let names = vec!["bash".to_string()];
        for i in 0..3 {
            let mut ctx = GateCtx::new("s");
            ctx.iteration = (i as u32) + 1;
            ctx.tool_names = &names;
            ctx.fingerprints = &fps;
            assert!(matches!(gate.check(&ctx), GateDecision::Bypass), "i={i}");
        }
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 4;
        ctx.tool_names = &names;
        ctx.fingerprints = &fps;
        match gate.check(&ctx) {
            GateDecision::Stop { reason } => assert!(reason.contains("Name streak"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn path_loop_halts_at_three_same_path() {
        // 一字不差的同路径重读仍在 3 次时 halt。
        let gate = PathLoopGate::new(3);
        for i in 0..2 {
            let fps = vec![
                ToolFingerprint::new("read", r#"{"offset":0}"#).with_path(Some("a.rs".into()))
            ];
            let mut ctx = GateCtx::new("s");
            ctx.iteration = i + 1;
            ctx.fingerprints = &fps;
            assert!(matches!(gate.check(&ctx), GateDecision::Bypass));
        }
        let fps =
            vec![ToolFingerprint::new("read", r#"{"offset":0}"#).with_path(Some("a.rs".into()))];
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 3;
        ctx.fingerprints = &fps;
        match gate.check(&ctx) {
            GateDecision::Stop { reason } => assert!(reason.contains("Path loop"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn path_loop_paging_reads_do_not_halt() {
        // offset 分页是压缩注记推荐的动作，不同 args 的 read 不计 streak。
        let gate = PathLoopGate::new(3);
        for i in 0..6 {
            let fps = vec![
                ToolFingerprint::new("read", &format!(r#"{{"offset":{}}}"#, i * 100))
                    .with_path(Some("a.rs".into())),
            ];
            let mut ctx = GateCtx::new("s");
            ctx.iteration = i + 1;
            ctx.fingerprints = &fps;
            assert!(matches!(gate.check(&ctx), GateDecision::Bypass), "i={i}");
        }
        // 分页后又原地重读同一参数（第 6 轮已算 1 次），第 3 次一字不差时 halt。
        let fps =
            vec![ToolFingerprint::new("read", r#"{"offset":500}"#).with_path(Some("a.rs".into()))];
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 7;
        ctx.fingerprints = &fps;
        assert!(matches!(gate.check(&ctx), GateDecision::Bypass));
        let mut ctx = GateCtx::new("s");
        ctx.iteration = 8;
        ctx.fingerprints = &fps;
        match gate.check(&ctx) {
            GateDecision::Stop { reason } => assert!(reason.contains("Path loop"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn path_loop_read_edit_read_does_not_halt() {
        let gate = PathLoopGate::new(3);
        let steps = [
            ("read", r#"{"offset":0}"#),
            ("edit", r#"{"old":"a"}"#),
            ("read", r#"{"offset":1}"#),
            ("edit", r#"{"old":"b"}"#),
        ];
        for (i, (name, args)) in steps.iter().enumerate() {
            let fps = vec![ToolFingerprint::new(*name, args).with_path(Some("a.rs".into()))];
            let mut ctx = GateCtx::new("s");
            ctx.iteration = (i as u32) + 1;
            ctx.fingerprints = &fps;
            assert!(
                matches!(gate.check(&ctx), GateDecision::Bypass),
                "step {name}"
            );
        }
    }

    #[test]
    fn fs_tool_path_sees_file_path_alias() {
        let via_alias = serde_json::json!({"file_path": "a.rs"});
        assert_eq!(fs_tool_path("read", &via_alias).as_deref(), Some("a.rs"));
        assert_eq!(fs_tool_path("edit", &via_alias).as_deref(), Some("a.rs"));
        assert_eq!(fs_tool_path("write", &via_alias).as_deref(), Some("a.rs"));
        let via_path = serde_json::json!({"path": "b.rs"});
        assert_eq!(fs_tool_path("read", &via_path).as_deref(), Some("b.rs"));
        let both = serde_json::json!({"path": "a.rs", "file_path": "b.rs"});
        assert_eq!(fs_tool_path("read", &both).as_deref(), Some("a.rs"));
        assert_eq!(fs_tool_path("bash", &via_alias), None);
    }
}
