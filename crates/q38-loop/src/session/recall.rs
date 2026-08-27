//! Session archive recall. Not in the frozen four-tool schema; the agent
//! appends `recall_tool()` after a compact (prefix already misses).

use crate::session::event::SessionEvent;
use crate::session::log::SessionLog;
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools::{arg_str, arg_u32, folded_response, BlobStore, ToolLimits};

pub fn run(
    log: Option<&SessionLog>,
    blobs: &BlobStore,
    call: &ToolCall,
    limits: ToolLimits,
) -> ToolResponse {
    if let Some(sha) = arg_str(&call.arguments, "blob").or_else(|| arg_str(&call.arguments, "sha"))
    {
        return expand_blob(blobs, &call.id, &sha, limits);
    }
    if let Some(seq) = arg_u32(&call.arguments, "seq") {
        return expand_seq(log, &call.id, seq, limits);
    }
    if let Some(query) = arg_str(&call.arguments, "query") {
        return search(log, &call.id, &query, limits);
    }
    ToolResponse::text(
        &call.id,
        "Error: recall needs `query`, `seq`, or `blob`.",
        ToolState::Error,
    )
}

fn search(log: Option<&SessionLog>, id: &str, query: &str, limits: ToolLimits) -> ToolResponse {
    let Some(log) = log else {
        return ToolResponse::text(id, "Error: no session index.", ToolState::Error);
    };
    match log.search(query, 8) {
        Ok(hits) if hits.is_empty() => ToolResponse::text(id, "No matches.", ToolState::Success),
        Ok(hits) => {
            let mut text = String::new();
            for h in hits {
                text.push_str(&format!(
                    "seq={} kind={} name={} blob={}\n{}\n\n",
                    h.seq,
                    h.kind,
                    h.name.as_deref().unwrap_or("-"),
                    h.blob.as_deref().unwrap_or("-"),
                    h.snippet.trim()
                ));
            }
            folded_response(id, text, ToolState::Success, limits, None)
        }
        Err(e) => ToolResponse::text(id, format!("Error: {e}"), ToolState::Error),
    }
}

fn expand_seq(log: Option<&SessionLog>, id: &str, seq: u32, limits: ToolLimits) -> ToolResponse {
    let Some(log) = log else {
        return ToolResponse::text(id, "Error: no session log.", ToolState::Error);
    };
    let Some(event) = log.events().get(seq as usize) else {
        return ToolResponse::text(
            id,
            format!("Error: seq {seq} out of range."),
            ToolState::Error,
        );
    };
    folded_response(
        id,
        format_event(seq, event),
        ToolState::Success,
        limits,
        None,
    )
}

fn expand_blob(blobs: &BlobStore, id: &str, sha: &str, limits: ToolLimits) -> ToolResponse {
    match blobs.get_text(sha.trim()) {
        Ok(text) => folded_response(id, text, ToolState::Success, limits, Some(blobs)),
        Err(e) => ToolResponse::text(id, format!("Error: blob {e}"), ToolState::Error),
    }
}

fn format_event(seq: u32, event: &SessionEvent) -> String {
    match event {
        SessionEvent::User(u) => {
            let mut s = format!("seq={seq} user\n{}", u.text);
            append_media(&mut s, &u.media);
            s
        }
        SessionEvent::Assistant(a) => {
            let mut s = format!("seq={seq} assistant\n{}", a.content);
            if let Some(calls) = &a.tool_calls {
                for c in calls {
                    s.push_str(&format!("\n{} {}", c.function.name, c.function.arguments));
                }
            }
            s
        }
        SessionEvent::Tool(t) => {
            let mut s = format!("seq={seq} tool {}\n{}", t.name, t.output);
            if let Some(blob) = &t.blob {
                s.push_str(&format!("\nblob={blob}"));
            }
            append_media(&mut s, &t.media);
            s
        }
        SessionEvent::Compact(c) => format!(
            "seq={seq} compact until={} keep_user={}\n{}\n{}",
            c.until_seq, c.keep_user_seq, c.summary, c.index
        ),
        other => format!("seq={seq} {}", other.type_name()),
    }
}

fn append_media(s: &mut String, media: &[crate::session::event::StoredMedia]) {
    for m in media {
        s.push_str(&format!("\nmedia={} {}", m.mime, m.url));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::event::{SessionEvent, StoredMedia};

    #[test]
    fn format_event_prints_full_blob_and_media_paths() {
        let sha = "a".repeat(64);
        let ev = SessionEvent::tool_folded("c1", "read", "image ok", Some(sha.clone()), Some(1200))
            .with_media(vec![StoredMedia {
                kind: "image".into(),
                mime: "image/jpeg".into(),
                url: ".q38-agent/generated/shot.jpg".into(),
            }]);
        let text = format_event(9, &ev);
        assert!(text.contains(&sha), "{text}");
        assert!(
            text.contains("media=image/jpeg .q38-agent/generated/shot.jpg"),
            "{text}"
        );
    }
}
