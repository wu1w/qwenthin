//! Hidden user notes after the live query. Not part of the frozen system/tools
//! prefix. 27B treats these as contracts; attach 0 or 1 skill, at most one
//! MEMORY card, and at most one MCP card, then stub after two later real user turns.

use crate::family::Family;
use crate::template::{is_hidden_user_text, wrap_tool_response, ChatMessage};
use crate::tokenize::count_tokens;

/// Real user turns after a note before it becomes `applied`.
pub const STUB_AFTER_USERS: usize = 2;

/// Interactive narration. Re-injected each real user turn once the old copy
/// stubs, so the instruction stays one live card (~30 tokens) at any time.
pub const STYLE_CARD: &str =
    "[style] 边做边说：每个阶段动手前，先用一句话（中文，≤20字）说明接下来做什么，再调用工具。给用户看的话写在正常回复里，思考通道用户看不见。收尾只报结果，不要交空回复。";
pub const SKILL_BODY_MAX_TOKENS: u32 = 400;
pub const AGENTS_MD_MAX_TOKENS: u32 = 400;
pub const MEMORY_HOT_MAX_LINES: usize = 12;
pub const MEMORY_FULL_MAX_LINES: usize = 40;

pub fn tokens(text: &str) -> u32 {
    count_tokens(Family::Qwen38, text)
        .unwrap_or_else(|_| text.chars().count().div_ceil(2).max(1) as u32)
}

pub fn clip_to_tokens(text: &str, max: u32) -> String {
    if tokens(text) <= max {
        return text.trim_end().to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        let cand = if out.is_empty() {
            line.to_string()
        } else {
            format!("{out}\n{line}")
        };
        if tokens(&cand) > max {
            break;
        }
        out = cand;
    }
    out
}

/// `/pdf args` becomes a TurnStart prompt the loop can split.
pub fn skill_turn_prompt(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        format!("[skill:{name}]")
    } else {
        format!("[skill:{name}]\n{args}")
    }
}

pub fn mcp_turn_prompt(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        format!("[mcp:{name}]")
    } else {
        format!("[mcp:{name}]\n{args}")
    }
}

pub fn split_skill_prefix(text: &str) -> (Option<String>, String) {
    split_tagged_prefix(text, "[skill:")
}

pub fn split_mcp_prefix(text: &str) -> (Option<String>, String) {
    split_tagged_prefix(text, "[mcp:")
}

fn split_tagged_prefix(text: &str, tag: &str) -> (Option<String>, String) {
    let t = text.trim();
    let Some(rest) = t.strip_prefix(tag) else {
        return (None, text.to_string());
    };
    let (name, tail) = match rest.split_once(']') {
        Some((n, r)) => (n.trim(), r.trim_start_matches('\n').to_string()),
        None => return (None, text.to_string()),
    };
    if name.is_empty() {
        return (None, text.to_string());
    }
    (Some(name.to_string()), tail)
}

pub fn is_sticky_note(text: &str) -> bool {
    let inner = unwrap_hidden(text);
    inner.starts_with("[skill:")
        || inner.starts_with("[mcp:")
        || inner.starts_with("[mcp]")
        || inner.starts_with("[style]")
        || inner.starts_with("MEMORY hot")
        || inner.starts_with("MEMORY hosts")
        || inner.starts_with("MEMORY.md")
}

fn unwrap_hidden(text: &str) -> &str {
    let t = text.trim();
    t.strip_prefix("<tool_response>")
        .and_then(|s| s.strip_suffix("</tool_response>"))
        .map(str::trim)
        .unwrap_or(t)
}

fn stub_body(inner: &str) -> String {
    if let Some(rest) = inner.strip_prefix("[skill:") {
        if let Some(name) = rest.split(']').next() {
            return format!("[skill: {}] applied", name.trim());
        }
    }
    if inner.starts_with("MEMORY hot") {
        return "MEMORY hot applied".into();
    }
    if inner.starts_with("MEMORY hosts") {
        return "MEMORY hosts applied".into();
    }
    if inner.starts_with("MEMORY.md") {
        return "MEMORY.md applied".into();
    }
    if let Some(rest) = inner.strip_prefix("[mcp:") {
        if let Some(name) = rest.split(']').next() {
            return format!("[mcp: {}] applied", name.trim());
        }
    }
    if inner.starts_with("[mcp]") {
        return "[mcp] applied".into();
    }
    if inner.starts_with("[style]") {
        return "[style] applied".into();
    }
    "applied".into()
}

/// Replace expired skill/memory notes in the live window. JSONL is unchanged.
/// 返回实际替换条数：原位改写会击穿前缀缓存，调用侧据此打观测 note。
pub fn stub_expired_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::AfterUsers(STUB_AFTER_USERS), |_| true)
}

/// User explicitly switched (`[skill:…]` / FAILED testhook). Don't wait two turns.
pub fn stub_live_skill_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| {
        inner.starts_with("[skill:")
    })
}

/// User explicitly named an MCP server. Don't wait two turns.
pub fn stub_live_mcp_notes(messages: &mut [ChatMessage]) -> usize {
    stub_notes(messages, StubWhen::Now, |inner| {
        inner.starts_with("[mcp:") || inner.starts_with("[mcp]")
    })
}

enum StubWhen {
    AfterUsers(usize),
    Now,
}

fn stub_notes(messages: &mut [ChatMessage], when: StubWhen, pred: impl Fn(&str) -> bool) -> usize {
    let mut stubbed = 0usize;
    let n = messages.len();
    for i in 0..n {
        if messages[i].role != "user" {
            continue;
        }
        let Some(content) = messages[i].content.clone() else {
            continue;
        };
        if !is_hidden_user_text(&content) || !is_sticky_note(&content) {
            continue;
        }
        let inner = unwrap_hidden(&content);
        if inner.contains(" applied") || !pred(inner) {
            continue;
        }
        match when {
            StubWhen::Now => {}
            StubWhen::AfterUsers(need) => {
                let later = messages[i + 1..]
                    .iter()
                    .filter(|m| {
                        m.role == "user" && !is_hidden_user_text(m.content.as_deref().unwrap_or(""))
                    })
                    .count();
                if later < need {
                    continue;
                }
            }
        }
        messages[i].content = Some(wrap_tool_response(&stub_body(inner)));
        stubbed += 1;
    }
    stubbed
}

pub fn live_has_skill_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                inner.starts_with("[skill:") && !inner.contains(" applied")
            })
    })
}

pub fn live_has_memory_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                inner.starts_with("MEMORY") && !inner.contains(" applied")
            })
    })
}

pub fn live_has_mcp_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                (inner.starts_with("[mcp:") || inner.starts_with("[mcp]"))
                    && !inner.contains(" applied")
            })
    })
}

pub fn live_has_cron_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| unwrap_hidden(c).starts_with("[console-cron]"))
    })
}

pub fn live_has_style_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content.as_deref().is_some_and(|c| {
                let inner = unwrap_hidden(c);
                inner.starts_with("[style]") && !inner.contains(" applied")
            })
    })
}

pub fn live_has_plan_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| unwrap_hidden(c).starts_with("PLAN MODE"))
    })
}

pub fn live_has_clarify_note(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| unwrap_hidden(c).starts_with("[clarify]"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_prefix_roundtrip() {
        let (name, rest) = split_skill_prefix("[skill:pdf]\nextract this");
        assert_eq!(name.as_deref(), Some("pdf"));
        assert_eq!(rest, "extract this");
        let (none, raw) = split_skill_prefix("just a question");
        assert!(none.is_none());
        assert_eq!(raw, "just a question");
        let (mcp, rest) = split_mcp_prefix("[mcp:docs]\nsearch lantern");
        assert_eq!(mcp.as_deref(), Some("docs"));
        assert_eq!(rest, "search lantern");
    }

    #[test]
    fn stubs_after_two_real_users() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user("[skill: testhook]\n1. rerun the failing file"),
            ChatMessage::assistant("ok"),
            ChatMessage::user("u2"),
            ChatMessage::assistant("ok2"),
            ChatMessage::user("u3"),
        ];
        let stubbed = stub_expired_notes(&mut msgs);
        assert_eq!(stubbed, 1, "one card replaced, caller can log the miss");
        let hidden = msgs[2].content.as_deref().unwrap();
        assert!(hidden.contains("applied"), "{hidden}");
        assert!(!hidden.contains("rerun"));
        // 已 stub 的卡不再计数：重复调用返回 0，观测线不刷屏。
        assert_eq!(stub_expired_notes(&mut msgs), 0);
    }

    #[test]
    fn keeps_note_for_one_followup() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user("[skill: testhook]\n1. rerun"),
            ChatMessage::user("u2"),
        ];
        assert_eq!(stub_expired_notes(&mut msgs), 0, "not expired, no rewrite");
        assert!(msgs[2].content.as_deref().unwrap().contains("rerun"));
    }

    #[test]
    fn forced_switch_stubs_skill_immediately() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user("[skill: testhook]\n1. rerun"),
            ChatMessage::assistant("ok"),
        ];
        stub_live_skill_notes(&mut msgs);
        let hidden = msgs[2].content.as_deref().unwrap();
        assert!(hidden.contains("applied"), "{hidden}");
        assert!(!hidden.contains("rerun"));
        assert!(!live_has_skill_note(&msgs));
    }

    #[test]
    fn style_card_stubs_and_reinjects() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("u1"),
            ChatMessage::hidden_user(STYLE_CARD),
            ChatMessage::assistant("ok"),
            ChatMessage::user("u2"),
            ChatMessage::assistant("ok2"),
            ChatMessage::user("u3"),
        ];
        assert!(live_has_style_note(&msgs));
        stub_expired_notes(&mut msgs);
        let hidden = msgs[2].content.as_deref().unwrap();
        assert!(hidden.contains("[style] applied"), "{hidden}");
        assert!(!live_has_style_note(&msgs), "stub must allow re-inject");
    }

    #[test]
    fn plan_note_detected_once() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("plan this"),
            ChatMessage::hidden_user(
                "PLAN MODE (read-only). Use read/view only. Do not call write.",
            ),
        ];
        assert!(live_has_plan_note(&msgs));
    }
}
