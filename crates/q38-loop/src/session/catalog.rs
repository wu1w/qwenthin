//! Session picker. JSONL stays the evidence; titles live beside it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::session::event::{SessionEvent, SessionMode, SessionStart};
use crate::session::log::SessionLog;
use crate::session::new_session_id;
use crate::template::is_hidden_user_text;

const TITLE_CHARS: usize = 32;
/// Last session the console/TUI sidecar had open. Not a jsonl; `list` skips it.
const CURRENT_FILE: &str = ".current";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub mode: SessionMode,
    pub workspace: String,
    pub channel: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub mtime: u64,
    pub events: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SessionMeta {
    #[serde(default)]
    title: String,
}

pub fn list(dir: impl AsRef<Path>) -> Result<Vec<SessionInfo>> {
    let dir = dir.as_ref();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match inspect(dir, id, &path) {
            Ok(info) => out.push(info),
            Err(_) => continue,
        }
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    Ok(out)
}

pub fn latest(dir: impl AsRef<Path>) -> Result<Option<SessionInfo>> {
    Ok(list(dir)?.into_iter().next())
}

/// Persist the session the local console should reopen after quit / `q38 web` restart.
pub fn remember(dir: impl AsRef<Path>, id: &str) -> Result<()> {
    let id = id.trim();
    if id.is_empty() {
        return Ok(());
    }
    let dir = dir.as_ref();
    fs::create_dir_all(dir)?;
    let path = dir.join(CURRENT_FILE);
    fs::write(&path, id)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Last remembered id, if the jsonl is still on disk.
pub fn remembered(dir: impl AsRef<Path>) -> Option<String> {
    let dir = dir.as_ref();
    let id = fs::read_to_string(dir.join(CURRENT_FILE)).ok()?;
    let id = id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    if dir.join(format!("{id}.jsonl")).is_file() {
        Some(id)
    } else {
        None
    }
}

pub fn is_console_channel(channel: &str) -> bool {
    channel.is_empty()
        || channel.eq_ignore_ascii_case("console")
        || channel.eq_ignore_ascii_case("cli")
        || channel.eq_ignore_ascii_case("sidecar")
        || channel.eq_ignore_ascii_case("tui")
}

fn has_transcript(info: &SessionInfo) -> bool {
    info.events > 1 || !info.preview.is_empty()
}

/// Last opened sidecar session, else newest console transcript with a user turn.
///
/// Ignores IM logs (QQ / Telegram / …) so a busy channel does not steal the
/// console. Empty leftover jsonls from older `q38 web` boots are skipped when
/// falling back (no `.current` pointer).
pub fn preferred_console(dir: impl AsRef<Path>) -> Result<Option<SessionInfo>> {
    let dir = dir.as_ref();
    let all = list(dir)?;
    if let Some(id) = remembered(dir) {
        if let Some(hit) = all.iter().find(|s| s.id == id) {
            return Ok(Some(hit.clone()));
        }
    }
    Ok(all
        .into_iter()
        .find(|s| is_console_channel(&s.channel) && has_transcript(s)))
}

/// `--session` if set; otherwise [`preferred_console`], otherwise a new id.
pub fn resume_console_id(dir: impl AsRef<Path>, explicit: &str) -> String {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    match preferred_console(dir) {
        Ok(Some(hit)) => hit.id,
        _ => new_session_id(),
    }
}

pub fn title_of(dir: impl AsRef<Path>, id: &str) -> String {
    read_meta(dir.as_ref(), id).title
}

pub fn set_title(dir: impl AsRef<Path>, id: &str, title: &str) -> Result<()> {
    let dir = dir.as_ref();
    let jsonl = dir.join(format!("{id}.jsonl"));
    if !jsonl.exists() {
        return Err(Error::msg(format!("session not found: {id}")));
    }
    let mut meta = read_meta(dir, id);
    meta.title = title.trim().to_string();
    let path = meta_path(dir, id);
    fs::write(
        &path,
        serde_json::to_string_pretty(&meta).map_err(Error::msg)?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn delete(dir: impl AsRef<Path>, id: &str) -> Result<()> {
    let dir = dir.as_ref();
    let jsonl = dir.join(format!("{id}.jsonl"));
    if !jsonl.exists() {
        return Err(Error::msg(format!("session not found: {id}")));
    }
    // GC 候选在删除前扫出；解析失败只是放弃 GC，绝不阻塞删除本身。
    let candidates = blob_refs(&jsonl).unwrap_or_default();
    fs::remove_file(jsonl)?;
    let meta = meta_path(dir, id);
    if meta.exists() {
        let _ = fs::remove_file(meta);
    }
    gc_blobs(dir, candidates);
    Ok(())
}

/// 本会话 jsonl 引用的 blob sha（ToolEvent.blob 字段）。读取或任一行
/// 解析失败返回 None——宁可漏删不可误删。
fn blob_refs(path: &Path) -> Option<std::collections::HashSet<String>> {
    let raw = fs::read_to_string(path).ok()?;
    let mut out = std::collections::HashSet::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: SessionEvent = serde_json::from_str(line).ok()?;
        if let SessionEvent::Tool(t) = event {
            if let Some(sha) = t.blob {
                out.insert(sha);
            }
        }
    }
    Some(out)
}

/// 引用计数 GC：候选只来自被删会话自己的引用，其余会话仍引用的保留；
/// 任何一份 jsonl 解析失败就整体放弃。blob 目录是 sessions 的兄弟目录
/// （`~/.q38-agent/blobs`），不存在则无事可做。
fn gc_blobs(dir: &Path, mut candidates: std::collections::HashSet<String>) {
    candidates.retain(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if candidates.is_empty() {
        return;
    }
    let Some(blob_root) = dir.parent().map(|p| p.join("blobs")) else {
        return;
    };
    if !blob_root.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        match blob_refs(&path) {
            Some(refs) => candidates.retain(|s| !refs.contains(s)),
            None => return,
        }
        if candidates.is_empty() {
            return;
        }
    }
    for sha in candidates {
        let _ = fs::remove_file(blob_root.join(&sha));
    }
}

pub fn resolve(dir: impl AsRef<Path>, query: &str) -> Result<Option<SessionInfo>> {
    let q = query.trim();
    if q.is_empty() || q.eq_ignore_ascii_case("latest") {
        return latest(dir);
    }
    let all = list(dir)?;
    if let Some(hit) = all.iter().find(|s| s.id == q) {
        return Ok(Some(hit.clone()));
    }
    let lower = q.to_ascii_lowercase();
    let hits: Vec<_> = all
        .into_iter()
        .filter(|s| {
            s.title.to_ascii_lowercase().contains(&lower)
                || s.preview.to_ascii_lowercase().contains(&lower)
                || s.id.starts_with(q)
        })
        .collect();
    Ok(hits.into_iter().next())
}

fn inspect(dir: &Path, id: &str, path: &Path) -> Result<SessionInfo> {
    let raw = fs::read_to_string(path)?;
    let mut events = 0usize;
    let mut start: Option<SessionStart> = None;
    let mut preview = String::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        events += 1;
        let event: SessionEvent = serde_json::from_str(line).map_err(Error::msg)?;
        if start.is_none() {
            if let SessionEvent::Start(s) = event {
                start = Some(s);
                continue;
            }
        }
        if preview.is_empty() {
            if let SessionEvent::User(u) = event {
                if !is_hidden_user_text(&u.text) {
                    preview = u.text.clone();
                }
            }
        }
    }
    let start = start.ok_or_else(|| Error::msg("missing session/start"))?;
    let mtime = fs::metadata(path)?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stored = read_meta(dir, id).title;
    let title = if stored.is_empty() {
        title_from_text(&preview)
    } else {
        stored
    };
    Ok(SessionInfo {
        id: id.to_string(),
        mode: start.mode,
        workspace: start.workspace,
        channel: start.channel,
        title,
        mtime,
        events,
        preview: clip(&preview, 80),
    })
}

fn read_meta(dir: &Path, id: &str) -> SessionMeta {
    let path = meta_path(dir, id);
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.meta.json"))
}

fn clip(s: &str, n: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n {
        t
    } else {
        format!(
            "{}…",
            t.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    }
}

/// Short natural-language label from a user prompt. Empty if there is nothing to show.
pub fn title_from_text(text: &str) -> String {
    if is_hidden_user_text(text) {
        return String::new();
    }
    let mut parts: Vec<&str> = Vec::new();
    let mut attached: Option<&str> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Some(rest) = l.strip_prefix("[attached:") {
            let p = rest.trim().trim_end_matches(']').trim();
            if attached.is_none() {
                attached = Some(p);
            }
            continue;
        }
        parts.push(l);
    }
    let mut t = parts.join(" ");
    if let Some(rest) = t.strip_prefix("[heartbeat]") {
        t = rest.trim().to_string();
    } else if let Some(rest) = t.strip_prefix("[cron:") {
        t = rest
            .find(']')
            .map(|i| rest[i + 1..].trim().to_string())
            .unwrap_or_else(|| rest.trim().to_string());
    }
    let t = t.trim();
    if t.starts_with('/') {
        return String::new();
    }
    if !t.is_empty() {
        return clip(t, TITLE_CHARS);
    }
    if let Some(p) = attached {
        let name = p.rsplit(['/', '\\']).next().unwrap_or(p).trim();
        if !name.is_empty() {
            return clip(name, TITLE_CHARS);
        }
    }
    String::new()
}

/// Directory used by [`SessionLog::sessions_dir`].
pub fn sessions_dir() -> Result<PathBuf> {
    SessionLog::sessions_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::event::{SessionMode, SessionStart};
    use crate::session::{new_session_id, SessionLog};

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("q38-cat-{}", new_session_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn list_title_delete() {
        let dir = tmp();
        let id = new_session_id();
        let mut log = SessionLog::create_in(
            &dir,
            SessionStart::new(
                &id,
                "/ws",
                SessionMode::Agent,
                "sys",
                "h",
                SessionMode::Agent.default_policy(),
            ),
        )
        .unwrap();
        log.append(SessionEvent::user("fix the cache miss"))
            .unwrap();
        let listed = list(&dir).unwrap();
        assert_eq!(listed[0].title, "fix the cache miss");
        set_title(&dir, &id, "cache work").unwrap();
        let listed = list(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "cache work");
        assert!(listed[0].preview.contains("cache"));
        assert_eq!(resolve(&dir, "cache").unwrap().unwrap().id, id);
        delete(&dir, &id).unwrap();
        assert!(list(&dir).unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_gcs_blobs_only_when_unreferenced() {
        use crate::tools::BlobStore;
        let root = tmp();
        let dir = root.join("sessions");
        let blob_dir = root.join("blobs");
        fs::create_dir_all(&dir).unwrap();
        let store = BlobStore::new(&blob_dir);
        let shared = store.put(b"shared body").unwrap();
        let only_a = store.put(b"only in a").unwrap();

        let mk = |id: &str, shas: &[&str]| {
            let mut log = SessionLog::create_in(
                &dir,
                SessionStart::new(
                    id,
                    "/ws",
                    SessionMode::Agent,
                    "sys",
                    "h",
                    SessionMode::Agent.default_policy(),
                ),
            )
            .unwrap();
            for (i, sha) in shas.iter().enumerate() {
                log.append(SessionEvent::tool_folded(
                    format!("c{i}"),
                    "read",
                    "head…tail",
                    Some((*sha).to_string()),
                    Some(9_000),
                ))
                .unwrap();
            }
        };
        mk("a", &[&shared, &only_a]);
        mk("b", &[&shared]);

        delete(&dir, "a").unwrap();
        assert!(
            !blob_dir.join(&only_a).exists(),
            "unreferenced blob must be GC'd"
        );
        assert!(
            blob_dir.join(&shared).exists(),
            "blob still referenced by b must survive"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_skips_gc_when_any_jsonl_is_corrupt() {
        use crate::tools::BlobStore;
        let root = tmp();
        let dir = root.join("sessions");
        let blob_dir = root.join("blobs");
        fs::create_dir_all(&dir).unwrap();
        let store = BlobStore::new(&blob_dir);
        let sha = store.put(b"maybe referenced").unwrap();

        let mut log = SessionLog::create_in(
            &dir,
            SessionStart::new(
                "a",
                "/ws",
                SessionMode::Agent,
                "sys",
                "h",
                SessionMode::Agent.default_policy(),
            ),
        )
        .unwrap();
        log.append(SessionEvent::tool_folded(
            "c0",
            "read",
            "head…tail",
            Some(sha.clone()),
            Some(9_000),
        ))
        .unwrap();
        // 一份坏 jsonl：无法证明它不引用该 blob，GC 必须整体放弃，
        // 但会话删除本身照常成功。
        fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();

        delete(&dir, "a").unwrap();
        assert!(!dir.join("a.jsonl").exists());
        assert!(
            blob_dir.join(&sha).exists(),
            "corrupt sibling jsonl must abort GC, not delete blobs"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn title_from_first_user_skips_slash_and_hidden() {
        assert_eq!(title_from_text("帮我看看这段 rust"), "帮我看看这段 rust");
        assert_eq!(title_from_text("/status"), "");
        assert_eq!(
            title_from_text("<tool_response>\nhidden\n</tool_response>"),
            ""
        );
        assert_eq!(title_from_text("[attached: src/lib.rs]"), "lib.rs");
        assert_eq!(
            title_from_text("[cron:nightly] 清一下构建缓存"),
            "清一下构建缓存"
        );
        let long = "啊".repeat(40);
        let got = title_from_text(&long);
        assert!(got.ends_with('…'), "{got}");
        assert!(got.chars().count() <= 32, "{got}");
    }

    fn write_session(dir: &Path, channel: &str, user: Option<&str>) -> String {
        let id = new_session_id();
        let mut start = SessionStart::new(
            &id,
            "/ws",
            SessionMode::Agent,
            "sys",
            "h",
            SessionMode::Agent.default_policy(),
        );
        start.channel = channel.into();
        let mut log = SessionLog::create_in(dir, start).unwrap();
        if let Some(text) = user {
            log.append(SessionEvent::user(text)).unwrap();
        }
        id
    }

    #[test]
    fn preferred_console_uses_remembered_then_skips_empty_and_im() {
        let dir = tmp();
        let chat = write_session(&dir, "console", Some("fix the cache miss"));
        let _qq = write_session(&dir, "qq", Some("from the group"));
        let empty = write_session(&dir, "console", None);

        assert_eq!(preferred_console(&dir).unwrap().unwrap().id, chat);
        assert_eq!(resume_console_id(&dir, ""), chat);
        assert_eq!(resume_console_id(&dir, "  explicit-id  "), "explicit-id");

        remember(&dir, &empty).unwrap();
        assert_eq!(remembered(&dir).as_deref(), Some(empty.as_str()));
        assert_eq!(preferred_console(&dir).unwrap().unwrap().id, empty);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn set_title_does_not_require_session_to_be_open() {
        let dir = tmp();
        let a = write_session(&dir, "console", Some("keep my name"));
        let b = write_session(&dir, "console", Some("rename me"));
        set_title(&dir, &b, "新标题").unwrap();
        assert_eq!(title_of(&dir, &b), "新标题");
        assert!(title_of(&dir, &a).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn is_console_channel_treats_cli_as_console_not_im() {
        assert!(is_console_channel("cli"));
        assert!(is_console_channel("console"));
        assert!(!is_console_channel("qq"));
        assert!(!is_console_channel("im"));
    }
}
