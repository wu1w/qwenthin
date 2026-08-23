//! `read` / `write` / `edit`. Unique `old_string` (exactly one match), atomic write.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    arg_new_string, arg_old_string, arg_path, arg_str, arg_u32, cleanup_stale_tmp, folded_response,
    BlobStore, ToolLimits, Workspace,
};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::vendor::sha256_hex;

pub fn read_file(
    ws: &Workspace,
    call: &ToolCall,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(raw) = arg_path(&call.arguments) else {
        return ToolResponse::text(&call.id, "Error: No `path` provided.", ToolState::Error);
    };
    let path = match ws.resolve(&raw) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };

    if crate::media::is_media_ext(&raw) {
        return ToolResponse::text(
            &call.id,
            format!(
                "Error: {} looks like a media file. Use view(path) for images, video stills, or an audio transcript.",
                ws.shown(&raw)
            ),
            ToolState::Error,
        );
    }

    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ToolResponse::text(
                &call.id,
                format!("Error: The file {} does not exist.", ws.shown(&raw)),
                ToolState::Error,
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => {
            return ToolResponse::text(
                &call.id,
                format!("Error: The path {} is not a file.", ws.shown(&raw)),
                ToolState::Error,
            );
        }
        Err(e) => {
            return ToolResponse::text(
                &call.id,
                format!("Error: Read file failed due to \n{e}"),
                ToolState::Error,
            );
        }
    };

    let lines: Vec<&str> = content.split('\n').collect();
    let total = lines.len();
    let start = arg_u32(&call.arguments, "offset")
        .or_else(|| arg_u32(&call.arguments, "start_line"))
        .map(|n| n.max(1))
        .unwrap_or(1) as usize;
    let requested = match arg_u32(&call.arguments, "end_line") {
        Some(end) if (end as usize) >= start => (end as usize) - start + 1,
        Some(end) => {
            return ToolResponse::text(
                &call.id,
                format!("Error: end_line {end} is before start_line {start}."),
                ToolState::Error,
            );
        }
        None => arg_u32(&call.arguments, "limit").unwrap_or(limits.read_default_lines) as usize,
    };
    let cap = read_page_cap(limits);
    let capped = requested > cap;
    let limit = requested.min(cap);
    let end = (start.saturating_sub(1) + limit).min(total);

    if start > total {
        return ToolResponse::text(
            &call.id,
            format!("Error: start_line {start} exceeds file length ({total} lines)."),
            ToolState::Error,
        );
    }

    let selected: Vec<String> = lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}|{}", start + i, line))
        .collect();
    let mut text = selected.join("\n");
    let sha = sha256_hex(content.as_bytes());
    text.push_str(&format!("\n[q38 sha256={}]", &sha[..12]));
    if end < total {
        text.push_str(&format!(
            " [continue with offset={} to read the rest; {total} lines total]",
            end + 1
        ));
    }
    if capped {
        text.push_str(&format!(
            " [limit capped at {cap} lines; pass offset to page instead of a huge limit]"
        ));
    }
    folded_response(&call.id, text, ToolState::Success, limits, blobs)
}

fn read_page_cap(limits: ToolLimits) -> usize {
    (limits.read_default_lines as usize)
        .saturating_mul(2)
        .max(1)
}

pub fn write_file(ws: &Workspace, call: &ToolCall) -> ToolResponse {
    let Some(raw) = arg_path(&call.arguments) else {
        return ToolResponse::text(&call.id, "Error: No `path` provided.", ToolState::Error);
    };
    let Some(content) = arg_str(&call.arguments, "content") else {
        return ToolResponse::text(&call.id, "Error: No `content` provided.", ToolState::Error);
    };
    if crate::stutter::is_placeholder_write(&raw, &content) {
        return ToolResponse::text(&call.id, "Error: invalid path.", ToolState::Error);
    }
    let path = match ws.resolve(&raw) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    match write_atomic(&path, &content) {
        Ok(()) => ToolResponse::text(
            &call.id,
            format!("Wrote {} bytes to {}.", content.len(), raw),
            ToolState::Success,
        ),
        Err(e) => ToolResponse::text(
            &call.id,
            format!("Error: Write file failed due to \n{e}"),
            ToolState::Error,
        ),
    }
}

pub fn edit_file(ws: &Workspace, call: &ToolCall) -> ToolResponse {
    let Some(raw) = arg_path(&call.arguments) else {
        return ToolResponse::text(&call.id, "Error: No `path` provided.", ToolState::Error);
    };
    let Some(old) = arg_old_string(&call.arguments) else {
        return ToolResponse::text(
            &call.id,
            "Error: No `old_string` provided.",
            ToolState::Error,
        );
    };
    if old.is_empty() {
        return ToolResponse::text(
            &call.id,
            "Error: `old_string` must be non-empty.",
            ToolState::Error,
        );
    }
    let Some(new) = arg_new_string(&call.arguments) else {
        return ToolResponse::text(
            &call.id,
            "Error: No `new_string` provided.",
            ToolState::Error,
        );
    };
    let path = match ws.resolve(&raw) {
        Ok(p) => p,
        Err(e) => return ToolResponse::text(&call.id, e, ToolState::Error),
    };
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ToolResponse::text(
                &call.id,
                format!("Error: The file {} does not exist.", ws.shown(&raw)),
                ToolState::Error,
            );
        }
        Err(e) => {
            return ToolResponse::text(
                &call.id,
                format!("Error: Read file failed due to \n{e}"),
                ToolState::Error,
            );
        }
    };
    let (updated, normalized_newlines) = if old.contains('\r') || old.contains('\n') {
        match replace_unique_newline_agnostic(&content, &old, &new) {
            Ok(result) => result,
            Err(0) => {
                return ToolResponse::text(
                    &call.id,
                    format!("Error: The text to replace was not found in {raw}."),
                    ToolState::Error,
                );
            }
            Err(n) => {
                return ToolResponse::text(
                    &call.id,
                    format!(
                        "Error: `old_string` matched {n} times in {raw}; provide a longer, more unique `old_string` so the edit targets exactly one location."
                    ),
                    ToolState::Error,
                );
            }
        }
    } else {
        let count = content.matches(&old).count();
        match count {
            0 => {
                return ToolResponse::text(
                    &call.id,
                    format!("Error: The text to replace was not found in {raw}."),
                    ToolState::Error,
                );
            }
            1 => (content.replacen(&old, &new, 1), false),
            n => {
                return ToolResponse::text(
                    &call.id,
                    format!(
                        "Error: `old_string` matched {n} times in {raw}; provide a longer, more unique `old_string` so the edit targets exactly one location."
                    ),
                    ToolState::Error,
                );
            }
        }
    };
    match write_atomic(&path, &updated) {
        Ok(()) => ToolResponse::text(
            &call.id,
            if normalized_newlines {
                format!("Successfully replaced text in {raw} (preserved file line endings).")
            } else {
                format!("Successfully replaced text in {raw}.")
            },
            ToolState::Success,
        ),
        Err(e) => ToolResponse::text(
            &call.id,
            format!("Error: Write file failed due to \n{e}"),
            ToolState::Error,
        ),
    }
}

/// Match a multiline edit after treating LF, CRLF and lone CR as the same
/// logical newline, then splice only the matched byte range in the original
/// string. This keeps every byte outside the edit unchanged and writes the new
/// fragment using the matched file region's line-ending style.
fn replace_unique_newline_agnostic(
    content: &str,
    old: &str,
    new: &str,
) -> std::result::Result<(String, bool), usize> {
    let (normalized_content, boundaries) = normalize_newlines_with_boundaries(content);
    let normalized_old = normalize_newlines(old);
    let mut matches = normalized_content.match_indices(&normalized_old);
    let Some((start, _)) = matches.next() else {
        return Err(0);
    };
    if matches.next().is_some() {
        return Err(normalized_content.matches(&normalized_old).count());
    }
    let end = start + normalized_old.len();
    let original_start = boundaries[start];
    let original_end = boundaries[end];
    let original_fragment = &content[original_start..original_end];
    let style = dominant_newline_style(original_fragment)
        .or_else(|| dominant_newline_style(content))
        .unwrap_or("\n");
    let replacement = normalize_newlines(new).replace('\n', style);
    let mut updated =
        String::with_capacity(content.len() - (original_end - original_start) + replacement.len());
    updated.push_str(&content[..original_start]);
    updated.push_str(&replacement);
    updated.push_str(&content[original_end..]);
    Ok((updated, original_fragment != old || replacement != new))
}

fn normalize_newlines(text: &str) -> String {
    normalize_newlines_with_boundaries(text).0
}

/// `boundaries[n]` is the original byte offset after `n` normalized bytes.
/// Newline conversion is ASCII-only, so UTF-8 byte boundaries remain stable.
fn normalize_newlines_with_boundaries(text: &str) -> (String, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut boundaries = Vec::with_capacity(bytes.len() + 1);
    boundaries.push(0);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            normalized.push(b'\n');
            i += if bytes.get(i + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
        } else {
            normalized.push(bytes[i]);
            i += 1;
        }
        boundaries.push(i);
    }
    // Replacing CR/CRLF with LF cannot produce invalid UTF-8 from valid input.
    (
        String::from_utf8(normalized).expect("normalized UTF-8"),
        boundaries,
    )
}

fn dominant_newline_style(text: &str) -> Option<&'static str> {
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut cr = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                crlf += 1;
                i += 2;
            }
            b'\r' => {
                cr += 1;
                i += 1;
            }
            b'\n' => {
                lf += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    [(crlf, "\r\n"), (lf, "\n"), (cr, "\r")]
        .into_iter()
        .max_by_key(|(count, _)| *count)
        .and_then(|(count, style)| (count > 0).then_some(style))
}

/// Unique temp next to the target, fsync, rename. Concurrent writers do not
/// share `{name}.q38tmp`.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("q38");

    let tmp = unique_tmp_path(&dir, stem);
    let guard = TmpGuard(tmp.clone());
    let res = write_then_rename(&tmp, path, content);
    drop(guard);
    if res.is_ok() {
        cleanup_stale_tmp(&dir, stem);
    }
    res
}

fn unique_tmp_path(dir: &Path, stem: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let rand = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("{stem}.q38tmp.{}.{}.{}", std::process::id(), now, rand);
    dir.join(name)
}

fn write_then_rename(tmp: &Path, path: &Path, content: &str) -> std::io::Result<()> {
    let mut f = fs::File::create(tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    fs::rename(tmp, path)
}

struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_calls::{ToolCall, ToolState};
    use serde_json::json;
    use std::time::Duration;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("q38-fs-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn workspace() -> (Workspace, PathBuf) {
        let dir = scratch();
        let w = Workspace::open(&dir, true).unwrap();
        (w, dir)
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn read_empty_file_returns_a_page() {
        let (ws, dir) = workspace();
        fs::write(dir.join("empty.txt"), "").unwrap();
        let r = read_file(
            &ws,
            &call("read", json!({"path": "empty.txt"})),
            ToolLimits::default(),
            None,
        );
        let t = r.joined_text();
        assert_eq!(r.state, ToolState::Success, "{t}");
        assert!(
            !t.contains("exceeds file length"),
            "empty file must not error as start > total: {t}"
        );
        assert!(t.contains("1|"), "{t}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_end_line_before_offset_is_error() {
        let (ws, dir) = workspace();
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let r = read_file(
            &ws,
            &call("read", json!({"path": "a.txt", "offset": 4, "end_line": 2})),
            ToolLimits::default(),
            None,
        );
        let t = r.joined_text();
        assert_eq!(r.state, ToolState::Error, "{t}");
        assert!(t.contains("end_line"), "{t}");
        assert!(t.contains("2"), "{t}");
        assert!(t.contains("4"), "{t}");
        assert!(!t.contains("one"), "{t}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_empty_old_string_is_error() {
        let (ws, dir) = workspace();
        fs::write(dir.join("empty.txt"), "").unwrap();
        let e = edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "path": "empty.txt",
                    "old_string": "",
                    "new_string": "oops"
                }),
            ),
        );
        let t = e.joined_text();
        assert_eq!(e.state, ToolState::Error, "{t}");
        assert!(t.contains("old_string"), "{t}");
        assert_eq!(fs::read_to_string(dir.join("empty.txt")).unwrap(), "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_lf_request_preserves_crlf_file() {
        let (ws, dir) = workspace();
        let path = dir.join("windows.py");
        fs::write(
            &path,
            b"def value():\r\n    return 1\r\n\r\nkeep = True\r\n",
        )
        .unwrap();
        let result = edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "path": "windows.py",
                    "old_string": "def value():\n    return 1",
                    "new_string": "def value():\n    return 2"
                }),
            ),
        );
        assert_eq!(result.state, ToolState::Success, "{}", result.joined_text());
        assert!(result.joined_text().contains("preserved file line endings"));
        assert_eq!(
            fs::read(&path).unwrap(),
            b"def value():\r\n    return 2\r\n\r\nkeep = True\r\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_crlf_request_preserves_lf_file() {
        let (ws, dir) = workspace();
        let path = dir.join("unix.py");
        fs::write(&path, b"def value():\n    return 1\nkeep = True\n").unwrap();
        let result = edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "path": "unix.py",
                    "old_string": "def value():\r\n    return 1",
                    "new_string": "def value():\r\n    return 2"
                }),
            ),
        );
        assert_eq!(result.state, ToolState::Success, "{}", result.joined_text());
        assert_eq!(
            fs::read(&path).unwrap(),
            b"def value():\n    return 2\nkeep = True\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_newline_normalization_keeps_uniqueness_guard() {
        let (ws, dir) = workspace();
        let path = dir.join("mixed.txt");
        fs::write(&path, b"a\r\nb\n--\na\nb\n").unwrap();
        let result = edit_file(
            &ws,
            &call(
                "edit",
                json!({
                    "path": "mixed.txt",
                    "old_string": "a\nb",
                    "new_string": "A\nB"
                }),
            ),
        );
        assert_eq!(result.state, ToolState::Error, "{}", result.joined_text());
        assert!(result.joined_text().contains("matched 2 times"));
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\n--\na\nb\n");
        let _ = fs::remove_dir_all(&dir);
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".q38tmp"))
            .collect()
    }

    fn age_file(path: &Path, secs: u64) {
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn unique_tmp_paths_are_distinct() {
        let dir = scratch();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let p = unique_tmp_path(&dir, "a.txt");
            let shown = p.display().to_string();
            assert!(seen.insert(p), "duplicate temp path: {shown}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_tmp_embeds_stem_and_pid() {
        let dir = scratch();
        let p = unique_tmp_path(&dir, "a.txt");
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("a.txt.q38tmp."), "{name}");
        assert!(name.contains(&std::process::id().to_string()), "{name}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_writes_content_and_leaves_no_tmp() {
        let dir = scratch();
        let target = dir.join("a.txt");
        write_atomic(&target, "hello").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
        write_atomic(&target, "world").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "world");
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_cleans_old_foreign_pid_leftover() {
        let dir = scratch();
        let target = dir.join("a.txt");
        let me = std::process::id();
        let other = if me == 1 { 2 } else { 1 };
        let stale = dir.join(format!("a.txt.q38tmp.{}.0.deadbeef", other));
        fs::write(&stale, b"crash").unwrap();
        age_file(&stale, 400);
        write_atomic(&target, "fresh").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "fresh");
        assert!(!stale.exists(), "old foreign leftover not cleaned");
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_keeps_recent_foreign_tmp() {
        let dir = scratch();
        let target = dir.join("a.txt");
        let me = std::process::id();
        let other = if me == 1 { 2 } else { 1 };
        let live = dir.join(format!("a.txt.q38tmp.{}.0.inflight", other));
        fs::write(&live, b"in-flight").unwrap();
        write_atomic(&target, "fresh").unwrap();
        assert!(
            live.exists(),
            "recent foreign tmp must survive (concurrent writer)"
        );
        let _ = fs::remove_file(&live);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_stale_keeps_same_process_tmp() {
        let dir = scratch();
        let me = std::process::id();
        let other = if me == 1 { 2 } else { 1 };
        let mine = dir.join(format!("a.txt.q38tmp.{}.0.mine", me));
        let theirs = dir.join(format!("a.txt.q38tmp.{}.0.theirs", other));
        fs::write(&mine, b"m").unwrap();
        fs::write(&theirs, b"t").unwrap();
        age_file(&theirs, 400);
        super::cleanup_stale_tmp(&dir, "a.txt");
        assert!(mine.exists(), "same-process temp must be preserved");
        assert!(!theirs.exists(), "old foreign temp must be removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tmp_guard_removes_our_tmp_on_drop() {
        let dir = scratch();
        let tmp = dir.join("a.txt.q38tmp.1.0.gone");
        fs::write(&tmp, b"x").unwrap();
        {
            let _guard = TmpGuard(tmp.clone());
        }
        assert!(!tmp.exists(), "guard should remove its temp");
        let _ = fs::remove_dir_all(&dir);
    }
}
