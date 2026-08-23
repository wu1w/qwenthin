//! Workspace path resolution. Relative paths join the root; `..` is lexical.
//!
//! Confinement is symlink-aware: for confined workspaces the resolved path is
//! canonicalized (existing parts, following symlinks) before the within-root
//! check, so an in-repo symlink pointing out of the repo cannot smuggle a
//! read/write past the root.

use std::path::{Component, Path, PathBuf};

use crate::error::Result;

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    confined: bool,
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>, confined: bool) -> Result<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)?;
        Ok(Self {
            root: root.canonicalize()?,
            confined,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn display(&self) -> String {
        self.root.display().to_string()
    }

    pub fn resolve(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        if raw.is_empty() {
            return Err("Error: No `path` provided.".into());
        }
        let joined = {
            let p = Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.root.join(p)
            }
        };
        let normalized = lexical_normalize(&joined);
        if self.confined {
            return self.check_confined(&normalized);
        }
        Ok(normalized)
    }

    /// Symlink-aware confinement check. Returns the path read/write must use.
    ///
    /// `canonicalize` only works on paths that exist, so:
    /// - existing path → canonicalize it (follows symlinks at every level)
    ///   and require the result to sit under the canonical root;
    /// - missing path (fresh write) → canonicalize the deepest *existing*
    ///   ancestor and re-append the absent tail lexically. This still catches
    ///   a symlinked directory: `hole -> /outside` with `hole/new.txt`
    ///   canonicalizes `hole` to `/outside` and fails the check, even though
    ///   `hole/new.txt` does not exist yet.
    fn check_confined(&self, normalized: &Path) -> std::result::Result<PathBuf, String> {
        let mut probe = normalized.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            match std::fs::canonicalize(&probe) {
                Ok(real) => {
                    let mut full = real;
                    for name in tail.iter().rev() {
                        full.push(name);
                    }
                    return if is_within(&full, &self.root) {
                        Ok(full)
                    } else {
                        Err(format!(
                            "Error: path `{}` is outside the workspace (resolves to {}).",
                            normalized.display(),
                            full.display()
                        ))
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    match (probe.file_name(), probe.parent()) {
                        (Some(name), Some(parent)) => {
                            tail.push(name.to_os_string());
                            probe = parent.to_path_buf();
                        }
                        _ => return Ok(probe),
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "Error: cannot resolve `{}`: {}",
                        normalized.display(),
                        e
                    ));
                }
            }
        }
    }

    /// Path the model should see: the argument it sent, not a host absolute.
    pub fn shown(&self, raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return trimmed.to_string();
        }
        let p = Path::new(trimmed);
        if !p.is_absolute() {
            return trimmed.to_string();
        }
        match self.resolve(trimmed) {
            Ok(resolved) => resolved
                .strip_prefix(&self.root)
                .ok()
                .map(|rel| {
                    let s = rel.to_string_lossy();
                    if s.is_empty() {
                        ".".to_string()
                    } else {
                        s.replace('\\', "/")
                    }
                })
                .unwrap_or_else(|| trimmed.to_string()),
            Err(_) => trimmed.to_string(),
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Component-wise containment, *not* string prefix.
/// `Path::strip_prefix` compares whole path components, so
/// `/workspace-evil` is NOT within `/workspace` even though the byte string
/// shares the prefix. Never reimplement this with `str::starts_with`.
fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.strip_prefix(root).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("q38-path-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn is_within_is_component_based_not_string_prefix() {
        assert!(is_within(Path::new("/workspace"), Path::new("/workspace")));
        assert!(is_within(
            Path::new("/workspace/a.txt"),
            Path::new("/workspace")
        ));
        assert!(!is_within(
            Path::new("/workspace-evil"),
            Path::new("/workspace")
        ));
        assert!(!is_within(
            Path::new("/workspace-evil/a"),
            Path::new("/workspace")
        ));
        assert!(!is_within(Path::new("/other"), Path::new("/workspace")));
    }

    #[test]
    fn missing_paths_stay_lexical_new_writes_allowed() {
        let d = temp_dir("missing");
        let ws = Workspace::open(&d, true).unwrap();
        let p = ws.resolve("brand/new/file.txt").unwrap();
        assert!(p.starts_with(ws.root()));
        assert!(p.ends_with("brand/new/file.txt"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn lexical_dotdot_escape_is_rejected() {
        let d = temp_dir("dotdot");
        let ws = Workspace::open(&d, true).unwrap();
        let err = ws.resolve("a/../../outside.txt").unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn in_repo_symlink_pointing_out_is_rejected() {
        use std::os::unix::fs::symlink;

        let d = temp_dir("sym");
        let outside =
            std::env::temp_dir().join(format!("q38-path-outside-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "top secret").unwrap();

        let ws = Workspace::open(&d, true).unwrap();

        let leak = d.join("leak.txt");
        symlink(&outside_file, &leak).unwrap();
        let err = ws.resolve("leak.txt").unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");

        let hole = d.join("hole");
        symlink(&outside, &hole).unwrap();
        let err = ws.resolve("hole/new.txt").unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");

        let inner = d.join("inner.txt");
        std::fs::write(&inner, "ok").unwrap();
        let good = d.join("good.txt");
        symlink(&inner, &good).unwrap();
        let p = ws.resolve("good.txt").unwrap();
        assert!(is_within(&p, ws.root()));

        let ws_open = Workspace::open(&d, false).unwrap();
        assert!(ws_open.resolve("leak.txt").is_ok());

        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
