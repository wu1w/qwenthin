use crate::policy::ThinkPolicy;
use crate::session::event::{AssistantEvent, OpenAiToolCall, SessionEvent, StoredMedia};
use crate::template::ChatMessage;

/// Rebuild the model-facing transcript. Exactly one leading `system` from `events[0]`.
/// `policy`, `session/fork`, `session/compact`, and `stop` do not become ChatMessage roles.
///
/// The latest `session/compact` drops events `1..=until_seq` from the live window
/// and injects a `<tool_response>`-wrapped archive so Jinja `last_query_index`
/// stays on the kept real user.
pub fn derive_messages(events: &[SessionEvent]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    if let Some(SessionEvent::Start(start)) = events.first() {
        out.push(ChatMessage::system(start.system.clone()));
    }
    let compact = events.iter().rev().find_map(|e| match e {
        SessionEvent::Compact(c) => Some(c),
        _ => None,
    });
    if let Some(c) = compact {
        out.push(ChatMessage::user(c.archive_user_text()));
        if c.keep_user_seq <= c.until_seq {
            if let Some(SessionEvent::User(u)) = events.get(c.keep_user_seq as usize) {
                out.push(user_message(u));
            }
        }
    }
    let skip_until = compact.map(|c| c.until_seq).unwrap_or(0);
    let undos: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Undo(u) => Some((u.from_seq, u.until_seq)),
            _ => None,
        })
        .collect();
    for (seq, event) in events.iter().enumerate().skip(1) {
        if (seq as u64) <= skip_until {
            continue;
        }
        if undos
            .iter()
            .any(|(from, until)| (seq as u64) >= *from && (seq as u64) <= *until)
        {
            continue;
        }
        match event {
            SessionEvent::User(u) => out.push(user_message(u)),
            SessionEvent::Assistant(a) => out.push(assistant_message(a)),
            SessionEvent::Tool(t) => {
                let mut msg = ChatMessage::tool(&t.tool_call_id, t.output.clone());
                msg.name = Some(t.name.clone());
                msg.parts = t.media.iter().filter_map(|m| stored_part(m)).collect();
                out.push(msg);
            }
            SessionEvent::Start(_)
            | SessionEvent::Policy(_)
            | SessionEvent::Fork(_)
            | SessionEvent::Compact(_)
            | SessionEvent::Stop(_)
            | SessionEvent::Undo(_)
            | SessionEvent::Delta(_) => {}
        }
    }
    crate::sticky::stub_expired_notes(&mut out);
    out
}

/// 用户消息的媒体与 tool 侧同样还原，否则 resume 后用户附图无声丢失。
fn user_message(u: &crate::session::event::UserEvent) -> ChatMessage {
    let mut msg = ChatMessage::user(u.text.clone());
    msg.parts = u.media.iter().filter_map(stored_part).collect();
    msg
}

fn assistant_message(a: &AssistantEvent) -> ChatMessage {
    let mut msg = match &a.tool_calls {
        Some(calls) if !calls.is_empty() => ChatMessage::assistant_tools(
            if a.content.is_empty() {
                None
            } else {
                Some(a.content.clone())
            },
            calls.iter().map(OpenAiToolCall::to_value).collect(),
        ),
        _ => ChatMessage::assistant(a.content.clone()),
    };
    if !a.reasoning.is_empty() {
        msg.reasoning_content = Some(a.reasoning.clone());
    }
    msg
}

/// Live policy is the last `policy` event, else the `session/start` snapshot.
/// Old JSONL with `preserve: false` is lifted onto the official keep-think path.
pub fn live_policy(events: &[SessionEvent]) -> Option<ThinkPolicy> {
    if let Some(policy) = events.iter().rev().find_map(|e| match e {
        SessionEvent::Policy(p) => Some(p.policy.clone()),
        _ => None,
    }) {
        return Some(policy.with_preserved_thinking());
    }
    match events.first() {
        Some(SessionEvent::Start(s)) => Some(s.policy.clone().with_preserved_thinking()),
        _ => None,
    }
}

fn stored_part(m: &StoredMedia) -> Option<crate::media::MediaPart> {
    let kind = crate::media::MediaKind::parse(&m.kind)?;
    Some(crate::media::MediaPart {
        kind,
        mime: m.mime.clone(),
        url: m.url.clone(),
    })
}
