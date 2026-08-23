//! Workspace tools. Frozen OpenAI names: `read` / `write` / `edit` / `bash` / `run_code`.
//! `search` is appended by the agent, not in that frozen JSON.
//!
//! Paths resolve from the workspace; writes stay inside it when confined.

mod bash;
mod code_index;
mod fold;
mod fs;
mod media_exec;
mod path;
mod run_code;
mod view;
mod web;

use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::tool_calls::{CancelFlag, TextBlock, ToolCall, ToolResponse, ToolState};

/// Foreign `{stem}.q38tmp.*` leftovers older than this are removed on the next
/// write/put. Newer files are left alone so a concurrent writer's in-flight tmp
/// is not deleted. Same-pid temps are never removed here (`TmpGuard` owns those).
pub(super) const STALE_TMP_MAX_AGE: Duration = Duration::from_secs(300);

pub use code_index::{bash_search_query, run_search, search_dump_too_big, CodeIndex};
pub use fold::{fold_text, BlobStore, Folded};
pub use path::Workspace;
pub use view::view;
pub use web::WebRunner;

#[derive(Clone, Copy, Debug)]
pub struct ToolLimits {
    pub read_default_lines: u32,
    pub result_max_chars: usize,
    pub result_head_chars: usize,
    pub result_tail_chars: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            read_default_lines: 2000,
            result_max_chars: 10_000,
            result_head_chars: 8_000,
            result_tail_chars: 2_000,
        }
    }
}

impl From<&crate::config::ToolsConfig> for ToolLimits {
    fn from(c: &crate::config::ToolsConfig) -> Self {
        Self {
            read_default_lines: c.read_default_lines,
            result_max_chars: c.result_max_chars as usize,
            result_head_chars: c.result_head_chars as usize,
            result_tail_chars: c.result_tail_chars as usize,
        }
    }
}

pub(super) fn cleanup_stale_tmp(dir: &Path, stem: &str) {
    cleanup_stale_tmp_older_than(dir, stem, STALE_TMP_MAX_AGE);
}

pub(super) fn cleanup_stale_tmp_older_than(dir: &Path, stem: &str, min_age: Duration) {
    let prefix = format!("{stem}.q38tmp.");
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(pid_str) = rest.split('.').next() else {
            continue;
        };
        let foreign = pid_str.parse::<u32>().map(|p| p != me).unwrap_or(true);
        if !foreign {
            continue;
        }
        if !tmp_is_stale(&entry.path(), min_age) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

fn tmp_is_stale(path: &Path, min_age: Duration) -> bool {
    if min_age.is_zero() {
        return true;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    match meta.modified() {
        Ok(mtime) => SystemTime::now()
            .duration_since(mtime)
            .map(|age| age >= min_age)
            .unwrap_or(true),
        Err(_) => true,
    }
}

pub async fn run_tool(
    workspace: &Workspace,
    call: &ToolCall,
    cancel: CancelFlag,
    limits: ToolLimits,
    inherit_env: bool,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    match call.name.as_str() {
        "read" => fs::read_file(workspace, call, limits, blobs),
        "write" => fs::write_file(workspace, call),
        "edit" => fs::edit_file(workspace, call),
        "bash" => bash::bash(workspace, call, cancel, limits, blobs).await,
        "run_code" => run_code::run_code(workspace, call, cancel, limits, inherit_env, blobs).await,
        "view" => {
            view::view(
                workspace,
                call,
                &crate::media::MediaCaps::default(),
                &crate::media::MediaBins::detect(),
                crate::media::MAX_INLINE_MEDIA_BYTES,
            )
            .await
        }
        other => ToolResponse::text(
            &call.id,
            format!("Error: unknown tool '{other}'."),
            ToolState::Error,
        ),
    }
}

pub(crate) fn folded_response(
    id: &str,
    text: String,
    state: ToolState,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let folded = fold_text(&text, limits, blobs);
    ToolResponse {
        id: id.to_string(),
        content: vec![TextBlock { text: folded.live }],
        state,
        offloaded: false,
        blob: folded.blob,
        original_chars: folded.original_chars,
        media: Vec::new(),
    }
}

pub(crate) fn arg_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) if !v.is_null() => Some(v.to_string()),
        _ => None,
    }
}

/// Frozen schema is `path`; QwenPaw `file_io.py` uses `file_path`.
pub(crate) fn arg_path(args: &Value) -> Option<String> {
    arg_str(args, "path").or_else(|| arg_str(args, "file_path"))
}

/// Frozen schema is `old_string` / `new_string`; QwenPaw uses `old_text` / `new_text`.
pub(crate) fn arg_old_string(args: &Value) -> Option<String> {
    arg_str(args, "old_string").or_else(|| arg_str(args, "old_text"))
}

pub(crate) fn arg_new_string(args: &Value) -> Option<String> {
    arg_str(args, "new_string").or_else(|| arg_str(args, "new_text"))
}

pub(crate) fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().map(|v| v as u32),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn scratch() -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("q38-tools-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Workspace::open(&dir, true).unwrap();
        (w, dir)
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn read_write_edit_roundtrip() {
        let (ws, dir) = scratch();
        let w = fs::write_file(
            &ws,
            &call(
                "write",
                json!({"path": "a.rs", "content": "fn main() {}\n"}),
            ),
        );
        assert_eq!(w.state, ToolState::Success);

        let r = fs::read_file(
            &ws,
            &call("read", json!({"path": "a.rs"})),
            ToolLimits::default(),
            None,
        );
        assert!(r.joined_text().contains("fn main()"), "{}", r.joined_text());

        let e = fs::edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "path": "a.rs",
                    "old_string": "fn main() {}",
                    "new_string": "fn start() {}"
                }),
            ),
        );
        assert_eq!(e.state, ToolState::Success);
        let r = fs::read_file(
            &ws,
            &call("read", json!({"path": "a.rs"})),
            ToolLimits::default(),
            None,
        );
        assert!(r.joined_text().contains("fn start()"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn qwenpaw_file_io_aliases() {
        let (ws, dir) = scratch();
        fs::write_file(
            &ws,
            &call(
                "write",
                json!({"file_path": "n.txt", "content": "one\ntwo\nthree\n"}),
            ),
        );
        let r = fs::read_file(
            &ws,
            &call(
                "read",
                json!({"file_path": "n.txt", "start_line": 2, "end_line": 3}),
            ),
            ToolLimits::default(),
            None,
        );
        let text = r.joined_text();
        assert!(text.contains("two"), "{text}");
        assert!(text.contains("three"), "{text}");
        assert!(!text.contains("one\n"), "{text}");
        let e = fs::edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "file_path": "n.txt",
                    "old_text": "two",
                    "new_text": "TWO"
                }),
            ),
        );
        assert_eq!(e.state, ToolState::Success);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_missing_text_errors() {
        let (ws, dir) = scratch();
        fs::write_file(
            &ws,
            &call("write", json!({"path": "a.txt", "content": "hello"})),
        );
        let e = fs::edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "path": "a.txt",
                    "old_string": "nope",
                    "new_string": "x"
                }),
            ),
        );
        assert_eq!(e.state, ToolState::Error);
        assert!(e.joined_text().contains("not found"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_multiple_matches_requires_unique_old() {
        let (ws, dir) = scratch();
        fs::write_file(
            &ws,
            &call("write", json!({"path": "a.txt", "content": "x\ny\nx\ny\n"})),
        );
        let e = fs::edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "path": "a.txt",
                    "old_string": "x",
                    "new_string": "X"
                }),
            ),
        );
        assert_eq!(e.state, ToolState::Error);
        let text = e.joined_text();
        assert!(text.contains("2 times"), "{text}");
        assert!(text.contains("more unique"), "{text}");
        let r = fs::read_file(
            &ws,
            &call("read", json!({"path": "a.txt"})),
            ToolLimits::default(),
            None,
        );
        let rtext = r.joined_text();
        assert!(!rtext.contains("X"), "{rtext}");
        assert!(rtext.contains("1|x"), "{rtext}");
        assert!(rtext.contains("3|x"), "{rtext}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn confined_write_rejects_escape() {
        let (ws, dir) = scratch();
        let out = fs::write_file(
            &ws,
            &call("write", json!({"path": "../escape.txt", "content": "no"})),
        );
        assert_eq!(out.state, ToolState::Error);
        assert!(out.joined_text().contains("workspace"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn placeholder_ellipsis_write_does_not_touch_disk() {
        let (ws, dir) = scratch();
        let out = fs::write_file(
            &ws,
            &call("write", json!({"path": "...", "content": "..."})),
        );
        assert_eq!(out.state, ToolState::Error);
        assert!(out.joined_text().contains("invalid path"));
        assert!(!std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name() == "..."));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_echo() {
        let (ws, dir) = scratch();
        let out = bash::bash(
            &ws,
            &call("bash", json!({"command": "echo hello"})),
            CancelFlag::new(),
            ToolLimits::default(),
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Success);
        assert!(out.joined_text().contains("hello"), "{}", out.joined_text());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_fold_writes_blob_and_keeps_head_tail() {
        let (ws, dir) = scratch();
        let blobs = BlobStore::new(dir.join("blobs"));
        let limits = ToolLimits {
            read_default_lines: 2000,
            result_max_chars: 40,
            result_head_chars: 16,
            result_tail_chars: 8,
        };
        let out = bash::bash(
            &ws,
            &call(
                "bash",
                json!({
                    "command": "echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }),
            ),
            CancelFlag::new(),
            limits,
            Some(&blobs),
        )
        .await;
        assert_eq!(out.state, ToolState::Success);
        let live = out.joined_text();
        assert!(live.contains("omitted"), "{live}");
        let sha = out.blob.as_deref().expect("blob");
        assert!(live.contains(sha));
        let full = blobs.get_text(sha).unwrap();
        assert!(full.contains("aaaa"), "{full}");
        assert_eq!(out.original_chars, full.chars().count());
        assert!(
            !live.contains(&"a".repeat(32)),
            "middle should be omitted: {live}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_caps_huge_limit_and_tells_to_page() {
        let (ws, dir) = scratch();
        let mut body = String::new();
        for i in 1..=40 {
            body.push_str(&format!("line{i}\n"));
        }
        fs::write_file(
            &ws,
            &call("write", json!({"path": "big.txt", "content": body})),
        );
        let limits = ToolLimits {
            read_default_lines: 5,
            result_max_chars: 10_000,
            result_head_chars: 8_000,
            result_tail_chars: 2_000,
        };
        let r = fs::read_file(
            &ws,
            &call("read", json!({"path": "big.txt", "limit": 10_000})),
            limits,
            None,
        );
        let t = r.joined_text();
        assert!(t.contains("limit capped"), "{t}");
        assert!(t.contains("line10"), "{t}");
        assert!(!t.contains("line11"), "{t}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_error_uses_workspace_path() {
        let (ws, dir) = scratch();
        let r = fs::read_file(
            &ws,
            &call("read", json!({"path": "nope.txt"})),
            ToolLimits::default(),
            None,
        );
        let t = r.joined_text();
        assert!(t.contains("nope.txt"), "{t}");
        assert!(
            !t.contains("/private/") && !t.contains("\\\\"),
            "host path leaked: {t}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
