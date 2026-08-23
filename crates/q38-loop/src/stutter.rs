//! Cheap stutter / restatement detectors. The model never sees these.
//!
//! Lossy stutter and dump-like hops receive a soft trajectory note instead of
//! a harness stop. `python3 scripts/stop_reasons.py`.

use std::collections::HashSet;

/// Lossy text stutter. One hidden observation, then the model decides.
pub const STUTTER_NOTE: &str = "[trajectory] 可见输出在原地重复。\
收束成一句结论，或改做能产生新证据的下一步。";
/// A repeated visible answer plus scratch-file/cleanup activity is usually a
/// sign that the trajectory is wandering. Give that observation back to the
/// model and let it choose; the placeholder-write tool guard still prevents a
/// bogus answer file from landing in the workspace.
pub const DUMP_NOTE: &str = "[trajectory] 可见答案正在重复，同时出现了低信息量的暂存或清理动作。\
若任务已经完成可直接收尾；否则明确还缺哪条证据，并只做能补齐它的下一步。";

/// Consecutive identical short lines ≥4, or a ≥16-char block repeated ≥5 times.
pub fn is_stutter(content: &str, reasoning: &str) -> bool {
    text_stutters(content) || text_stutters(reasoning)
}

/// Long enough to be a user-visible answer, not a one-line status.
pub fn is_substantial_reply(s: &str) -> bool {
    normalize_reply(s).chars().count() >= MIN_RESTATE_CHARS
}

/// Same essay restated on a later hop (pi-style answer stagnation).
/// Short status lines never match, so read/edit narration is untouched.
pub fn is_restated_reply(prev: &str, next: &str) -> bool {
    let a = normalize_reply(prev);
    let b = normalize_reply(next);
    let na = a.chars().count();
    let nb = b.chars().count();
    if na < MIN_RESTATE_CHARS || nb < MIN_RESTATE_CHARS {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if na <= nb { (&a, &b) } else { (&b, &a) };
    // Prefix match only counts when the two replies are the same scale.
    // A later full review that starts with a checkpoint is work, not a dump.
    if long.starts_with(short.as_str()) && similar_reply_scale(na, nb) {
        return true;
    }
    trigram_jaccard(&a, &b) >= RESTATE_JACCARD
}

/// `rm` / `unlink` plus inspect/`echo` (the 27B often `ls && cat && rm`).
/// `rm && cargo test` is still work.
pub fn is_cleanup_bash(command: &str) -> bool {
    let parts: Vec<&str> = command
        .split(|c| matches!(c, ';' | '\n'))
        .flat_map(|chunk| chunk.split("&&"))
        .flat_map(|chunk| chunk.split("||"))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return false;
    }
    let mut saw_rm = false;
    for p in parts {
        if is_rm_statement(p) {
            saw_rm = true;
            continue;
        }
        if is_inspect_statement(p) {
            continue;
        }
        return false;
    }
    saw_rm
}

/// Truncated native call: `write({path:"...", content:"..."})`.
pub fn is_placeholder_path(path: &str) -> bool {
    let t = path.trim().trim_matches(|c| matches!(c, '\'' | '"' | '`'));
    if t.is_empty() {
        return true;
    }
    let name = t.rsplit('/').next().unwrap_or(t).trim();
    !name.is_empty() && name.chars().all(|c| matches!(c, '.' | '…' | '⋯'))
}

pub fn is_placeholder_content(s: &str) -> bool {
    let t = s.trim();
    let n = t.chars().count();
    n > 0 && n <= 16 && t.chars().all(|c| matches!(c, '.' | '…' | '⋯' | ' ' | '\n'))
}

pub fn is_placeholder_write(path: &str, content: &str) -> bool {
    is_placeholder_path(path) || is_placeholder_content(content)
}

/// Visible reply was a stub; the write body holds the rest of the same essay.
/// Programming writes (code body ≠ chat) return None.
pub fn promote_dumped_reply(content: &str, write_bodies: &[&str]) -> Option<String> {
    if write_bodies.len() != 1 {
        return None;
    }
    let c = content.trim();
    let body = write_bodies[0].trim();
    if !is_substantial_reply(c) || !is_substantial_reply(body) {
        return None;
    }
    if body == c {
        return None;
    }
    if body.starts_with(c) || body.contains(c) || is_restated_reply(c, body) {
        return Some(body.to_string());
    }
    None
}

fn is_rm_statement(p: &str) -> bool {
    matches!(command_head(p), "rm" | "unlink")
}

fn is_inspect_statement(p: &str) -> bool {
    matches!(
        command_head(p),
        "ls" | "cat"
            | "head"
            | "tail"
            | "wc"
            | "file"
            | "stat"
            | "pwd"
            | "cd"
            | "echo"
            | "printf"
            | "true"
            | ":"
            | "test"
            | "["
    )
}

fn command_head(p: &str) -> &str {
    let t = p.trim().trim_start_matches("command ").trim();
    let t = t
        .trim_start_matches("/usr/bin/")
        .trim_start_matches("/bin/");
    t.split_whitespace().next().unwrap_or("")
}

const MIN_RESTATE_CHARS: usize = 160;
const RESTATE_JACCARD: f32 = 0.82;
const GRAM_SCAN: usize = 800;

fn similar_reply_scale(na: usize, nb: usize) -> bool {
    let (lo, hi) = if na <= nb { (na, nb) } else { (nb, na) };
    hi <= lo.saturating_mul(5) / 4 + 48
}

fn normalize_reply(s: &str) -> String {
    let mut out = String::new();
    let mut spaced = false;
    for c in s.chars() {
        if c.is_whitespace()
            || matches!(
                c,
                '#' | '*' | '`' | '|' | '_' | '[' | ']' | '(' | ')' | '>' | '-'
            )
        {
            if !spaced && !out.is_empty() {
                out.push(' ');
                spaced = true;
            }
            continue;
        }
        spaced = false;
        for lower in c.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

fn trigram_jaccard(a: &str, b: &str) -> f32 {
    let ga = char_trigrams(a);
    let gb = char_trigrams(b);
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    let inter = ga.intersection(&gb).count() as f32;
    let union = ga.union(&gb).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn char_trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().take(GRAM_SCAN).collect();
    if chars.len() < 3 {
        return HashSet::new();
    }
    chars.windows(3).map(|w| w.iter().collect()).collect()
}

fn text_stutters(s: &str) -> bool {
    !s.is_empty() && (consecutive_short_lines(s) || repeated_block(s))
}

fn consecutive_short_lines(s: &str) -> bool {
    let mut prev: Option<&str> = None;
    let mut streak = 0u32;
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() || t.chars().count() > 80 {
            prev = None;
            streak = 0;
            continue;
        }
        if Some(t) == prev {
            streak += 1;
            if streak >= 4 {
                return true;
            }
        } else {
            prev = Some(t);
            streak = 1;
        }
    }
    false
}

fn repeated_block(s: &str) -> bool {
    const MIN_UNIT: usize = 16;
    const TIMES: usize = 5;
    const MAX_UNIT: usize = 64;
    const SCAN: usize = 2000;
    let chars: Vec<char> = {
        let n = s.chars().count();
        if n > SCAN {
            s.chars().skip(n - SCAN).collect()
        } else {
            s.chars().collect()
        }
    };
    let n = chars.len();
    if n < MIN_UNIT * TIMES {
        return false;
    }
    let max_unit = MAX_UNIT.min(n / TIMES);
    for unit in MIN_UNIT..=max_unit {
        let need = unit * TIMES;
        let step = (unit / 4).max(1);
        let mut start = 0;
        while start + need <= n {
            let pat = &chars[start..start + unit];
            let mut ok = true;
            for k in 1..TIMES {
                if &chars[start + k * unit..start + (k + 1) * unit] != pat {
                    ok = false;
                    break;
                }
            }
            if ok {
                return true;
            }
            start += step;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_short_lines_hit() {
        let s = "x\nx\nx\nx\n";
        assert!(is_stutter(s, ""));
        assert!(!is_stutter("x\nx\nx\n", ""));
    }

    #[test]
    fn sixteen_char_block_times_five() {
        let unit = "abcdefghijklmnop";
        let s = unit.repeat(5);
        assert!(is_stutter(&s, ""));
        assert!(!is_stutter(&unit.repeat(4), ""));
        assert!(!is_stutter("normal sentence about a coding task.", ""));
    }

    #[test]
    fn reasoning_also_counts() {
        let s = "wait\nwait\nwait\nwait";
        assert!(is_stutter("", s));
    }

    const ESSAY: &str =
        "I studied the q-harness agent loop in detail. The core crate is q38-loop. \
It runs a ReAct cycle with frozen tools read write edit bash. Template rendering uses the \
official Qwen3.8 Jinja chat template. Adapter builds OpenAI-compat requests. Sticky notes \
hold skill and MCP cards. This is a strong fit for the 27B local model because the prefix \
is byte-stable and tools stay frozen.";

    #[test]
    fn restated_essay_hits() {
        let other = ESSAY.replace("in detail", "carefully");
        assert!(is_restated_reply(ESSAY, &other));
        assert!(is_restated_reply(ESSAY, ESSAY));
    }

    #[test]
    fn short_status_is_not_a_restate() {
        assert!(!is_restated_reply("好的，我继续读。", "好的，我继续读。"));
        assert!(!is_substantial_reply("好的，我继续读。"));
    }

    #[test]
    fn different_long_tasks_do_not_hit() {
        let other = "Next I will add a unit test in agent/mod.rs that writes ping.txt then \
reads it back. The test should use a Scripted completer and assert stop_reason is none. \
Then I will run cargo test -p q38-loop --lib. After green, update the cron job prompt. \
This is a different task from architecture review and names different files on purpose.";
        assert!(is_substantial_reply(other));
        assert!(!is_restated_reply(ESSAY, other));
    }

    #[test]
    fn expanded_checkpoint_is_not_a_restate() {
        let checkpoint: String = ESSAY.chars().take(200).collect();
        assert!(is_substantial_reply(&checkpoint));
        assert!(
            !is_restated_reply(&checkpoint, ESSAY),
            "a later full answer may start with an earlier observation"
        );
        assert!(is_restated_reply(
            ESSAY,
            &ESSAY.replace("in detail", "carefully")
        ));
    }

    #[test]
    fn cleanup_bash_is_rm_only() {
        assert!(is_cleanup_bash("rm -f /tmp/analysis.md"));
        assert!(is_cleanup_bash("rm -f a.md b.md 2>/dev/null; echo done"));
        assert!(is_cleanup_bash(
            "ls -la /tmp/... 2>/dev/null && cat /tmp/... && rm /tmp/... && echo REMOVED"
        ));
        assert!(is_cleanup_bash(
            "cd /tmp && rm -f './...' && ls -la | head -20"
        ));
        assert!(!is_cleanup_bash("rm -f a.md && cargo test"));
        assert!(!is_cleanup_bash("scp report.md host:"));
        assert!(!is_cleanup_bash("python3 scripts/github_trending.py"));
    }

    #[test]
    fn placeholder_write_is_ellipsis_path() {
        assert!(is_placeholder_path("..."));
        assert!(is_placeholder_path("/Users/william/q-harness/..."));
        assert!(is_placeholder_path("./..."));
        assert!(is_placeholder_write("...", "..."));
        assert!(!is_placeholder_path("notes/analysis.md"));
        assert!(!is_placeholder_path(".gitignore"));
        assert!(is_placeholder_content("..."));
        assert!(!is_placeholder_content("fn main() {}"));
    }

    #[test]
    fn promote_prefix_essay_to_write_body() {
        let stub = format!("{ESSAY}\n\n### 适配度评分");
        let body = format!("{stub}\n\n完整表格和结论写在这里，直到收束。");
        assert_eq!(
            promote_dumped_reply(&stub, &[body.as_str()]).as_deref(),
            Some(body.as_str())
        );
        assert!(promote_dumped_reply("ok", &[body.as_str()]).is_none());
        let code = "fn main() { println!(\"hi\"); }\n".repeat(8);
        assert!(promote_dumped_reply(ESSAY, &[code.as_str()]).is_none());
    }
}
