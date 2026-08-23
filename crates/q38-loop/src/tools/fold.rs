//! Ingest-time fold: live context keeps head+tail; the omitted body is a blob.
//!
//! Rewriting an old tool message later would miss the prefix cache. Fold once
//! when the tool returns, then never touch that message again.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::vendor::sha256_hex;

use super::{cleanup_stale_tmp, ToolLimits};

/// Live-window cap for tool output, even when `result_max_chars` is higher.
const LIVE_RESULT_MAX_CHARS: usize = 12_000;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Content-addressed tool-result archive (`~/.q38-agent/blobs/{sha256}`).
#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let path = self.path_for(&sha)?;
        ensure_dir(&self.root)?;
        cleanup_stale_tmp(&self.root, &sha);
        if path.exists() {
            return Ok(sha);
        }
        let tmp = unique_tmp_path(&self.root, &sha);
        let guard = TmpGuard(tmp.clone());
        {
            let mut opts = fs::OpenOptions::new();
            opts.create(true).write(true).truncate(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let mut f = opts.open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        drop(guard);
        set_file_mode(&path)?;
        Ok(sha)
    }

    pub fn get(&self, sha: &str) -> Result<Vec<u8>> {
        let path = self.path_for(sha)?;
        Ok(fs::read(path)?)
    }

    pub fn get_text(&self, sha: &str) -> Result<String> {
        let bytes = self.get(sha)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn path_for(&self, sha: &str) -> Result<PathBuf> {
        if !is_sha256_hex(sha) {
            return Err(Error::msg("invalid blob id"));
        }
        Ok(self.root.join(sha))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folded {
    pub live: String,
    pub blob: Option<String>,
    pub original_chars: usize,
}

/// Truncate `text` for the live window. When it exceeds
/// `min(result_max_chars, LIVE_RESULT_MAX_CHARS)` and a store is provided,
/// the full original is written to a blob. Head/tail still come from `limits`.
pub fn fold_text(text: &str, limits: ToolLimits, blobs: Option<&BlobStore>) -> Folded {
    let original_chars = text.chars().count();
    let live_max = limits.result_max_chars.min(LIVE_RESULT_MAX_CHARS);
    if original_chars <= live_max {
        return Folded {
            live: text.to_string(),
            blob: None,
            original_chars,
        };
    }
    let head_n = limits.result_head_chars.min(original_chars);
    let tail_n = limits
        .result_tail_chars
        .min(original_chars.saturating_sub(head_n));
    if head_n + tail_n >= original_chars {
        return Folded {
            live: text.to_string(),
            blob: None,
            original_chars,
        };
    }
    let blob = blobs.and_then(|store| store.put(text.as_bytes()).ok());
    let head: String = text.chars().take(head_n).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(tail_n)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let omitted = original_chars - head_n - tail_n;
    let marker = match &blob {
        Some(sha) => format!("\n…[{omitted} chars omitted blob={sha}]…\n"),
        None => format!("\n…[{omitted} chars omitted]…\n"),
    };
    Folded {
        live: format!("{head}{marker}{tail}"),
        blob,
        original_chars,
    }
}

pub(super) fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `{stem}.q38tmp.{pid}.{nanos}.{uuid}` next to the blob. Two writers cannot
/// share `{sha}.q38tmp` the way `path.with_extension("q38tmp")` did.
fn unique_tmp_path(dir: &Path, stem: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let rand = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("{stem}.q38tmp.{}.{}.{}", std::process::id(), now, rand);
    dir.join(name)
}

struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn ensure_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn set_file_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tight() -> ToolLimits {
        ToolLimits {
            read_default_lines: 2000,
            result_max_chars: 20,
            result_head_chars: 8,
            result_tail_chars: 4,
        }
    }

    #[test]
    fn fold_high_result_max_still_folds_20k() {
        let dir = std::env::temp_dir().join(format!("q38-blobs-{}", uuid::Uuid::new_v4().simple()));
        let store = BlobStore::new(&dir);
        let limits = ToolLimits {
            read_default_lines: 2000,
            result_max_chars: 50_000,
            result_head_chars: 8_000,
            result_tail_chars: 2_000,
        };
        let text = "x".repeat(20_000);
        let f = fold_text(&text, limits, Some(&store));
        let sha = f.blob.as_deref().expect("blob id");
        assert!(
            f.live.chars().count() < 20_000,
            "20k text must fold under the 12k live cap even when result_max_chars is 50k"
        );
        assert!(f.live.contains("omitted") && f.live.contains(sha));
        assert_eq!(f.original_chars, 20_000);
        assert_eq!(store.get_text(sha).unwrap(), text);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn short_text_is_unchanged() {
        let f = fold_text("hello", tight(), None);
        assert_eq!(f.live, "hello");
        assert!(f.blob.is_none());
        assert_eq!(f.original_chars, 5);
    }

    #[test]
    fn fold_without_store_drops_middle() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let f = fold_text(text, tight(), None);
        assert!(f.live.starts_with("abcdefgh"));
        assert!(f.live.ends_with("wxyz"));
        assert!(f.live.contains("omitted"));
        assert!(!f.live.contains("ijklmnop"));
        assert!(f.blob.is_none());
        assert_eq!(f.original_chars, 26);
    }

    #[test]
    fn fold_with_store_roundtrips_full_text() {
        let dir = std::env::temp_dir().join(format!("q38-blobs-{}", uuid::Uuid::new_v4().simple()));
        let store = BlobStore::new(&dir);
        let text = "abcdefghijklmnopqrstuvwxyz";
        let f = fold_text(text, tight(), Some(&store));
        let sha = f.blob.as_deref().expect("blob id");
        assert!(f.live.contains(sha));
        assert_eq!(store.get_text(sha).unwrap(), text);
        assert_eq!(store.put(text.as_bytes()).unwrap(), sha);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_path_escape_blob_id() {
        let dir = std::env::temp_dir().join(format!("q38-blobs-{}", uuid::Uuid::new_v4().simple()));
        let store = BlobStore::new(&dir);
        assert!(store.get("../passwd").is_err());
        assert!(store.get("abcd").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unique_tmp_paths_are_distinct() {
        let dir = std::env::temp_dir().join(format!("q38-blobs-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let p = unique_tmp_path(&dir, "abcd");
            assert!(seen.insert(p.clone()), "duplicate {}", p.display());
            assert!(p
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("abcd.q38tmp."));
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn put_leaves_no_tmp_and_cleans_foreign_pid() {
        let dir = std::env::temp_dir().join(format!("q38-blobs-{}", uuid::Uuid::new_v4().simple()));
        let store = BlobStore::new(&dir);
        let bytes = b"abcdefghijklmnopqrstuvwxyz";
        let sha = store.put(bytes).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".q38tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let me = std::process::id();
        let other = if me == 1 { 2 } else { 1 };
        let stale = dir.join(format!("{sha}.q38tmp.{other}.0.deadbeef"));
        fs::write(&stale, b"crash").unwrap();
        let f = fs::File::options().write(true).open(&stale).unwrap();
        f.set_modified(SystemTime::now() - std::time::Duration::from_secs(400))
            .unwrap();
        assert_eq!(store.put(bytes).unwrap(), sha);
        assert!(!stale.exists(), "foreign leftover not cleaned");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tmp_guard_removes_our_tmp_on_drop() {
        let dir = std::env::temp_dir().join(format!("q38-blobs-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("x.q38tmp.1.0.gone");
        fs::write(&tmp, b"x").unwrap();
        {
            let _guard = TmpGuard(tmp.clone());
        }
        assert!(!tmp.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
