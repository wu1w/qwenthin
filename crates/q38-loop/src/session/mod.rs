//! Append-only JSONL session log, `derive_messages`, and `/mode` fork.

pub mod catalog;
mod compact;
mod derive;
mod event;
mod index;
mod log;
mod recall;

pub use compact::{apply_compact, compact_messages, plan_compact};
pub use derive::{derive_messages, live_policy};
pub use event::{
    policy_for_effort, AssistantEvent, CompactEvent, DeltaChannel, DeltaEvent, ForkEvent,
    OpenAiFunction, OpenAiToolCall, PolicyEvent, PolicyReason, SessionEvent, SessionMode,
    SessionStart, StopEvent, StoredMedia, ToolEvent, UndoEvent, UserEvent,
};
pub use index::{HistoryIndex, Hit};
pub use log::SessionLog;
pub use recall::run as run_recall;

use crate::vendor::sha256_hex;

pub use crate::slash::{parse_slash, SlashCmd};

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// SHA-256 of compact canonical tools JSON (`serde_json` preserve_order).
pub fn tools_hash(tools: &[serde_json::Value]) -> String {
    let bytes = serde_json::to_vec(tools).unwrap_or_else(|_| b"[]".to_vec());
    sha256_hex(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Effort, ThinkPolicy};
    use crate::session::{plan_compact, SessionEvent};
    use serde_json::json;
    use std::fs;

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("q38-sess-{}", new_session_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn start(id: &str, mode: SessionMode, system: &str) -> SessionStart {
        SessionStart::new(
            id,
            "/tmp/ws",
            mode,
            system,
            tools_hash(&[]),
            mode.default_policy(),
        )
    }

    fn roles(msgs: &[crate::template::ChatMessage]) -> Vec<&str> {
        msgs.iter().map(|m| m.role.as_str()).collect()
    }

    #[test]
    fn derive_messages_has_exactly_one_leading_system() {
        let dir = tmp_dir();
        let mut log =
            SessionLog::create_in(&dir, start("s1", SessionMode::Agent, "sys-a")).unwrap();
        log.append(SessionEvent::user("hi")).unwrap();
        log.append(SessionEvent::assistant("hello", "thought", None))
            .unwrap();
        log.append(SessionEvent::policy(
            parse_slash("/think xhigh").unwrap().policy().unwrap(),
            PolicyReason::Slash,
        ))
        .unwrap();

        let msgs = derive_messages(log.events());
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content.as_deref(), Some("sys-a"));
        assert_eq!(
            msgs.iter().filter(|m| m.role == "system").count(),
            1,
            "exactly one leading system, got {msgs:?}"
        );
        assert_eq!(roles(&msgs), ["system", "user", "assistant"]);
        assert_eq!(msgs[2].reasoning_content.as_deref(), Some("thought"));
        assert!(!msgs[2].content.as_deref().unwrap_or("").contains("<think>"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn derive_skips_undone_range() {
        let dir = tmp_dir();
        let mut log =
            SessionLog::create_in(&dir, start("s-undo", SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("hi")).unwrap();
        log.append(SessionEvent::assistant("yo", "", None)).unwrap();
        log.append(SessionEvent::undo(1, 2)).unwrap();
        let msgs = derive_messages(log.events());
        assert_eq!(roles(&msgs), ["system"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn derive_skips_delta_and_log_drops_it() {
        let dir = tmp_dir();
        let mut log =
            SessionLog::create_in(&dir, start("s-delta", SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("hi")).unwrap();
        log.append(SessionEvent::delta_chunk(
            crate::session::DeltaChannel::Content,
            "x",
        ))
        .unwrap();
        log.append(SessionEvent::assistant("hello", "", None))
            .unwrap();
        let kinds: Vec<_> = log.events().iter().map(|e| e.type_name()).collect();
        assert_eq!(kinds, ["session/start", "user", "assistant"]);

        let mut events = log.events().to_vec();
        events.insert(
            2,
            SessionEvent::delta_chunk(crate::session::DeltaChannel::Reasoning, "hmm"),
        );
        let msgs = derive_messages(&events);
        assert_eq!(roles(&msgs), ["system", "user", "assistant"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn derive_restores_view_media_parts() {
        let dir = tmp_dir();
        let mut log =
            SessionLog::create_in(&dir, start("s-media", SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("color?")).unwrap();
        log.append(
            SessionEvent::tool("c1", "view", "Image loaded: red.png").with_media(vec![
                crate::session::StoredMedia {
                    kind: "image".into(),
                    mime: "image/png".into(),
                    url: "data:image/png;base64,xx".into(),
                },
            ]),
        )
        .unwrap();
        let msgs = derive_messages(log.events());
        let tool = msgs.iter().find(|m| m.role == "tool").unwrap();
        assert_eq!(tool.parts.len(), 1);
        assert_eq!(tool.parts[0].url, "data:image/png;base64,xx");
        assert_eq!(tool.text(), "Image loaded: red.png");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn derive_restores_user_media_parts() {
        // 用户附图与 tool 侧同样落盘还原，resume 后不丢图。
        let dir = tmp_dir();
        let mut log =
            SessionLog::create_in(&dir, start("s-umedia", SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("what color is this?").with_media(vec![
            crate::session::StoredMedia {
                kind: "image".into(),
                mime: "image/png".into(),
                url: "data:image/png;base64,yy".into(),
            },
        ]))
        .unwrap();
        let msgs = derive_messages(log.events());
        let user = msgs.iter().find(|m| m.role == "user").unwrap();
        assert_eq!(user.parts.len(), 1);
        assert_eq!(user.parts[0].url, "data:image/png;base64,yy");
        assert_eq!(user.content.as_deref(), Some("what color is this?"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn policy_event_does_not_create_a_second_session_start() {
        let dir = tmp_dir();
        let mut log = SessionLog::create_in(&dir, start("s2", SessionMode::Agent, "sys")).unwrap();
        let before = log.start().unwrap().mode;
        log.append(SessionEvent::user("q")).unwrap();
        log.append(SessionEvent::policy(
            policy_for_effort(crate::policy::Effort::Medium),
            PolicyReason::Slash,
        ))
        .unwrap();

        let starts = log
            .events()
            .iter()
            .filter(|e| matches!(e, SessionEvent::Start(_)))
            .count();
        assert_eq!(starts, 1);
        assert_eq!(log.start().unwrap().mode, before);
        assert_eq!(
            log.policy().unwrap().effort,
            Some(crate::policy::Effort::Medium)
        );
        assert_ne!(
            log.policy().unwrap().max_think_tokens,
            log.start().unwrap().policy.max_think_tokens
        );
        assert!(log
            .append(SessionEvent::Start(start("nope", SessionMode::Chat, "x")))
            .is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn mode_fork_replaces_events0_and_appends_fork() {
        let dir = tmp_dir();
        let mut old =
            SessionLog::create_in(&dir, start("old", SessionMode::Agent, "sys-agent")).unwrap();
        old.append(SessionEvent::user("work")).unwrap();
        old.append(SessionEvent::policy(
            policy_for_effort(crate::policy::Effort::Low),
            PolicyReason::Cli,
        ))
        .unwrap();

        let new_start = old.start().unwrap().for_fork(
            "new",
            SessionMode::Think,
            "sys-think",
            tools_hash(&[json!({"type":"function"})]),
        );
        let forked = old.fork(new_start).unwrap();

        match &forked.events()[0] {
            SessionEvent::Start(s) => {
                assert_eq!(s.mode, SessionMode::Think);
                assert_eq!(s.id, "new");
                assert_eq!(s.system, "sys-think");
                assert_ne!(s.tools_hash, old.start().unwrap().tools_hash);
            }
            other => panic!("expected session/start, got {other:?}"),
        }
        match forked.events().last() {
            Some(SessionEvent::Fork(f)) => assert_eq!(f.from_id, "old"),
            other => panic!("expected session/fork, got {other:?}"),
        }
        assert_eq!(old.start().unwrap().mode, SessionMode::Agent);
        assert_ne!(old.start().unwrap().mode, forked.start().unwrap().mode);
        assert_eq!(
            forked
                .events()
                .iter()
                .filter(|e| matches!(e, SessionEvent::Start(_)))
                .count(),
            1
        );
        assert!(
            forked
                .events()
                .iter()
                .any(|e| matches!(e, SessionEvent::Policy(_))),
            "depth/policy stays in the forked file"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn policy_fork_stop_are_not_chat_message_roles() {
        let dir = tmp_dir();
        let mut log = SessionLog::create_in(&dir, start("s4", SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("hi")).unwrap();
        log.append(SessionEvent::assistant(
            "ok",
            "",
            Some(vec![OpenAiToolCall::function(
                "c1",
                "read",
                "{\"path\":\"a.rs\"}",
            )]),
        ))
        .unwrap();
        log.append(SessionEvent::tool("c1", "read", "fn main() {}"))
            .unwrap();
        log.append(SessionEvent::policy(
            ThinkPolicy::off(),
            PolicyReason::Slash,
        ))
        .unwrap();
        log.append(SessionEvent::stop("budget:context")).unwrap();
        let forked = log
            .fork(
                log.start()
                    .unwrap()
                    .for_fork("s4b", SessionMode::Chat, "sys", tools_hash(&[])),
            )
            .unwrap();

        let msgs = derive_messages(forked.events());
        for msg in &msgs {
            assert!(
                matches!(msg.role.as_str(), "system" | "user" | "assistant" | "tool"),
                "unexpected role {}",
                msg.role
            );
            assert_ne!(msg.role, "policy");
            assert_ne!(msg.role, "session/fork");
            assert_ne!(msg.role, "fork");
            assert_ne!(msg.role, "stop");
            assert_ne!(msg.role, "session/start");
        }
        assert_eq!(roles(&msgs), ["system", "user", "assistant", "tool"]);
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("c1"));
        let args = &msgs[2].tool_calls.as_ref().unwrap()[0]["function"]["arguments"];
        assert!(args.is_object(), "{args}");
        assert_eq!(args["path"], "a.rs");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fts_indexes_live_text_not_reasoning() {
        let dir = tmp_dir();
        let mut log = SessionLog::create_in(&dir, start("idx", SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("prefix cache miss on effort"))
            .unwrap();
        log.append(SessionEvent::assistant(
            "will fold dumps",
            "do not index the word zirconium",
            None,
        ))
        .unwrap();
        log.append(SessionEvent::tool("c1", "bash", "cargo test passed"))
            .unwrap();

        let hits = log.search("prefix cache", 10).unwrap();
        assert!(hits.iter().any(|h| h.kind == "user"), "{hits:?}");
        assert!(log.search("zirconium", 10).unwrap().is_empty());
        assert!(log
            .search("cargo", 10)
            .unwrap()
            .iter()
            .any(|h| h.kind == "tool"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn compact_skips_evicted_events_and_wraps_archive() {
        let dir = tmp_dir();
        let mut log = SessionLog::create_in(&dir, start("c1", SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("fix prefix cache")).unwrap();
        log.append(SessionEvent::assistant(
            "will inspect",
            "do not leak zirconium",
            Some(vec![OpenAiToolCall::function(
                "c1",
                "read",
                r#"{"path":"a.rs"}"#,
            )]),
        ))
        .unwrap();
        log.append(SessionEvent::tool("c1", "read", "fn main() {}"))
            .unwrap();
        log.append(SessionEvent::user("now edit it")).unwrap();
        let plan = plan_compact(log.events()).expect("plan");
        log.append(SessionEvent::compact(plan)).unwrap();

        let raw = fs::read_to_string(log.path()).unwrap();
        assert!(raw.contains("fix prefix cache"));
        assert!(raw.contains("zirconium"));
        assert!(raw.contains("\"type\":\"session/compact\""));

        let msgs = derive_messages(log.events());
        assert_eq!(msgs[0].role, "system");
        assert!(
            crate::template::is_hidden_user_text(msgs[1].content.as_deref().unwrap()),
            "{:?}",
            msgs[1].content
        );
        assert!(!msgs[1].content.as_deref().unwrap().contains("zirconium"));
        assert_eq!(
            msgs.iter()
                .filter(|m| m.role == "user"
                    && !crate::template::is_hidden_user_text(m.content.as_deref().unwrap_or("")))
                .count(),
            1
        );
        let live = msgs
            .iter()
            .find(|m| {
                m.role == "user"
                    && !crate::template::is_hidden_user_text(m.content.as_deref().unwrap_or(""))
            })
            .unwrap();
        assert_eq!(live.content.as_deref(), Some("now edit it"));
        assert!(!msgs
            .iter()
            .any(|m| m.content.as_deref() == Some("fn main() {}")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn jsonl_append_reopen_roundtrip() {
        let dir = tmp_dir();
        let id = "round";
        let mut log = SessionLog::create_in(&dir, start(id, SessionMode::Agent, "sys")).unwrap();
        log.append(SessionEvent::user("ping")).unwrap();
        log.append(SessionEvent::assistant("pong", "r", None))
            .unwrap();
        log.append(SessionEvent::policy(
            ThinkPolicy::off(),
            PolicyReason::Watchdog,
        ))
        .unwrap();
        log.append(SessionEvent::stop("end")).unwrap();
        let expected = log.events().to_vec();
        let path = log.path().to_path_buf();
        drop(log);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "session file must be 0600");
        }

        let reopened = SessionLog::open_in(&dir, id).unwrap();
        assert_eq!(reopened.events(), expected.as_slice());
        assert_eq!(reopened.messages().len(), 3); // system, user, assistant
        assert_eq!(reopened.policy().unwrap(), ThinkPolicy::off());

        let raw = fs::read_to_string(&path).unwrap();
        let type_names: Vec<String> = raw
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            type_names,
            ["session/start", "user", "assistant", "policy", "stop"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parse_slash_depth_and_mode() {
        assert_eq!(parse_slash("/think"), Some(SlashCmd::Think(Effort::Medium)));
        assert_eq!(
            parse_slash("  /think medium  "),
            Some(SlashCmd::Think(Effort::Medium))
        );
        assert_eq!(
            parse_slash("/think low"),
            Some(SlashCmd::Think(Effort::Low))
        );
        assert_eq!(
            parse_slash("/think xhigh"),
            Some(SlashCmd::Think(Effort::Xhigh))
        );
        assert_eq!(parse_slash("/fast"), Some(SlashCmd::Off));
        assert_eq!(parse_slash("/think off"), Some(SlashCmd::Off));
        assert_eq!(
            parse_slash("/mode think"),
            Some(SlashCmd::Mode(SessionMode::Think))
        );
        assert!(parse_slash("hello").is_none());
        assert!(parse_slash("/think nope").is_none());
        assert!(parse_slash("/mode").is_none());

        let med = parse_slash("/think").unwrap().policy().unwrap();
        assert_eq!(med.effort, Some(Effort::Medium));
        assert_eq!(med.max_think_tokens, 2048);
        assert_eq!(med.max_tokens, 8192);
        assert!(med.preserve);

        let xhigh = parse_slash("/think xhigh").unwrap().policy().unwrap();
        assert_eq!(xhigh.effort, Some(Effort::Xhigh));
        assert_eq!(xhigh.max_think_tokens, 4096);
        assert_eq!(xhigh.max_tokens, 16384);
        assert!(xhigh.preserve);

        let think_mode = SessionMode::Think.default_policy();
        assert!(think_mode.preserve);
        assert_eq!(think_mode.max_think_tokens, 2048);

        assert!(parse_slash("/mode chat").unwrap().policy().is_none());
    }
}
