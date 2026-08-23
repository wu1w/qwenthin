//! Judgment-layer guards over executed edits. Bare-model probes show the 27B
//! complies with a false premise even with uncapped thinking (3/3 samples
//! rationalized a spec conflict instead of questioning it), so these nudges
//! must come from the harness, not from a bigger think budget.
//!
//! Guards observe successful `edit`/`write`. Notes are hidden user messages
//! after the live query — the frozen system/tools prefix is untouched. `--print`
//! may also run a unittest probe when production files changed and the model
//! never invoked a test runner.

use crate::paw_loop::{fs_tool_path, hash_args};

/// Injected at most once per session.
pub const TEST_EXPECTATION_NOTE: &str = "[guard] 你在修改测试的期望值。测试即规格：\
若用户描述的 bug 与现有测试/文档一致，这可能是设计行为——先向用户指出矛盾并等确认，\
不要静默改期望。用户明确要求改测试时忽略本条。";

/// Injected on a detected revert pair, at most twice per session.
pub const THRASH_NOTE: &str = "[guard] 你撤销了自己刚才的修改（同一位置改了又改回）。\
停下：先用一句话说明选定的方案和理由，再按它执行，不要继续来回改。";

/// Injected at most once per session. Notices a test runner (model-invoked or
/// `--print` oracle) going red after a production-only edit.
pub const TEST_RED_NOTE: &str = "[guard] 代码改了、测试红了、测试文件没动。\
这通常是指令和规格打架——先向用户指出矛盾并等确认，不要继续改代码或改期望把测试改绿。";

/// `--print` / cron stop reason. 27B will narrate the note then keep editing.
pub const STOP_TEST_EXPECTATION: &str = "guard:test-expectation";
/// `--print` / cron stop reason after prod edit + red tests.
pub const STOP_TEST_RED: &str = "guard:test-red";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardNote {
    TestExpectation,
    Thrash,
    TestRed,
}

impl GuardNote {
    pub fn text(&self) -> &'static str {
        match self {
            Self::TestExpectation => TEST_EXPECTATION_NOTE,
            Self::Thrash => THRASH_NOTE,
            Self::TestRed => TEST_RED_NOTE,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::TestExpectation => "test-expectation",
            Self::Thrash => "edit-thrash",
            Self::TestRed => "test-red",
        }
    }

    /// Unattended channels halt the loop; TUI keeps the hidden note only.
    pub fn unattended_stop(self) -> Option<&'static str> {
        match self {
            Self::TestExpectation => Some(STOP_TEST_EXPECTATION),
            Self::TestRed => Some(STOP_TEST_RED),
            Self::Thrash => None,
        }
    }
}

#[derive(Clone, Debug)]
struct EditFp {
    path: String,
    old8: String,
    new8: String,
}

const EDIT_HISTORY_MAX: usize = 64;
const THRASH_NOTE_MAX: u8 = 2;

#[derive(Debug, Default)]
pub struct EditGuard {
    edits: Vec<EditFp>,
    test_note_fired: bool,
    thrash_notes: u8,
    red_note_fired: bool,
    prod_this_turn: bool,
    test_this_turn: bool,
    tests_seen_this_turn: bool,
    user_allows_test_edit: bool,
    /// Suite state before this turn's first edit. `None` = never probed.
    baseline_red: Option<bool>,
}

impl EditGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset_turn(&mut self, user: &str) {
        self.prod_this_turn = false;
        self.test_this_turn = false;
        self.tests_seen_this_turn = false;
        self.baseline_red = None;
        self.user_allows_test_edit = allows_test_edit(user);
    }

    /// Suite state sampled before any edit this turn. Deliberately does not mark
    /// `tests_seen_this_turn`: the post-edit run is what S7 judges.
    pub fn set_baseline(&mut self, red: bool) {
        self.baseline_red = Some(red);
    }

    /// A red suite only proves a regression when it was green before the edits.
    /// Without a baseline there is no proof, so an unattended halt is not earned.
    pub fn red_is_proven_regression(&self) -> bool {
        self.baseline_red == Some(false)
    }

    /// `--print` should run tests itself: production changed, tests file not
    /// touched, and the model did not already invoke a runner this turn.
    pub fn wants_oracle(&self) -> bool {
        self.prod_this_turn
            && !self.test_this_turn
            && !self.red_note_fired
            && !self.tests_seen_this_turn
    }

    /// Observe one successfully executed tool call. Returns the notes to
    /// inject after this tool round (already deduplicated / rate-limited).
    ///
    /// `prior_write` is the file contents before a successful `write` (None if
    /// the path was new). Needed because write has no `old_string`.
    pub fn observe(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        prior_write: Option<&str>,
    ) -> Vec<GuardNote> {
        if matches!(name, "edit" | "write") {
            if let Some(path) = fs_tool_path(name, args) {
                if is_test_path(&path) {
                    self.test_this_turn = true;
                } else {
                    self.prod_this_turn = true;
                }
            }
        }
        if name == "write" {
            return self.observe_write(args, prior_write);
        }
        if name != "edit" {
            return Vec::new();
        }
        let Some(path) = fs_tool_path(name, args) else {
            return Vec::new();
        };
        let Some(old) = args.get("old_string").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let Some(new) = args.get("new_string").and_then(|v| v.as_str()) else {
            return Vec::new();
        };

        let mut notes = Vec::new();
        if self.should_note_test_expectation(&path, old, new) {
            self.test_note_fired = true;
            notes.push(GuardNote::TestExpectation);
        }

        let fp = EditFp {
            path: normalize_path(&path),
            old8: hash_args(old),
            new8: hash_args(new),
        };
        let reverts = self
            .edits
            .iter()
            .any(|e| e.path == fp.path && e.old8 == fp.new8 && e.new8 == fp.old8);
        if reverts && self.thrash_notes < THRASH_NOTE_MAX {
            self.thrash_notes += 1;
            notes.push(GuardNote::Thrash);
        }
        if self.edits.len() == EDIT_HISTORY_MAX {
            self.edits.remove(0);
        }
        self.edits.push(fp);
        notes
    }

    fn observe_write(&mut self, args: &serde_json::Value, prior: Option<&str>) -> Vec<GuardNote> {
        let Some(path) = fs_tool_path("write", args) else {
            return Vec::new();
        };
        let Some(new) = args.get("content").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let Some(old) = prior else {
            return Vec::new();
        };
        if self.should_note_test_expectation(&path, old, new) {
            self.test_note_fired = true;
            vec![GuardNote::TestExpectation]
        } else {
            Vec::new()
        }
    }

    fn should_note_test_expectation(&self, path: &str, old: &str, new: &str) -> bool {
        !self.test_note_fired
            && !self.user_allows_test_edit
            && is_test_path(path)
            && changes_expectation(old, new)
    }

    /// After a tool round: bash/run_code already ran tests and they went red,
    /// while this user turn only edited production files.
    pub fn observe_tool_output(&mut self, name: &str, output: &str) -> Option<GuardNote> {
        if looks_like_test_output(name, output) {
            self.tests_seen_this_turn = true;
        }
        if self.red_note_fired || !self.prod_this_turn || self.test_this_turn {
            return None;
        }
        // Already red before the turn: red now is ordinary work in progress, not
        // a spec conflict. Claiming otherwise would derail a legitimate fix.
        if self.baseline_red == Some(true) {
            return None;
        }
        if !looks_like_test_fail(name, output) {
            return None;
        }
        self.red_note_fired = true;
        Some(GuardNote::TestRed)
    }
}

/// A tool round produced test-runner output (any colour).
pub fn is_test_output(name: &str, output: &str) -> bool {
    looks_like_test_output(name, output)
}

/// A tool round produced a failing test run.
pub fn is_test_fail(name: &str, output: &str) -> bool {
    looks_like_test_fail(name, output)
}

fn looks_like_test_fail(name: &str, output: &str) -> bool {
    if !matches!(name, "bash" | "run_code") {
        return false;
    }
    let l = output.to_ascii_lowercase();
    l.contains("assertionerror")
        || l.contains("failed (failures")
        || l.contains("failed (errors")
        || l.contains("test failed")
        || (l.contains("ran ") && l.contains("failed"))
}

fn looks_like_test_output(name: &str, output: &str) -> bool {
    if !matches!(name, "bash" | "run_code") {
        return false;
    }
    let l = output.to_ascii_lowercase();
    (l.contains("ran ") && l.contains("test"))
        || l.contains("failed (failures")
        || l.contains("failed (errors")
        || l.contains("short test summary")
}

fn normalize_path(path: &str) -> String {
    path.trim().trim_start_matches("./").replace('\\', "/")
}

/// tests/ or test/ directory component, or a test-named file.
fn is_test_path(path: &str) -> bool {
    let p = normalize_path(path).to_lowercase();
    let mut parts = p.split('/').peekable();
    let mut file = "";
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            file = part;
        } else if part == "tests" || part == "test" || part == "__tests__" {
            return true;
        }
    }
    file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_tests.rs")
}

/// User explicitly asked to change tests this turn — S1 stays silent.
fn allows_test_edit(user: &str) -> bool {
    let l = user.to_ascii_lowercase();
    const NEG: &[&str] = &[
        "不要改测试",
        "别改测试",
        "不许改测试",
        "不准改测试",
        "不要修改测试",
        "不要改期望",
        "不要改断言",
        "don't change the test",
        "do not change the test",
        "don't edit the test",
        "do not edit the test",
        "don't change the assertion",
        "don't update the test",
    ];
    if NEG.iter().any(|p| l.contains(&p.to_ascii_lowercase())) {
        return false;
    }
    // Only phrases that license rewriting an existing expectation. "补回归" and
    // friends are additions, which `changes_expectation` already lets through —
    // treating them as blanket permission would switch the guard off wholesale.
    [
        "改测试",
        "修改测试",
        "更新测试",
        "改断言",
        "改期望",
        "允许改测试",
        "可以改测试",
        "change the test",
        "update the test",
        "edit the test",
        "change the assertion",
        "update the assertion",
        "rewrite the test",
    ]
    .iter()
    .any(|p| l.contains(&p.to_ascii_lowercase()))
}

/// Existing assertion literals were rewritten, not merely added to.
fn changes_expectation(old: &str, new: &str) -> bool {
    if old == new {
        return false;
    }
    let looks_assert = |s: &str| {
        ["assert", "expect", ".should", "verify("]
            .iter()
            .any(|k| s.to_lowercase().contains(k))
    };
    if !looks_assert(old) || !looks_assert(new) {
        return false;
    }
    !is_submultiset(&literals(old), &literals(new))
}

fn is_submultiset(small: &[String], big: &[String]) -> bool {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for x in big {
        *counts.entry(x.as_str()).or_insert(0) += 1;
    }
    for x in small {
        match counts.get_mut(x.as_str()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => return false,
        }
    }
    true
}

/// Digit runs plus quoted strings — the values an assertion pins down.
fn literals(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_num = false;
    for ch in s.chars() {
        let numeric = ch.is_ascii_digit() || (in_num && (ch == '.' || ch == '_'));
        if numeric {
            cur.push(ch);
            in_num = true;
        } else if in_num {
            out.push(std::mem::take(&mut cur));
            in_num = false;
        }
    }
    if in_num {
        out.push(cur);
    }
    for quote in ['"', '\''] {
        let mut rest = s;
        while let Some(start) = rest.find(quote) {
            let tail = &rest[start + 1..];
            let Some(end) = tail.find(quote) else { break };
            out.push(tail[..end].to_string());
            rest = &tail[end + 1..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit(path: &str, old: &str, new: &str) -> serde_json::Value {
        json!({"path": path, "old_string": old, "new_string": new})
    }

    fn obs(g: &mut EditGuard, name: &str, args: serde_json::Value) -> Vec<GuardNote> {
        g.observe(name, &args, None)
    }

    #[test]
    fn revert_pair_fires_thrash_once_per_pair() {
        let mut g = EditGuard::new();
        assert!(obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2")).is_empty());
        let notes = obs(&mut g, "edit", edit("src/a.py", "x = 2", "x = 1"));
        assert_eq!(notes, vec![GuardNote::Thrash]);
    }

    #[test]
    fn thrash_capped_at_two_notes() {
        let mut g = EditGuard::new();
        obs(&mut g, "edit", edit("a.py", "v1", "v2"));
        assert_eq!(
            obs(&mut g, "edit", edit("a.py", "v2", "v1")),
            vec![GuardNote::Thrash]
        );
        // Ping-pong continues: the forward edit itself reverts the revert.
        assert_eq!(
            obs(&mut g, "edit", edit("a.py", "v1", "v2")),
            vec![GuardNote::Thrash]
        );
        // Cap reached — later flips stay silent.
        assert!(obs(&mut g, "edit", edit("a.py", "v2", "v1")).is_empty());
        assert!(obs(&mut g, "edit", edit("a.py", "v1", "v2")).is_empty());
    }

    #[test]
    fn different_paths_do_not_thrash() {
        let mut g = EditGuard::new();
        obs(&mut g, "edit", edit("a.py", "x", "y"));
        assert!(obs(&mut g, "edit", edit("b.py", "y", "x")).is_empty());
    }

    #[test]
    fn forward_progress_does_not_thrash() {
        let mut g = EditGuard::new();
        obs(&mut g, "edit", edit("a.py", "v1", "v2"));
        assert!(obs(&mut g, "edit", edit("a.py", "v2", "v3")).is_empty());
        assert!(obs(&mut g, "edit", edit("a.py", "v3", "v4")).is_empty());
    }

    #[test]
    fn assertion_literal_change_in_tests_fires_once() {
        let mut g = EditGuard::new();
        let notes = obs(
            &mut g,
            "edit",
            edit(
                "tests/test_report.py",
                "self.assertAlmostEqual(totals[\"2026-08\"], 1932.00)",
                "self.assertAlmostEqual(totals[\"2026-08\"], -1957.50)",
            ),
        );
        assert_eq!(notes, vec![GuardNote::TestExpectation]);
        let again = obs(
            &mut g,
            "edit",
            edit(
                "tests/test_report.py",
                "assertEqual(n, 3)",
                "assertEqual(n, 4)",
            ),
        );
        assert!(again.is_empty());
    }

    #[test]
    fn non_assertion_test_edit_does_not_fire() {
        let mut g = EditGuard::new();
        let notes = obs(
            &mut g,
            "edit",
            edit("tests/test_report.py", "import os", "import os\nimport sys"),
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn assertion_change_outside_tests_does_not_fire() {
        let mut g = EditGuard::new();
        let notes = obs(
            &mut g,
            "edit",
            edit("src/report.py", "assert n == 3", "assert n == 4"),
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn new_test_write_without_prior_is_ignored() {
        let mut g = EditGuard::new();
        assert!(obs(
            &mut g,
            "write",
            json!({"path": "tests/test_x.py", "content": "assert 1 == 2"})
        )
        .is_empty());
        assert!(obs(&mut g, "read", json!({"path": "tests/test_x.py"})).is_empty());
    }

    #[test]
    fn overwrite_test_write_with_changed_literals_fires() {
        let mut g = EditGuard::new();
        let notes = g.observe(
            "write",
            &json!({"path": "tests/test_x.py", "content": "assertEqual(n, 4)"}),
            Some("assertEqual(n, 3)"),
        );
        assert_eq!(notes, vec![GuardNote::TestExpectation]);
    }

    #[test]
    fn explicit_test_edit_request_skips_expectation_note() {
        let mut g = EditGuard::new();
        g.reset_turn("请改测试期望，把 3 改成 4");
        assert!(obs(
            &mut g,
            "edit",
            edit("tests/test_x.py", "assertEqual(n, 3)", "assertEqual(n, 4)")
        )
        .is_empty());
    }

    #[test]
    fn adding_regression_test_does_not_fire() {
        let mut g = EditGuard::new();
        let old = "self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])";
        let new = "self.assertEqual(merge_intervals([[1,3],[2,6],[8,10]]), [[1,6],[8,10]])\n\
             def test_unsorted_regression(self):\n\
                 self.assertEqual(merge_intervals([[8,10],[1,3],[2,6]]), [[1,6],[8,10]])";
        assert!(obs(&mut g, "edit", edit("tests/test_merge.py", old, new)).is_empty());
    }

    #[test]
    fn do_not_change_tests_does_not_allow_expectation_edit() {
        let mut g = EditGuard::new();
        g.reset_turn("立刻改。不要改测试。");
        assert_eq!(
            obs(
                &mut g,
                "edit",
                edit("tests/test_x.py", "assertEqual(n, 3)", "assertEqual(n, 4)")
            ),
            vec![GuardNote::TestExpectation]
        );
    }

    /// "补回归" is permission to add, not to rewrite: an appended case passes,
    /// a rewritten literal still trips the guard.
    #[test]
    fn regression_ask_allows_adding_but_not_rewriting() {
        let mut g = EditGuard::new();
        g.reset_turn("修并补回归。不要动 legacy。");
        let old = "self.assertEqual(f(1), 3)";
        assert!(obs(
            &mut g,
            "edit",
            edit(
                "tests/test_x.py",
                old,
                &format!("{old}\n        self.assertEqual(f(2), 9)")
            )
        )
        .is_empty());
        assert_eq!(
            obs(
                &mut g,
                "edit",
                edit("tests/test_x.py", old, "self.assertEqual(f(1), 4)")
            ),
            vec![GuardNote::TestExpectation]
        );
    }

    #[test]
    fn red_baseline_suppresses_test_red_note() {
        let mut g = EditGuard::new();
        g.set_baseline(true);
        obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2"));
        assert!(g
            .observe_tool_output("bash", "Ran 1 test in 0.001s\n\nFAILED (failures=1)")
            .is_none());
        assert!(!g.red_is_proven_regression());
    }

    #[test]
    fn green_baseline_makes_red_a_proven_regression() {
        let mut g = EditGuard::new();
        g.set_baseline(false);
        obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2"));
        assert_eq!(
            g.observe_tool_output("bash", "Ran 1 test in 0.001s\n\nFAILED (failures=1)"),
            Some(GuardNote::TestRed)
        );
        assert!(g.red_is_proven_regression());
    }

    #[test]
    fn baseline_probe_does_not_consume_the_oracle() {
        let mut g = EditGuard::new();
        g.set_baseline(false);
        obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2"));
        assert!(g.wants_oracle(), "post-edit run is still owed");
    }

    #[test]
    fn test_path_heuristics() {
        for p in [
            "tests/test_report.py",
            "test/util_test.go",
            "src/__tests__/app.test.ts",
            "pkg/thing_test.go",
            "spec/models/user.spec.js",
            "test_cli.py",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "src/report.py",
            "contest/entry.py",
            "src/testing_helpers.py",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn quoted_string_literals_detected() {
        assert!(changes_expectation(
            "expect(msg).toBe('hello')",
            "expect(msg).toBe('goodbye')"
        ));
        assert!(!changes_expectation(
            "expect(msg).toBe('hello')",
            "expect(msg) .toBe('hello')"
        ));
    }

    #[test]
    fn prod_edit_then_test_fail_fires_once() {
        let mut g = EditGuard::new();
        obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2"));
        assert_eq!(
            g.observe_tool_output("bash", "Ran 1 test in 0.001s\n\nFAILED (failures=1)"),
            Some(GuardNote::TestRed)
        );
        assert!(g
            .observe_tool_output("bash", "FAILED (failures=1)")
            .is_none());
    }

    #[test]
    fn test_file_edit_suppresses_red_note() {
        let mut g = EditGuard::new();
        obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2"));
        obs(
            &mut g,
            "edit",
            edit("tests/test_a.py", "assert x == 1", "assert x == 2"),
        );
        assert!(g
            .observe_tool_output("bash", "FAILED (failures=1)")
            .is_none());
    }

    #[test]
    fn read_output_does_not_count_as_test_fail() {
        let mut g = EditGuard::new();
        obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2"));
        assert!(g
            .observe_tool_output("read", "AssertionError: boom")
            .is_none());
    }

    #[test]
    fn prod_edit_without_runner_wants_oracle() {
        let mut g = EditGuard::new();
        obs(&mut g, "edit", edit("src/a.py", "x = 1", "x = 2"));
        assert!(g.wants_oracle());
        let _ = g.observe_tool_output("bash", "Ran 1 test in 0.001s\n\nOK\n");
        assert!(!g.wants_oracle());
    }
}
