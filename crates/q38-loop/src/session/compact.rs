//! 262k live-window compact. JSONL is never rewritten.
//!
//! Soft trigger: `n + reserve > working_window * compact_ratio` (ratio clamped
//! 0.10..=1.0) — try compact. Hard trigger: `n + reserve > working_window` —
//! `budget:context` after compact cannot shrink enough. A new user turn also
//! tries PreviousTurns compact when prefix is over soft **or** above 120k **or**
//! the previous turn had ≥8 tool results (Flash-Next: 6 — 1-slot cold prefill
//! is expensive), so a finished tool-heavy round is not replayed cold.
//! The index keeps the first tools as well as the latest (same shape as
//! Current State) and prints full blob SHAs so `recall(blob=)` can expand.
//!
//! Compaction rewrites the *shape* of the live prefix (one cache miss,
//! `cache_invalidated=compact`) while normal turns preserve historical think.
//! Appending `recall` after that miss is `cache_invalidated=tools` on the same hop.
//!
//! Official Qwen3.8 Jinja (and the Unsloth copy on the reference box):
//! - `last_query_index` = last user whose trimmed content is **not** a
//!   `<tool_response>` wrap. Archive text must be wrapped; a real user must
//!   remain. Live probe: wrapped archive + "reply 7" answered 7; unwrapped
//!   archive-only was treated as the task.
//! - consecutive `role=tool` concatenate into one user turn. Do not split an
//!   assistant's `tool_calls` from their matching results (including parallel).
//! - Unsloth raises if `function.arguments` is a JSON string; derive already
//!   emits objects via `OpenAiToolCall::to_value`.

use crate::session::event::{CompactEvent, OpenAiToolCall, SessionEvent, ToolEvent};
use crate::template::{is_hidden_user_text, wrap_tool_response, ChatMessage};

const INDEX_LINES: usize = 80;
/// Keep the first tools as well as the latest — same shape as Current State.
const INDEX_HEAD: usize = 20;
const CLIP: usize = 120;
const ARCHIVE_CHARS: usize = 4000;
const STATE_CAP: usize = 10;
const STATE_HEAD: usize = 3;
const NOTE_CAP: usize = 2;
const DECISION_CAP: usize = 8;
/// Prior real user turns that left the live window. Cap drops oldest.
const CONSTRAINT_CAP: usize = 6;
const PRIOR_USER: &str = "Prior User";
const PRIOR_USER_LEGACY: &str = "Constraints";

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Evict closed groups strictly before the last real user.
    PreviousTurns,
    /// Also evict older closed tool rounds in the current user turn, keeping
    /// the last closed group (or only the user if that is the only shrink).
    KeepLastGroup,
}

#[derive(Clone, Debug)]
struct Group {
    start: usize,
    end: usize,
    closed: bool,
}

pub fn plan_compact(events: &[SessionEvent]) -> Option<CompactEvent> {
    let prev_until = last_until(events);
    if let Some(plan) = plan_mode(events, Mode::PreviousTurns) {
        if plan.until_seq > prev_until {
            return Some(plan);
        }
    }
    let plan = plan_mode(events, Mode::KeepLastGroup)?;
    (plan.until_seq > prev_until).then_some(plan)
}

pub fn apply_compact(messages: &[ChatMessage], plan: &CompactEvent) -> Vec<ChatMessage> {
    let keep_user = plan.keep_user_seq as usize;
    let until = plan.until_seq as usize;
    let mut out = Vec::with_capacity(messages.len().saturating_sub(until).saturating_add(3));
    if let Some(system) = messages.first() {
        out.push(system.clone());
    }
    out.push(ChatMessage::user(plan.archive_user_text()));
    if keep_user <= until {
        if let Some(user) = messages.get(keep_user) {
            out.push(user.clone());
        }
    }
    for (i, msg) in messages.iter().enumerate().skip(1) {
        if i > until {
            out.push(msg.clone());
        }
    }
    out
}

pub fn compact_messages(messages: &[ChatMessage]) -> Option<(CompactEvent, Vec<ChatMessage>)> {
    let events = events_from_messages(messages);
    let plan = plan_compact(&events)?;
    Some((plan.clone(), apply_compact(messages, &plan)))
}

impl CompactEvent {
    pub fn with_hint(mut self, hint: &str) -> Self {
        let h = hint.trim();
        if !h.is_empty() {
            self.summary.push_str("\n\n## Compact hint\n");
            self.summary
                .push_str(&h.chars().take(2000).collect::<String>());
        }
        self
    }

    pub fn archive_user_text(&self) -> String {
        wrap_tool_response(&self.archive_body())
    }

    pub fn archive_body(&self) -> String {
        let mut head = String::from("[archived]\n");
        if !self.summary.is_empty() {
            head.push_str(self.summary.trim());
            head.push('\n');
        }
        let footer = "\n═══════════════ END OF ARCHIVED INDEX ═══════════════\n\
             The CURRENT LIVE TURN follows. Answer the most recent USER message there.\n\
             Use recall to search archived turns or expand a blob by sha.\n";
        let mut body = head;
        if !self.index.is_empty() {
            let used = body.chars().count() + footer.chars().count() + "## Index\n".len();
            let budget = ARCHIVE_CHARS.saturating_sub(used);
            body.push('\n');
            body.push_str("## Index\n");
            body.push_str(&fit_tail(self.index.trim(), budget));
            body.push('\n');
        }
        body.push_str(footer);
        clip_chars(&body, ARCHIVE_CHARS)
    }
}

fn last_until(events: &[SessionEvent]) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            SessionEvent::Compact(c) => Some(c.until_seq),
            _ => None,
        })
        .unwrap_or(0)
}

fn last_real_user_seq(events: &[SessionEvent]) -> Option<usize> {
    events.iter().enumerate().rev().find_map(|(i, e)| match e {
        SessionEvent::User(u) if !is_hidden_user_text(&u.text) => Some(i),
        _ => None,
    })
}

fn plan_mode(events: &[SessionEvent], mode: Mode) -> Option<CompactEvent> {
    if events.is_empty() {
        return None;
    }
    let user = last_real_user_seq(events)?;
    let groups = groups(events);
    let mut evict_end: Option<usize> = None;

    let already = last_until(events) as usize;
    let closed_before: Vec<&Group> = groups.iter().filter(|g| g.closed && g.end < user).collect();
    let closed_after: Vec<&Group> = groups
        .iter()
        .filter(|g| g.closed && g.start > user && g.end > already)
        .collect();

    match mode {
        Mode::PreviousTurns => {
            evict_end = closed_before.last().map(|g| g.end);
        }
        Mode::KeepLastGroup => {
            if let Some(g) = closed_before.last() {
                evict_end = Some(g.end);
            }
            let drop_after = match closed_after.len() {
                0 => 0,
                1 => 1, // only shrink is to drop the sole tool round
                n => n - 1,
            };
            if drop_after > 0 {
                evict_end = Some(closed_after[drop_after - 1].end);
            }
        }
    }

    let until = evict_end?;
    if until == 0 {
        return None;
    }
    let prev = last_compact(events);
    let from = prev
        .map(|c| (c.until_seq as usize).saturating_add(1))
        .unwrap_or(1)
        .max(1);
    let (slice_summary, slice_index) = extract(events, from, until, user);
    let (summary, index) = match prev {
        Some(p) if !slice_has_evidence(events, from, until) => {
            // Fail-closed: keep the previous archive, refresh Open Work.
            (
                replace_open_work(&p.summary, open_work(events, from, until, user)),
                p.index.clone(),
            )
        }
        Some(p) => merge_extract(&p.summary, &p.index, &slice_summary, &slice_index),
        None => (slice_summary, slice_index),
    };
    if summary.trim().is_empty() && index.trim().is_empty() {
        return None;
    }
    let mut summary = summary;
    let task = live_task(events, user);
    if !task.is_empty() {
        summary = set_active_task(&summary, &task);
    }
    Some(CompactEvent {
        until_seq: until as u64,
        keep_user_seq: user as u64,
        summary,
        index,
    })
}

fn live_task(events: &[SessionEvent], keep_user: usize) -> String {
    match events.get(keep_user) {
        Some(SessionEvent::User(u)) if !is_hidden_user_text(&u.text) => clip(&u.text, CLIP * 2),
        _ => String::new(),
    }
}

fn set_active_task(summary: &str, task: &str) -> String {
    let mut sections = parse_sections(summary);
    upsert_section(&mut sections, "Active Task", task.trim().to_string());
    render_sections(&sections)
}

fn last_compact(events: &[SessionEvent]) -> Option<&CompactEvent> {
    events.iter().rev().find_map(|e| match e {
        SessionEvent::Compact(c) => Some(c),
        _ => None,
    })
}

fn slice_has_evidence(events: &[SessionEvent], from: usize, until: usize) -> bool {
    events
        .iter()
        .enumerate()
        .take(until.saturating_add(1))
        .skip(from)
        .any(|(_, e)| {
            matches!(
                e,
                SessionEvent::User(_) | SessionEvent::Assistant(_) | SessionEvent::Tool(_)
            )
        })
}

fn open_work(events: &[SessionEvent], from: usize, until: usize, keep_user: usize) -> String {
    let already = collapse_runs(collect_tool_states(events, from, until));
    format_open_work(&already, keep_user <= until)
}

fn replace_open_work(summary: &str, open: String) -> String {
    let mut sections = parse_sections(summary);
    upsert_section(
        &mut sections,
        "Open Work",
        if open.is_empty() {
            "(see live user)".into()
        } else {
            open
        },
    );
    render_sections(&sections)
}

fn merge_extract(prev_s: &str, prev_i: &str, new_s: &str, new_i: &str) -> (String, String) {
    let prev = parse_sections(prev_s);
    let new = parse_sections(new_s);
    let mut out = prev.clone();
    upsert_section(
        &mut out,
        "Active Task",
        pick_plain(
            section_get(&new, "Active Task"),
            section_get(&prev, "Active Task"),
        ),
    );
    upsert_section(
        &mut out,
        "Current State",
        merge_bullets(
            section_get(&prev, "Current State"),
            section_get(&new, "Current State"),
            12,
        ),
    );
    upsert_section(
        &mut out,
        PRIOR_USER,
        merge_bullets(
            prior_user_body(&prev),
            prior_user_body(&new),
            CONSTRAINT_CAP,
        ),
    );
    upsert_section(
        &mut out,
        "Decisions",
        merge_bullets(
            section_get(&prev, "Decisions"),
            section_get(&new, "Decisions"),
            8,
        ),
    );
    upsert_section(
        &mut out,
        "Open Work",
        pick_plain(
            section_get(&new, "Open Work"),
            section_get(&prev, "Open Work"),
        ),
    );
    (render_sections(&out), merge_index(prev_i, new_i))
}

fn upsert_section(sections: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some((_, body)) = sections.iter_mut().find(|(k, _)| k == name) {
        *body = value;
    } else {
        sections.push((name.to_string(), value));
    }
}

fn parse_sections(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut title = String::new();
    let mut body = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if !title.is_empty() {
                out.push((title.clone(), body.trim().to_string()));
            }
            title = rest.trim().to_string();
            body.clear();
        } else {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
        }
    }
    if !title.is_empty() {
        out.push((title, body.trim().to_string()));
    }
    out
}

fn section_get<'a>(sections: &'a [(String, String)], name: &str) -> Option<&'a str> {
    sections
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn pick_plain(new: Option<&str>, prev: Option<&str>) -> String {
    new.filter(|s| !s.trim().is_empty())
        .or(prev)
        .unwrap_or("(see live user)")
        .to_string()
}

fn prior_user_body(sections: &[(String, String)]) -> Option<&str> {
    section_get(sections, PRIOR_USER).or_else(|| section_get(sections, PRIOR_USER_LEGACY))
}

fn bullet_key(item: &str) -> String {
    item.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Short acknowledgements are not standing intent. Do not keyword-mine constraints.
fn is_ack_user(text: &str) -> bool {
    if text.contains('/') || text.contains('\\') || text.contains('`') {
        return false;
    }
    let key: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(c))
        .flat_map(|c| c.to_lowercase())
        .collect();
    matches!(
        key.as_str(),
        "好" | "好的"
            | "行"
            | "继续"
            | "嗯"
            | "ok"
            | "okay"
            | "yes"
            | "y"
            | "是"
            | "对"
            | "就这样"
            | "好就这样"
            | "继续吧"
            | "go"
    )
}

fn merge_bullets(prev: Option<&str>, new: Option<&str>, cap: usize) -> String {
    let mut bullets = Vec::new();
    for block in [prev, new].into_iter().flatten() {
        for line in block.lines() {
            let t = line.trim();
            let item = t.strip_prefix("- ").unwrap_or(t);
            if item.is_empty() || is_stub_section(item) {
                continue;
            }
            let key = bullet_key(item);
            if !bullets.iter().any(|b: &String| bullet_key(b) == key) {
                bullets.push(item.to_string());
            }
        }
    }
    if bullets.is_empty() {
        return "(none)".into();
    }
    let skip = bullets.len().saturating_sub(cap);
    bullets
        .into_iter()
        .skip(skip)
        .map(|b| format!("- {b}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn merge_index(prev: &str, new: &str) -> String {
    let mut lines: Vec<String> = prev
        .lines()
        .chain(new.lines())
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() > INDEX_LINES {
        lines = head_tail(lines, INDEX_LINES, INDEX_HEAD);
    }
    lines.join("\n")
}

fn is_stub_section(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || t == "(none)" || t == "(none extracted)" || t == "(see live user)"
}

fn render_sections(sections: &[(String, String)]) -> String {
    const ORDER: [&str; 5] = [
        "Active Task",
        "Current State",
        PRIOR_USER,
        "Decisions",
        "Open Work",
    ];
    let mut body = String::new();
    for name in ORDER {
        let val = section_get(sections, name).unwrap_or("(none)");
        // An empty Prior User heading is a false hint ("there are none").
        // Omit it; S1/S7 enforce test/spec constraints in the live turn.
        if name == PRIOR_USER && is_stub_section(val) {
            continue;
        }
        body.push_str("## ");
        body.push_str(name);
        body.push('\n');
        body.push_str(val.trim());
        body.push('\n');
        if name != "Open Work" {
            body.push('\n');
        }
    }
    body
}

fn groups(events: &[SessionEvent]) -> Vec<Group> {
    let mut out = Vec::new();
    let mut i = 1usize;
    while i < events.len() {
        match &events[i] {
            SessionEvent::User(_) => {
                out.push(Group {
                    start: i,
                    end: i,
                    closed: true,
                });
                i += 1;
            }
            SessionEvent::Assistant(a) => {
                let ids: Vec<String> = a
                    .tool_calls
                    .as_ref()
                    .map(|cs| cs.iter().map(|c| c.id.clone()).collect())
                    .unwrap_or_default();
                if ids.is_empty() {
                    out.push(Group {
                        start: i,
                        end: i,
                        closed: true,
                    });
                    i += 1;
                    continue;
                }
                let start = i;
                i += 1;
                let mut open = ids;
                let mut end = start;
                while i < events.len() && !open.is_empty() {
                    match &events[i] {
                        SessionEvent::Tool(t) => {
                            if let Some(pos) = open.iter().position(|id| id == &t.tool_call_id) {
                                open.remove(pos);
                                end = i;
                            } else {
                                break;
                            }
                            i += 1;
                        }
                        SessionEvent::Policy(_)
                        | SessionEvent::Fork(_)
                        | SessionEvent::Compact(_)
                        | SessionEvent::Stop(_)
                        | SessionEvent::Undo(_)
                        | SessionEvent::Start(_)
                        | SessionEvent::Delta(_) => {
                            i += 1;
                        }
                        SessionEvent::User(_) | SessionEvent::Assistant(_) => break,
                    }
                }
                out.push(Group {
                    start,
                    end,
                    closed: open.is_empty(),
                });
            }
            _ => i += 1,
        }
    }
    out
}

fn extract(
    events: &[SessionEvent],
    from: usize,
    until: usize,
    keep_user: usize,
) -> (String, String) {
    let task = live_task(events, keep_user);
    let mut tools: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut decisions: Vec<String> = Vec::new();
    let mut constraints: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut pending: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for (seq, event) in events.iter().enumerate().take(until + 1).skip(from) {
        match event {
            SessionEvent::User(u) if !is_hidden_user_text(&u.text) => {
                let clipped = clip(&u.text, CLIP);
                lines.push(format!("seq {seq}  user  {clipped}"));
                // Live user is Active Task. Earlier real turns are the sticky
                // channel as Prior User — no keyword mining.
                if seq != keep_user && !clipped.is_empty() && !is_ack_user(&clipped) {
                    constraints.push(clipped);
                }
            }
            SessionEvent::Assistant(a) => {
                if let Some(calls) = &a.tool_calls {
                    for c in calls {
                        let detail = call_detail(c);
                        pending.insert(c.id.clone(), detail.clone());
                        lines.push(format!("seq {seq}  assistant  {detail}"));
                    }
                }
                if !a.content.trim().is_empty() && a.tool_calls.is_none() {
                    let c = clip(&a.content, CLIP);
                    decisions.push(c.clone());
                    lines.push(format!("seq {seq}  assistant  {c}"));
                }
                // Full think stays out of FTS. A short note is only useful when
                // the turn has no user-facing text (those go to Decisions) and
                // the model did more than restate the user / announce the tool.
                if a.content.trim().is_empty() {
                    if let Some(note) = think_note(&a.reasoning, a.tool_calls.as_deref()) {
                        notes.push(format!("note: {note}"));
                    }
                }
            }
            SessionEvent::Tool(t) => {
                let label = pending
                    .remove(&t.tool_call_id)
                    .unwrap_or_else(|| t.name.clone());
                tools.push(tool_state_line(&label, t));
                lines.push(format!("seq {seq}  {}", tool_line(&label, t)));
            }
            _ => {}
        }
    }

    let already = collapse_runs(tools);
    let mut state = head_tail(already.clone(), STATE_CAP, STATE_HEAD);
    for note in take_last(notes, NOTE_CAP) {
        state.push(note);
    }

    let mut summary = String::from("## Active Task\n");
    summary.push_str(if task.is_empty() { "(none)" } else { &task });
    summary.push_str("\n\n## Current State\n");
    write_bullets(&mut summary, &state);
    let constraints = take_last(constraints, CONSTRAINT_CAP);
    if !constraints.is_empty() {
        summary.push_str("\n## Prior User\n");
        write_bullets(&mut summary, &constraints);
    }
    summary.push_str("\n## Decisions\n");
    write_bullets(&mut summary, &take_last(decisions, DECISION_CAP));
    summary.push_str("\n## Open Work\n");
    summary.push_str(&format_open_work(&already, keep_user <= until));
    summary.push('\n');

    (
        summary,
        head_tail(lines, INDEX_LINES, INDEX_HEAD).join("\n"),
    )
}

fn collect_tool_states(events: &[SessionEvent], from: usize, until: usize) -> Vec<String> {
    let mut pending: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut tools = Vec::new();
    for event in events.iter().take(until + 1).skip(from) {
        match event {
            SessionEvent::Assistant(a) => {
                if let Some(calls) = &a.tool_calls {
                    for c in calls {
                        pending.insert(c.id.clone(), call_detail(c));
                    }
                }
            }
            SessionEvent::Tool(t) => {
                let label = pending
                    .remove(&t.tool_call_id)
                    .unwrap_or_else(|| t.name.clone());
                tools.push(tool_state_line(&label, t));
            }
            _ => {}
        }
    }
    tools
}

fn format_open_work(already: &[String], intra_turn: bool) -> String {
    if already.is_empty() {
        return "(see live user)".into();
    }
    let heads: Vec<String> = take_last(already.to_vec(), 6)
        .into_iter()
        .map(|line| {
            line.split(" → ")
                .next()
                .unwrap_or(line.as_str())
                .to_string()
        })
        .collect();
    let mut items = vec![format!("already: {}", clip(&heads.join("; "), CLIP * 2))];
    if !intra_turn {
        items.push("(see live user)".into());
    }
    items
        .into_iter()
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_bullets(out: &mut String, items: &[String]) {
    if items.is_empty() {
        out.push_str("(none)\n");
        return;
    }
    for s in items {
        out.push_str("- ");
        out.push_str(s);
        out.push('\n');
    }
}

fn collapse_runs(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for s in items {
        if let Some((last, n)) = out.last_mut() {
            if *last == s {
                *n += 1;
                continue;
            }
        }
        out.push((s, 1));
    }
    out.into_iter()
        .map(|(s, n)| if n > 1 { format!("{s} (×{n})") } else { s })
        .collect()
}

fn head_tail(items: Vec<String>, cap: usize, head: usize) -> Vec<String> {
    if items.len() <= cap {
        return items;
    }
    let head = head.min(cap);
    let tail = cap.saturating_sub(head);
    let mut out = items[..head].to_vec();
    if tail > 0 {
        out.extend(items[items.len() - tail..].iter().cloned());
    }
    out
}

fn take_last<T>(items: Vec<T>, n: usize) -> Vec<T> {
    let skip = items.len().saturating_sub(n);
    items.into_iter().skip(skip).collect()
}

fn call_detail(c: &OpenAiToolCall) -> String {
    let args: serde_json::Value =
        serde_json::from_str(&c.function.arguments).unwrap_or(serde_json::Value::Null);
    let hint = args
        .get("path")
        .or_else(|| args.get("file_path"))
        .or_else(|| args.get("command"))
        .or_else(|| args.get("query"))
        .or_else(|| args.get("code"))
        .and_then(|v| v.as_str())
        .map(|s| clip(s, 60));
    match hint {
        Some(h) => format!("{} {h}", c.function.name),
        None => c.function.name.clone(),
    }
}

fn think_note(reasoning: &str, calls: Option<&[OpenAiToolCall]>) -> Option<String> {
    let t = reasoning.trim();
    if t.is_empty() || is_task_restatement(t) || is_tool_announcement(t, calls) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // Stale next-step after compact: the tool line already records what ran.
    if lower.contains("now i need to") || lower.contains("now i have to") {
        return None;
    }
    Some(clip(t, 80))
}

fn is_task_restatement(s: &str) -> bool {
    let t = s.trim().to_ascii_lowercase();
    t.starts_with("the user ")
        || t.starts_with("the task ")
        || t.starts_with("user wants")
        || t.starts_with("user asked")
        || t.starts_with("ok, ")
        || t.starts_with("okay, ")
        || t.starts_with("ok the ")
}

/// "I'll read X" / "Now I need to run bash" with no extra plan is noise
/// next to the tool line itself.
fn is_tool_announcement(think: &str, calls: Option<&[OpenAiToolCall]>) -> bool {
    let Some(calls) = calls.filter(|c| !c.is_empty()) else {
        return false;
    };
    let t = think.trim().to_ascii_lowercase();
    let announces = t.starts_with("i'll ")
        || t.starts_with("i will ")
        || t.starts_with("let me ")
        || t.starts_with("i need to ")
        || t.starts_with("now i ")
        || t.starts_with("next i ");
    if !announces || think.chars().count() >= 160 {
        return false;
    }
    let extra = t.contains("because")
        || t.contains(" instead")
        || t.contains(" after ")
        || t.contains(" then ");
    if extra {
        return false;
    }
    calls.iter().any(|c| {
        let name = c.function.name.to_ascii_lowercase();
        t.contains(&name)
    })
}

fn media_bits(t: &ToolEvent) -> String {
    if t.media.is_empty() {
        return String::new();
    }
    let urls: Vec<&str> = t.media.iter().map(|m| m.url.as_str()).collect();
    format!(" media={}", urls.join(","))
}

fn tool_state_line(label: &str, t: &ToolEvent) -> String {
    let snippet = clip(&t.output, 72);
    let media = media_bits(t);
    match (&t.blob, t.original_chars) {
        (Some(blob), Some(n)) => {
            format!("{label} → {snippet} blob={blob} chars={n}{media}")
        }
        (Some(blob), None) => format!("{label} → {snippet} blob={blob}{media}"),
        (None, Some(n)) if n > 200 => format!("{label} → {snippet} chars={n}{media}"),
        _ if !media.is_empty() => format!("{label} → {snippet}{media}"),
        _ => format!("{label} → {snippet}"),
    }
}

fn tool_line(label: &str, t: &ToolEvent) -> String {
    let mut s = format!("tool {label} ");
    if let Some(blob) = &t.blob {
        s.push_str("blob=");
        s.push_str(blob);
        s.push(' ');
    }
    if let Some(n) = t.original_chars {
        s.push_str(&format!("chars={n} "));
    }
    s.push_str(&clip(&t.output, 48));
    s.push_str(&media_bits(t));
    s
}

fn clip(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    clip_chars(&flat, n)
}

fn clip_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let take = n.saturating_sub(1);
    let head: String = s.chars().take(take).collect();
    format!("{head}…")
}

fn fit_tail(s: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if s.chars().count() <= budget {
        return s.to_string();
    }
    let mut lines: Vec<&str> = s.lines().collect();
    while lines.len() > 1 {
        lines.remove(0);
        if lines.join("\n").chars().count() <= budget {
            return lines.join("\n");
        }
    }
    clip_chars(&lines.join("\n"), budget)
}

fn events_from_messages(messages: &[ChatMessage]) -> Vec<SessionEvent> {
    use crate::policy::ThinkPolicy;
    use crate::session::event::SessionStart;
    use crate::session::SessionMode;

    let system = messages
        .first()
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    let mut events = vec![SessionEvent::Start(SessionStart::new(
        "live",
        "",
        SessionMode::Agent,
        system,
        "",
        ThinkPolicy::agent_default(),
    ))];
    for msg in messages.iter().skip(1) {
        match msg.role.as_str() {
            "user" => {
                let text = msg.content.clone().unwrap_or_default();
                // 无日志路径：上一份 [archived] 归档卡还原成 Compact 事件，
                // 二次压缩才有 prev_until 单调保护，旧归档正文并入新 summary
                // 而不是被当普通消息驱逐吞掉。
                if let Some(c) = archive_compact_event(&text, events.len() as u64) {
                    events.push(SessionEvent::Compact(c));
                    continue;
                }
                let mut ev = SessionEvent::user(text);
                if !msg.parts.is_empty() {
                    ev = ev.with_media(
                        msg.parts
                            .iter()
                            .map(|p| crate::session::event::StoredMedia {
                                kind: p.kind.as_str().into(),
                                mime: p.mime.clone(),
                                url: p.url.clone(),
                            })
                            .collect(),
                    );
                }
                events.push(ev);
            }
            "assistant" => {
                let content = msg.content.clone().unwrap_or_default();
                let reasoning = msg.reasoning_content.clone().unwrap_or_default();
                let calls = msg
                    .tool_calls
                    .as_ref()
                    .map(|cs| cs.iter().map(value_to_stored).collect::<Vec<_>>());
                events.push(SessionEvent::assistant(content, reasoning, calls));
            }
            "tool" => {
                let mut ev = SessionEvent::tool(
                    msg.tool_call_id.clone().unwrap_or_default(),
                    msg.name.clone().unwrap_or_default(),
                    msg.content.clone().unwrap_or_default(),
                );
                if !msg.parts.is_empty() {
                    ev = ev.with_media(
                        msg.parts
                            .iter()
                            .map(|p| crate::session::event::StoredMedia {
                                kind: p.kind.as_str().into(),
                                mime: p.mime.clone(),
                                url: p.url.clone(),
                            })
                            .collect(),
                    );
                }
                events.push(ev);
            }
            _ => {}
        }
    }
    events
}

/// 反解无日志路径自己写出的归档卡（`archive_body` 的逆）：summary 与 index
/// 拆回 CompactEvent，`until_seq` 指向归档卡自身的消息位。footer 与
/// "## Index" 标题只是排版，丢弃。
fn archive_compact_event(text: &str, seq: u64) -> Option<CompactEvent> {
    if !is_hidden_user_text(text) {
        return None;
    }
    let t = text.trim();
    let inner = t
        .strip_prefix("<tool_response>")
        .and_then(|s| s.strip_suffix("</tool_response>"))
        .map(str::trim)
        .unwrap_or(t);
    let body = inner.strip_prefix("[archived]")?;
    let body = match body.find("═══") {
        Some(i) => &body[..i],
        None => body,
    };
    let (summary, index) = match body.find("\n## Index\n") {
        Some(i) => (&body[..i], body[i + "\n## Index\n".len()..].trim()),
        None => (body, ""),
    };
    Some(CompactEvent {
        until_seq: seq,
        keep_user_seq: seq,
        summary: summary.trim().to_string(),
        index: index.to_string(),
    })
}

fn value_to_stored(v: &serde_json::Value) -> OpenAiToolCall {
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let func = v
        .get("function")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let name = func
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let arguments = match func.get("arguments") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "{}".into(),
    };
    OpenAiToolCall::function(id, name, arguments)
}

#[cfg(test)]
mod tests {
    use super::super::tools_hash;
    use super::*;
    use crate::policy::ThinkPolicy;
    use crate::session::event::{SessionMode, SessionStart};
    use crate::template::is_hidden_user_text;

    fn start() -> SessionEvent {
        SessionEvent::Start(SessionStart::new(
            "s",
            "/tmp",
            SessionMode::Agent,
            "sys",
            tools_hash(&[]),
            ThinkPolicy::agent_default(),
        ))
    }

    fn read_call(id: &str, path: &str) -> OpenAiToolCall {
        OpenAiToolCall::function(id, "read", format!(r#"{{"path":"{path}"}}"#))
    }

    fn bash_call(id: &str, cmd: &str) -> OpenAiToolCall {
        OpenAiToolCall::function(id, "bash", format!(r#"{{"command":"{cmd}"}}"#))
    }

    #[test]
    fn previous_turn_evicted_last_user_kept() {
        let events = vec![
            start(),
            SessionEvent::user("fix prefix cache"),
            SessionEvent::assistant("ok", "secret zirconium", None),
            SessionEvent::user("now edit bash.rs"),
        ];
        let plan = plan_compact(&events).expect("compact");
        assert_eq!(plan.until_seq, 2);
        assert_eq!(plan.keep_user_seq, 3);
        assert!(plan.index.contains("seq 1"));
        assert!(!plan.summary.contains("zirconium"));
        assert!(!plan.index.contains("zirconium"));
        assert!(is_hidden_user_text(&plan.archive_user_text()));
        assert!(plan.archive_body().contains("[archived]"));
    }

    #[test]
    fn follow_up_user_archives_finished_turn() {
        let events = vec![
            start(),
            SessionEvent::user("160k audit of node_modules"),
            SessionEvent::assistant("findings: lots of files", "", None),
            SessionEvent::stop("stop"),
            SessionEvent::user("ok next"),
        ];
        let plan = plan_compact(&events).expect("previous turns");
        assert_eq!(plan.keep_user_seq, 4);
        assert!(
            plan.until_seq >= 2,
            "finished assistant must leave the live window: {plan:?}"
        );
        assert!(plan.summary.contains("ok next") || plan.index.contains("160k audit"));
    }

    #[test]
    fn tool_outcomes_land_in_current_state() {
        let events = vec![
            start(),
            SessionEvent::user("what is the token"),
            SessionEvent::assistant("", "", Some(vec![read_call("c1", "fact.txt")])),
            SessionEvent::tool("c1", "read", "     1|obsidian-compact"),
            SessionEvent::assistant("", "", Some(vec![read_call("c2", "other.rs")])),
            SessionEvent::tool("c2", "read", "fn other"),
        ];
        let plan = plan_compact(&events).expect("intra-turn");
        assert!(
            plan.summary.contains("obsidian-compact"),
            "folded-away tools must leave a readable clip: {}",
            plan.summary
        );
        assert!(
            plan.index.contains("obsidian-compact") || plan.index.contains("fact.txt"),
            "{}",
            plan.index
        );
    }

    #[test]
    fn useful_think_becomes_a_note_restatement_does_not() {
        let events = vec![
            start(),
            SessionEvent::user("fix the linker"),
            SessionEvent::assistant(
                "",
                "switch to edit after reading linker.rs",
                Some(vec![read_call("a", "linker.rs")]),
            ),
            SessionEvent::tool("a", "read", "fn linker"),
            SessionEvent::user("now edit"),
        ];
        let plan = plan_compact(&events).expect("prev turn");
        assert!(
            plan.summary.contains("switch to edit"),
            "non-restatement think should clip into Current State: {}",
            plan.summary
        );

        let noisy = vec![
            start(),
            SessionEvent::user("read note"),
            SessionEvent::assistant(
                "",
                "The user wants me to read note.txt",
                Some(vec![read_call("b", "note.txt")]),
            ),
            SessionEvent::tool("b", "read", "     1|hello-from-q38-live"),
            SessionEvent::user("next"),
        ];
        let plan = plan_compact(&noisy).expect("noisy");
        assert!(
            !plan.summary.to_ascii_lowercase().contains("the user wants"),
            "{}",
            plan.summary
        );
        assert!(
            plan.summary.contains("hello-from-q38-live"),
            "{}",
            plan.summary
        );
    }

    #[test]
    fn extract_keeps_early_fact_collapses_repeats_and_clips_blobs() {
        let mut events = vec![
            start(),
            SessionEvent::user("what is the token"),
            SessionEvent::assistant("", "", Some(vec![read_call("c0", "fact.txt")])),
            SessionEvent::tool("c0", "read", "     1|obsidian-compact"),
        ];
        for i in 1..=12 {
            let id = format!("f{i}");
            events.push(SessionEvent::assistant(
                "",
                "I'll read filler.txt",
                Some(vec![read_call(&id, "filler.txt")]),
            ));
            events.push(SessionEvent::tool(
                &id,
                "read",
                "FILLER_LINE_FOR_PREFIX_PRESSURE",
            ));
        }
        events.push(SessionEvent::assistant(
            "",
            "",
            Some(vec![read_call("z", "z.rs")]),
        ));
        events.push(SessionEvent::tool_folded(
            "z",
            "read",
            "head of z.rs\n…[omitted]…\ntail",
            Some("ab".repeat(32)),
            Some(80_000),
        ));
        events.push(SessionEvent::user("now continue"));
        let plan = plan_compact(&events).expect("prev turn");
        assert!(
            plan.summary.contains("obsidian-compact"),
            "early fact must survive head+tail: {}",
            plan.summary
        );
        assert!(
            plan.summary.contains("fact.txt"),
            "Current State should name the path: {}",
            plan.summary
        );
        assert!(
            plan.summary.contains("×") || plan.summary.matches("filler.txt").count() < 12,
            "repeated filler reads should collapse: {}",
            plan.summary
        );
        assert!(
            !plan.summary.contains("I'll read filler"),
            "tool announcements must not become notes: {}",
            plan.summary
        );
        assert!(
            plan.summary.contains("z.rs") && plan.summary.contains("head of z.rs"),
            "folded blob must keep a readable clip, not only the sha: {}",
            plan.summary
        );
        assert!(
            plan.index.contains("obsidian-compact") || plan.index.contains("fact.txt"),
            "{}",
            plan.index
        );
        let sections = parse_sections(&plan.summary);
        let ow = section_get(&sections, "Open Work").unwrap_or("");
        assert!(
            ow.contains("already:"),
            "Open Work must be residual, not the user prompt: {ow}"
        );
        assert!(
            !ow.contains("what is the token"),
            "Open Work copied the user prompt: {ow}"
        );
    }

    #[test]
    fn open_work_intra_turn_does_not_repeat_the_prompt() {
        let events = vec![
            start(),
            SessionEvent::user(
                "Read fact.txt first. Then run bash three times. Then reply with the token.",
            ),
            SessionEvent::assistant("", "", Some(vec![read_call("c1", "fact.txt")])),
            SessionEvent::tool("c1", "read", "     1|obsidian-compact"),
            SessionEvent::assistant("", "", Some(vec![bash_call("b1", "echo WWW")])),
            SessionEvent::tool("b1", "bash", "WWWWWWWWWW"),
        ];
        let plan = plan_compact(&events).expect("intra-turn");
        let sections = parse_sections(&plan.summary);
        let ow = section_get(&sections, "Open Work").unwrap_or("");
        assert!(
            ow.contains("already:") && !ow.contains("Read fact.txt first"),
            "{ow}"
        );
        assert!(
            !ow.contains("Then run bash three times"),
            "Open Work must not restate the recipe: {ow}"
        );
        assert!(plan.summary.contains("obsidian-compact"));
        assert!(
            !plan.summary.contains("## Prior User"),
            "empty Prior User is a false hint: {}",
            plan.summary
        );
        assert!(
            !plan.summary.contains("none extracted"),
            "must not tell the model there are no constraints: {}",
            plan.summary
        );
    }

    #[test]
    fn merge_omits_stub_constraints() {
        let (merged, _) = merge_extract(
            "## Active Task\na\n\n## Current State\n- x\n\n## Constraints\n(none extracted)\n\n## Decisions\n(none)\n\n## Open Work\nlive\n",
            "idx-a",
            "## Active Task\nb\n\n## Current State\n- y\n\n## Decisions\n- chose b\n\n## Open Work\nlive\n",
            "idx-b",
        );
        assert!(
            !merged.contains("## Constraints") && !merged.contains("## Prior User"),
            "stub Prior User survived merge: {merged}"
        );
        assert!(merged.contains("chose b"));
    }

    #[test]
    fn restart_think_and_ok_token_notes_are_dropped() {
        let events = vec![
            start(),
            SessionEvent::user("what is the token"),
            SessionEvent::assistant(
                "",
                "OK, the token is obsidian-compact. Now I need to run bash three times.",
                Some(vec![bash_call("b1", "echo WWW")]),
            ),
            SessionEvent::tool("b1", "bash", "WWWWWWWWWW"),
            SessionEvent::assistant(
                "",
                "Now I need to run the bash command three times",
                Some(vec![bash_call("b2", "echo WWW")]),
            ),
            SessionEvent::tool("b2", "bash", "WWWWWWWWWW"),
            SessionEvent::user("continue"),
        ];
        let plan = plan_compact(&events).expect("prev turn");
        let lower = plan.summary.to_ascii_lowercase();
        assert!(
            !lower.contains("now i need") && !lower.contains("ok, the token"),
            "restart think must not coach a redo: {}",
            plan.summary
        );
        assert!(
            plan.summary.contains("WWWW"),
            "bash clip must remain: {}",
            plan.summary
        );
    }

    #[test]
    fn parallel_tool_round_stays_one_group() {
        let events = vec![
            start(),
            SessionEvent::user("read both"),
            SessionEvent::assistant(
                "",
                "",
                Some(vec![read_call("a", "a.txt"), read_call("b", "b.txt")]),
            ),
            SessionEvent::tool("a", "read", "     1|alpha"),
            SessionEvent::tool("b", "read", "     1|bravo"),
            SessionEvent::assistant("", "", Some(vec![read_call("c", "c.txt")])),
            SessionEvent::tool("c", "read", "     1|charlie"),
        ];
        let plan = plan_compact(&events).expect("keep last group");
        assert!(
            plan.until_seq < 6,
            "must evict the parallel pair as one group, not split it: {plan:?}"
        );
        assert!(
            plan.summary.contains("alpha") && plan.summary.contains("a.txt"),
            "parallel results must both land in Current State: {}",
            plan.summary
        );
        assert!(
            plan.summary.contains("bravo") && plan.summary.contains("b.txt"),
            "{}",
            plan.summary
        );
    }

    #[test]
    fn archive_keeps_footer_when_index_is_fat() {
        let plan = CompactEvent {
            until_seq: 9,
            keep_user_seq: 1,
            summary: "## Active Task\nkeep me\n\n## Current State\n- read fact.txt → obsidian-compact\n\n## Constraints\n(none)\n\n## Decisions\n(none)\n\n## Open Work\nlive".into(),
            index: (0..200)
                .map(|i| format!("seq {i}  tool bash {}", "W".repeat(40)))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let body = plan.archive_body();
        assert!(
            body.contains("END OF ARCHIVED INDEX"),
            "footer must survive index clip: {}",
            body.chars()
                .skip(body.chars().count().saturating_sub(200))
                .collect::<String>()
        );
        assert!(body.contains("obsidian-compact"));
        assert!(body.chars().count() <= ARCHIVE_CHARS);
        assert!(is_hidden_user_text(&plan.archive_user_text()));
    }

    #[test]
    fn does_not_split_assistant_from_tool_results() {
        let events = vec![
            start(),
            SessionEvent::user("read both"),
            SessionEvent::assistant(
                "",
                "",
                Some(vec![read_call("a", "a.rs"), read_call("b", "b.rs")]),
            ),
            SessionEvent::tool("a", "read", "fn a"),
            // missing b — unclosed; nothing before the user to evict except empty
            SessionEvent::user("continue"),
        ];
        // unclosed pair is after last real user? last user is "continue" at 5.
        // group assistant+tool a is unclosed (b missing), so PreviousTurns has
        // no closed group ending before user 5 except user "read both".
        let plan = plan_compact(&events).expect("can still evict the first user");
        assert!(
            plan.until_seq < 2,
            "must not cut inside the tool pair: {plan:?}"
        );
        assert_eq!(plan.until_seq, 1);
    }

    #[test]
    fn intra_turn_drops_older_closed_tool_round() {
        let events = vec![
            start(),
            SessionEvent::user("task"),
            SessionEvent::assistant("", "", Some(vec![read_call("c1", "old.rs")])),
            SessionEvent::tool_folded("c1", "read", "old", Some("aa".repeat(32)), Some(80_000)),
            SessionEvent::assistant("", "", Some(vec![read_call("c2", "new.rs")])),
            SessionEvent::tool("c2", "read", "new"),
        ];
        // no previous turn; KeepLastGroup evicts the first tool round
        let plan = plan_compact(&events).expect("intra-turn");
        assert_eq!(plan.until_seq, 3);
        assert_eq!(plan.keep_user_seq, 1);
        assert!(plan.index.contains("old.rs") || plan.summary.contains("old.rs"));
    }

    #[test]
    fn hidden_continue_is_not_the_real_user() {
        let events = vec![
            start(),
            SessionEvent::user("real task"),
            SessionEvent::assistant("working", "", None),
            SessionEvent::user(wrap_tool_response("Continue working on the task.")),
        ];
        let plan = plan_compact(&events);
        // last real user is seq 1; evict nothing before it except... assistant is after.
        // PreviousTurns: closed groups before seq 1 = none. KeepLastGroup: closed
        // groups after user: assistant seq 2, and hidden user seq 3.
        assert!(plan.is_some());
        let plan = plan.unwrap();
        assert_eq!(plan.keep_user_seq, 1);
        assert!(plan.until_seq >= 2);
    }

    #[test]
    fn empty_transcript_does_not_compact() {
        let events = vec![start(), SessionEvent::user("hi")];
        assert!(plan_compact(&events).is_none());
    }

    #[test]
    fn active_task_tracks_the_live_user_not_the_first() {
        let events = vec![
            start(),
            SessionEvent::user("刚加班，别用工具，陪两句。"),
            SessionEvent::assistant("得嘞，客官。", "", None),
            SessionEvent::user("read pads/p01.txt then stop"),
            SessionEvent::assistant("", "", Some(vec![read_call("r", "pads/p01.txt")])),
            SessionEvent::tool("r", "read", "pad-01 line"),
        ];
        let plan = plan_compact(&events).expect("compact");
        assert!(
            plan.summary.contains("read pads/p01.txt"),
            "Active Task should be the live user: {}",
            plan.summary
        );
        let sections = parse_sections(&plan.summary);
        let task = section_get(&sections, "Active Task").unwrap_or("");
        assert!(
            !task.contains("别用工具"),
            "stale first-turn Active Task hijacks later work: {}",
            plan.summary
        );
        let constraints = section_get(&sections, "Prior User").unwrap_or("");
        assert!(
            constraints.contains("别用工具"),
            "evicted user text must stick in Prior User: {}",
            plan.summary
        );
        assert!(
            !constraints.contains("read pads/p01.txt"),
            "live user must not duplicate into Prior User: {}",
            plan.summary
        );
    }

    #[test]
    fn hidden_user_is_not_a_constraint() {
        let events = vec![
            start(),
            SessionEvent::user("real task"),
            SessionEvent::assistant("working", "", None),
            SessionEvent::user(wrap_tool_response("Continue working on the task.")),
            SessionEvent::user("next file"),
        ];
        let plan = plan_compact(&events).expect("compact");
        let sections = parse_sections(&plan.summary);
        let constraints = section_get(&sections, "Prior User").unwrap_or("");
        assert!(
            constraints.contains("real task"),
            "prior real user should stick: {}",
            plan.summary
        );
        assert!(
            !constraints
                .to_ascii_lowercase()
                .contains("continue working"),
            "hidden continue must not become Prior User: {}",
            plan.summary
        );
    }

    #[test]
    fn ack_user_is_not_prior_user() {
        let events = vec![
            start(),
            SessionEvent::user("好，就这样"),
            SessionEvent::assistant("嗯。", "", None),
            SessionEvent::user("read pads/p01.txt then stop"),
            SessionEvent::assistant("", "", Some(vec![read_call("r", "pads/p01.txt")])),
            SessionEvent::tool("r", "read", "pad-01 line"),
        ];
        let plan = plan_compact(&events).expect("compact");
        assert!(
            !plan.summary.contains("就这样") && !plan.summary.contains("## Prior User"),
            "ack must not become Prior User: {}",
            plan.summary
        );
    }

    #[test]
    fn constraints_merge_appends_and_drops_oldest() {
        let prev = "## Active Task\na\n\n## Current State\n- x\n\n## Constraints\n- keep-zh\n- old-eval\n\n## Decisions\n(none)\n\n## Open Work\nlive\n";
        let new = "## Active Task\nb\n\n## Current State\n- y\n\n## Constraints\n- new-path\n\n## Decisions\n(none)\n\n## Open Work\nlive\n";
        let (merged, _) = merge_extract(prev, "i1", new, "i2");
        let sections = parse_sections(&merged);
        let constraints = section_get(&sections, "Prior User").unwrap_or("");
        assert!(
            constraints.contains("keep-zh") && constraints.contains("new-path"),
            "merge must append, not freeze: {merged}"
        );
        let many_prev = format!(
            "## Active Task\na\n\n## Current State\n- x\n\n## Constraints\n{}\n\n## Decisions\n(none)\n\n## Open Work\nlive\n",
            (0..CONSTRAINT_CAP)
                .map(|i| format!("- c{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let (capped, _) = merge_extract(&many_prev, "i1", new, "i2");
        let capped_sections = parse_sections(&capped);
        let body = section_get(&capped_sections, "Prior User").unwrap_or("");
        assert!(
            !body.contains("- c0"),
            "cap should drop the oldest constraint: {capped}"
        );
        assert!(
            body.contains("new-path") && body.contains(&format!("c{}", CONSTRAINT_CAP - 1)),
            "newest prev + new slice should remain: {capped}"
        );
    }

    #[test]
    fn apply_reinjects_user_when_until_covers_them() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("task"),
            ChatMessage::assistant("old"),
            ChatMessage::user("task"), // won't happen; use apply with keep<=until
        ];
        let plan = CompactEvent {
            until_seq: 2,
            keep_user_seq: 1,
            summary: "## Active Task\ntask".into(),
            index: "seq 2  assistant  old".into(),
        };
        let live = apply_compact(&msgs, &plan);
        assert_eq!(live[0].role, "system");
        assert!(is_hidden_user_text(live[1].content.as_deref().unwrap()));
        assert_eq!(live[2].content.as_deref(), Some("task"));
        assert!(!live.iter().any(|m| m.content.as_deref() == Some("old")));
    }

    #[test]
    fn second_compact_merges_instead_of_rewalking_from_seq_1() {
        let mut events = vec![
            start(),
            SessionEvent::user("fix prefix cache"),
            SessionEvent::assistant("", "", Some(vec![read_call("a", "a.rs")])),
            SessionEvent::tool("a", "read", "fn a"),
            SessionEvent::user("now edit bash.rs"),
        ];
        let c1 = plan_compact(&events).expect("c1");
        assert!(
            c1.summary.contains("now edit bash.rs"),
            "c1 Active Task is the live user: {}",
            c1.summary
        );
        assert!(
            c1.index.contains("fix prefix cache"),
            "c1 index keeps the evicted prompt: {}",
            c1.index
        );
        events.push(SessionEvent::compact(c1.clone()));
        events.push(SessionEvent::assistant(
            "",
            "",
            Some(vec![read_call("b", "bash.rs")]),
        ));
        events.push(SessionEvent::tool("b", "read", "fn bash"));
        events.push(SessionEvent::user("keep going"));
        let c2 = plan_compact(&events).expect("c2");
        assert!(c2.until_seq > c1.until_seq);
        assert!(
            c2.summary.contains("keep going"),
            "Active Task must track the live user, not the first prompt: {}",
            c2.summary
        );
        assert!(
            c2.index.contains("fix prefix cache") || c2.summary.contains("fix prefix cache"),
            "old task must remain in archive/index: {}",
            c2.summary
        );
        assert!(
            c2.summary.contains("bash.rs") || c2.index.contains("bash.rs"),
            "new slice must land in archive: {}",
            c2.summary
        );
        assert!(c2.archive_body().chars().count() <= ARCHIVE_CHARS);
    }

    #[test]
    fn logless_second_compact_keeps_previous_archive() {
        // 无日志路径（persist_session=false）：第一份 [archived] 卡在
        // 二次压缩时必须还原成 Compact 事件并入新归档，不能被当普通
        // 消息驱逐吞掉。
        fn read_call_json(id: &str, path: &str) -> serde_json::Value {
            serde_json::json!({
                "id": id,
                "function": {"name": "read", "arguments": format!(r#"{{"path":"{path}"}}"#)},
            })
        }
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("fix prefix cache"),
            ChatMessage::assistant_tools(None, vec![read_call_json("a", "a.rs")]),
            ChatMessage::tool("a", "fn a"),
            ChatMessage::user("now edit bash.rs"),
        ];
        let (c1, mut live) = compact_messages(&msgs).expect("c1");
        assert!(c1.index.contains("fix prefix cache"));

        // 归档卡要能反解为 Compact 事件，prev_until 才有单调保护。
        let events = events_from_messages(&live);
        let c1_back = match &events[1] {
            SessionEvent::Compact(c) => c,
            other => panic!("archive card must round-trip to Compact, got {other:?}"),
        };
        assert!(
            c1_back.summary.contains("bash.rs") || c1_back.index.contains("fix prefix cache"),
            "round-trip lost archive content: {c1_back:?}"
        );

        live.push(ChatMessage::assistant_tools(
            None,
            vec![read_call_json("b", "bash.rs")],
        ));
        live.push(ChatMessage::tool("b", "fn bash"));
        live.push(ChatMessage::user("keep going"));
        let (c2, live2) = compact_messages(&live).expect("c2");
        assert!(
            c2.summary.contains("fix prefix cache") || c2.index.contains("fix prefix cache"),
            "second compact dropped previous archive: summary={} index={}",
            c2.summary,
            c2.index
        );
        let cards = live2
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|t| t.contains("[archived]"))
            })
            .count();
        assert_eq!(cards, 1, "exactly one archive card after re-compact");
    }

    #[test]
    fn events_from_messages_keeps_user_media() {
        // 无日志二次压缩重建 transcript 时用户附图不能丢。
        let mut user = ChatMessage::user("look at this");
        user.parts = vec![crate::media::MediaPart {
            kind: crate::media::MediaKind::Image,
            mime: "image/png".into(),
            url: "data:image/png;base64,xx".into(),
        }];
        let msgs = vec![ChatMessage::system("sys"), user];
        let events = events_from_messages(&msgs);
        match &events[1] {
            SessionEvent::User(u) => {
                assert_eq!(u.media.len(), 1);
                assert_eq!(u.media[0].url, "data:image/png;base64,xx");
            }
            other => panic!("expected user event, got {other:?}"),
        }
    }

    #[test]
    fn second_intra_turn_compact_evicts_the_new_round() {
        let mut events = vec![
            start(),
            SessionEvent::user("what is the token"),
            SessionEvent::assistant("", "", Some(vec![read_call("c1", "fact.txt")])),
            SessionEvent::tool("c1", "read", "     1|obsidian-compact"),
        ];
        let c1 = plan_compact(&events).expect("first intra-turn drops the sole round");
        events.push(SessionEvent::compact(c1.clone()));
        events.push(SessionEvent::assistant(
            "",
            "",
            Some(vec![read_call("c3", "fat.rs")]),
        ));
        events.push(SessionEvent::tool_folded(
            "c3",
            "read",
            "WWWWWWWWWW",
            Some("cd".repeat(32)),
            Some(8_000),
        ));
        let c2 = plan_compact(&events).expect("must evict the new round, not re-evict c1");
        assert!(
            c2.until_seq > c1.until_seq,
            "second compact stuck at until={} after {}",
            c2.until_seq,
            c1.until_seq
        );
        assert!(
            c2.summary.contains("obsidian-compact"),
            "merged archive dropped the fact: {}",
            c2.summary
        );
        assert!(
            c2.summary.contains("fat.rs") || c2.summary.contains("WWWW"),
            "new round must leave a clip: {}",
            c2.summary
        );
    }

    #[test]
    fn thirty_compacts_keep_live_window_bounded() {
        let mut events = vec![start()];
        let mut n = 0u32;
        let mut last_live = 0usize;
        for i in 1..=45 {
            events.push(SessionEvent::user(format!("task-{i} edit file_{i}.rs")));
            events.push(SessionEvent::assistant(
                "",
                "",
                Some(vec![read_call(&format!("c{i}"), &format!("file_{i}.rs"))]),
            ));
            events.push(SessionEvent::tool(
                format!("c{i}"),
                "read",
                format!("fn f{i}() {{ /* {} */ }}", "x".repeat(120)),
            ));
            if let Some(plan) = plan_compact(&events) {
                assert!(
                    plan.archive_body().chars().count() <= ARCHIVE_CHARS,
                    "archive grew past clip at compact {n}"
                );
                assert!(is_hidden_user_text(&plan.archive_user_text()));
                events.push(SessionEvent::compact(plan));
                n += 1;
                let msgs = crate::session::derive_messages(&events);
                let real_users = msgs
                    .iter()
                    .filter(|m| {
                        m.role == "user" && !is_hidden_user_text(m.content.as_deref().unwrap_or(""))
                    })
                    .count();
                assert_eq!(real_users, 1, "compact {n} lost last_query_index");
                assert!(
                    msgs.iter().any(|m| {
                        m.role == "user" && is_hidden_user_text(m.content.as_deref().unwrap_or(""))
                    }),
                    "compact {n} missing wrapped archive"
                );
                assert!(
                    msgs.len() <= 12,
                    "live window bloated to {} after {n} compacts",
                    msgs.len()
                );
                last_live = msgs.len();
            }
        }
        assert!(n >= 30, "only {n} compact cycles; live={last_live}");
        let last = events.iter().rev().find_map(|e| match e {
            SessionEvent::Compact(c) => Some(c),
            _ => None,
        });
        let last = last.expect("last compact");
        assert!(
            last.summary.contains("task-1")
                || last.summary.contains("file_1.rs")
                || last.index.contains("file_1")
                || last.summary.contains("Active Task"),
            "{}",
            last.summary
        );
        assert!(last.archive_body().chars().count() <= ARCHIVE_CHARS);
    }

    #[test]
    fn two_hundred_compacts_stay_extractive_and_fast() {
        let t0 = std::time::Instant::now();
        let mut events = vec![start()];
        let mut n = 0u32;
        let mut last_live = 0usize;
        for i in 1..=240 {
            events.push(SessionEvent::user(format!(
                "task-{i} edit src/mod_{i}.rs keep token tok-{i}"
            )));
            events.push(SessionEvent::assistant(
                "",
                "",
                Some(vec![read_call(
                    &format!("c{i}"),
                    &format!("src/mod_{i}.rs"),
                )]),
            ));
            events.push(SessionEvent::tool(
                format!("c{i}"),
                "read",
                format!("pub fn f{i}() {{ /* {} tok-{i} */ }}", "x".repeat(80)),
            ));
            if let Some(plan) = plan_compact(&events) {
                assert!(
                    plan.archive_body().chars().count() <= ARCHIVE_CHARS,
                    "archive grew past clip at compact {n}"
                );
                assert!(is_hidden_user_text(&plan.archive_user_text()));
                events.push(SessionEvent::compact(plan));
                n += 1;
                let msgs = crate::session::derive_messages(&events);
                let real_users = msgs
                    .iter()
                    .filter(|m| {
                        m.role == "user" && !is_hidden_user_text(m.content.as_deref().unwrap_or(""))
                    })
                    .count();
                assert_eq!(real_users, 1, "compact {n} lost last_query_index");
                assert!(
                    msgs.len() <= 12,
                    "live window bloated to {} after {n} compacts",
                    msgs.len()
                );
                last_live = msgs.len();
            }
        }
        let elapsed = t0.elapsed();
        assert!(n >= 200, "only {n} compact cycles; live={last_live}");
        assert!(
            elapsed.as_secs() < 8,
            "200 extractive compacts took {elapsed:?}; scan is too heavy for hour-scale sessions"
        );
        assert!(events.len() > 200, "jsonl must keep growing");
        let last = events.iter().rev().find_map(|e| match e {
            SessionEvent::Compact(c) => Some(c),
            _ => None,
        });
        let last = last.expect("last compact");
        assert!(last.archive_body().chars().count() <= ARCHIVE_CHARS);
        // Early tokens may fall out of the clipped index; the live user must remain.
        let live = crate::session::derive_messages(&events);
        let last_user = live
            .iter()
            .rev()
            .find(|m| m.role == "user" && !is_hidden_user_text(m.content.as_deref().unwrap_or("")))
            .expect("live user");
        assert!(
            last_user
                .content
                .as_deref()
                .unwrap_or("")
                .contains("task-240"),
            "{}",
            last_user.content.as_deref().unwrap_or("")
        );
    }

    #[test]
    fn index_keeps_full_blob_sha_and_media_paths() {
        let sha = "b".repeat(64);
        let events = vec![
            start(),
            SessionEvent::user("look at the screenshot"),
            SessionEvent::assistant("", "", Some(vec![read_call("c1", "shot.png")])),
            SessionEvent::tool_folded("c1", "read", "image ok", Some(sha.clone()), Some(4000))
                .with_media(vec![crate::session::event::StoredMedia {
                    kind: "image".into(),
                    mime: "image/jpeg".into(),
                    url: ".q38-agent/generated/shot.jpg".into(),
                }]),
            SessionEvent::user("click it"),
        ];
        let plan = plan_compact(&events).expect("compact");
        assert!(
            plan.index.contains(&sha),
            "recall(blob=) needs the full sha:\n{}",
            plan.index
        );
        assert!(
            plan.index.contains(".q38-agent/generated/shot.jpg"),
            "media path missing:\n{}",
            plan.index
        );
        assert!(
            plan.summary.contains(&sha) || plan.summary.contains("shot.jpg"),
            "{}",
            plan.summary
        );
    }

    #[test]
    fn index_keeps_head_and_tail_across_a_long_turn() {
        let mut events = vec![start(), SessionEvent::user("old")];
        for i in 0..90 {
            let id = format!("c{i}");
            let path = format!("early-{i}.rs");
            events.push(SessionEvent::assistant(
                "",
                "",
                Some(vec![read_call(&id, &path)]),
            ));
            events.push(SessionEvent::tool(&id, "read", format!("body {i}")));
        }
        events.push(SessionEvent::user("next"));
        let plan = plan_compact(&events).expect("compact");
        assert!(
            plan.index.contains("early-0.rs"),
            "head of index dropped:\n{}",
            plan.index
        );
        assert!(
            plan.index.contains("early-89.rs"),
            "tail of index dropped:\n{}",
            plan.index
        );
        let n = plan.index.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(n <= INDEX_LINES, "index grew to {n} lines:\n{}", plan.index);
    }
}
