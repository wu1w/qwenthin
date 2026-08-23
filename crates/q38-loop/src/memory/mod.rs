//! Durable memory files. Session JSONL stays the evidence; these files are
//! conclusions the model may `read` or `memory_search`.
//!
//! - `MEMORY.md` — short, human-editable, **not** FTS-indexed, **not** in system
//! - `memory/YYYY-MM-DD/*.md` — compact snapshots (code-extracted)
//! - `digest/{personal,procedure,wiki}/` — idle/manual notes, BM25/FTS
//!
//! Default is zero recall. Harness may pin a ≤12-line hot card as a hidden user
//! after the live query. Never rewrite MEMORY.md on a timer.

mod index;

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools::{arg_str, folded_response, ToolLimits};

pub use index::{MemoryHit, MemoryIndex};

const MEMORY_STUB: &str = "\
# Prefs

# Hosts

# Decisions
";

#[derive(Clone, Debug)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let store = Self { root };
        store.ensure_layout()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn memory_md(&self) -> PathBuf {
        self.root.join("MEMORY.md")
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for sub in [
            "memory",
            "digest/personal",
            "digest/procedure",
            "digest/wiki",
            "skills",
        ] {
            std::fs::create_dir_all(self.root.join(sub))?;
        }
        let mem = self.memory_md();
        if !mem.exists() {
            std::fs::write(mem, MEMORY_STUB)?;
        }
        if let Ok(idx) = MemoryIndex::open(&self.root) {
            let _ = idx.reindex_tree(&self.root);
        }
        Ok(())
    }

    pub fn index(&self) -> Result<MemoryIndex> {
        MemoryIndex::open(&self.root)
    }

    /// Code-extracted compact slice. Not model-written. No thinking.
    pub fn write_compact_note(
        &self,
        session_id: &str,
        until_seq: u64,
        body: &str,
    ) -> Result<PathBuf> {
        let day = chrono_today();
        let dir = self.root.join("memory").join(&day);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{session_id}-{until_seq}.md"));
        let text = format!("# compact {session_id} until={until_seq}\n\n{body}\n");
        std::fs::write(&path, &text)?;
        if let Ok(idx) = MemoryIndex::open(&self.root) {
            let rel = format!("memory/{day}/{session_id}-{until_seq}.md");
            let _ = idx.upsert(&rel, "daily", &text);
        }
        Ok(path)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryHit>> {
        MemoryIndex::open(&self.root)?.search(query, limit)
    }

    pub fn read_memory_md(&self) -> Option<String> {
        std::fs::read_to_string(self.memory_md()).ok()
    }
}

pub fn run_memory_search(store: &MemoryStore, call: &ToolCall, limits: ToolLimits) -> ToolResponse {
    let query = arg_str(&call.arguments, "query").unwrap_or_default();
    if query.trim().is_empty() {
        return ToolResponse::text(
            &call.id,
            "Error: memory_search needs `query`.",
            ToolState::Error,
        );
    }
    match store.search(&query, 8) {
        Ok(hits) if hits.is_empty() => ToolResponse::text(
            &call.id,
            "No matches in digest/daily notes. MEMORY.md is not indexed.",
            ToolState::Success,
        ),
        Ok(hits) => {
            let mut text = String::new();
            for h in hits {
                text.push_str(&format!(
                    "path={} kind={}\n{}\n\n",
                    h.path,
                    h.kind,
                    h.snippet.trim()
                ));
            }
            folded_response(&call.id, text, ToolState::Success, limits, None)
        }
        Err(e) => ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error),
    }
}

/// Hidden-user MEMORY card, or `None` when this turn should stay zero-recall.
pub fn card_for(user: &str, md: &str) -> Option<String> {
    let md = md.trim();
    if md.is_empty() || !has_facts(md) {
        return None;
    }
    let prefs_hit = wants_prefs(user);
    let hosts_hit = wants_hosts(user);
    let full_hit = wants_full(user);
    if !prefs_hit && !hosts_hit && !full_hit {
        return None;
    }
    let prefs = section(md, "Prefs");
    let hosts = section(md, "Hosts");
    let decisions = section(md, "Decisions");
    let lines = md.lines().filter(|l| !l.trim().is_empty()).count();

    if full_hit {
        if lines <= crate::sticky::MEMORY_FULL_MAX_LINES {
            return Some(format!("MEMORY.md\n{}", md.trim()));
        }
        return join_card(
            "MEMORY.md",
            &[
                take_lines(&prefs, crate::sticky::MEMORY_HOT_MAX_LINES),
                take_lines(&hosts, crate::sticky::MEMORY_HOT_MAX_LINES),
                take_lines(&decisions, crate::sticky::MEMORY_HOT_MAX_LINES),
            ],
        );
    }
    if hosts_hit && prefs_hit {
        return join_card(
            "MEMORY hot (do not restate, do not expand):",
            &[
                take_lines(&prefs, crate::sticky::MEMORY_HOT_MAX_LINES),
                take_lines(&hosts, crate::sticky::MEMORY_HOT_MAX_LINES),
            ],
        );
    }
    if hosts_hit {
        let body = take_lines(&hosts, crate::sticky::MEMORY_HOT_MAX_LINES);
        if body.is_empty() {
            return None;
        }
        return Some(format!("MEMORY hosts:\n{body}"));
    }
    let body = if !prefs.is_empty() {
        take_lines(&prefs, crate::sticky::MEMORY_HOT_MAX_LINES)
    } else if !hosts.is_empty() || !decisions.is_empty() {
        return None;
    } else {
        take_lines(md, crate::sticky::MEMORY_HOT_MAX_LINES)
    };
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "MEMORY hot (do not restate, do not expand):\n{body}"
    ))
}

fn wants_prefs(user: &str) -> bool {
    const CJK: &[&str] = &[
        "习惯",
        "偏好",
        "风格",
        "提交",
        "按我的来",
        "按我的习惯",
        "一直",
    ];
    contains_any(user, CJK)
        || ascii_words(user).any(|w| {
            matches!(
                w.as_str(),
                "commit"
                    | "commits"
                    | "conventional"
                    | "changelog"
                    | "style"
                    | "preference"
                    | "preferences"
            )
        })
}

fn wants_hosts(user: &str) -> bool {
    const CJK: &[&str] = &["主机", "多机", "部署"];
    contains_any(user, CJK)
        || user.contains("dev 机器")
        || ascii_words(user).any(|w| {
            matches!(
                w.as_str(),
                "ssh" | "scp" | "jumphost" | "deploy" | "host" | "hosts"
            )
        })
}

fn ascii_words(hay: &str) -> impl Iterator<Item = String> + '_ {
    hay.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
}

fn wants_full(user: &str) -> bool {
    const CJK: &[&str] = &["你记得", "上次说过", "按 MEMORY"];
    let lower = user.to_ascii_lowercase();
    contains_any(user, CJK) || lower.contains("memory.md") || lower.contains("按 memory")
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

fn has_facts(md: &str) -> bool {
    md.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    })
}

fn section(md: &str, name: &str) -> String {
    let heading = format!("# {name}");
    let heading2 = format!("## {name}");
    let mut lines = md.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim();
        if t.eq_ignore_ascii_case(&heading) || t.eq_ignore_ascii_case(&heading2) {
            let mut body = Vec::new();
            while let Some(next) = lines.peek() {
                let n = next.trim();
                if n.starts_with('#') {
                    break;
                }
                body.push(*next);
                lines.next();
            }
            return body.join("\n").trim().to_string();
        }
    }
    String::new()
}

fn take_lines(text: &str, max: usize) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .take(max)
        .collect::<Vec<_>>()
        .join("\n")
}

fn join_card(title: &str, parts: &[String]) -> Option<String> {
    let body = parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if body.is_empty() {
        None
    } else {
        Some(format!("{title}\n{body}"))
    }
}

fn chrono_today() -> String {
    // Local calendar date without extra deps: YYYY-MM-DD from local time.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let offset = local_offset_secs();
    let days = (secs + offset) / 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn local_offset_secs() -> i64 {
    // `localtime` via `chrono` is not a dep; use 0 (UTC) for filenames if we
    // cannot read. macOS/Linux: parse `date +%z` is overkill. UTC is stable.
    0
}

/// Howard Hinnant civil_from_days (UTC days since 1970-01-01).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_md_is_not_searchable() {
        let dir = std::env::temp_dir().join(format!("q38-mem-{}", uuid::Uuid::new_v4().simple()));
        let store = MemoryStore::open(&dir).unwrap();
        std::fs::write(store.memory_md(), "I always use zirconium-pref\n").unwrap();
        store
            .write_compact_note("s", 3, "read crates/foo.rs linker rewrite")
            .unwrap();
        assert!(store.search("zirconium-pref", 8).unwrap().is_empty());
        let hits = store.search("linker", 8).unwrap();
        assert!(hits.iter().any(|h| h.kind == "daily"), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hot_card_skips_local_questions() {
        let md = "# Prefs\n- 回复中文\n# Hosts\n- ssh dev = ops@192.0.2.8\n";
        assert!(card_for("这个函数 off-by-one 吗？只答是或否。", md).is_none());
        let commit = card_for("写一条 commit 标题", md).unwrap();
        assert!(commit.starts_with("MEMORY hot"));
        assert!(commit.contains("回复中文"));
        assert!(!commit.contains("192.0.2.8"));
        let host = card_for("dev ssh 地址？", md).unwrap();
        assert!(host.contains("192.0.2.8"));
        let full = card_for("按 MEMORY.md 来", md).unwrap();
        assert!(full.starts_with("MEMORY.md"));
        assert!(full.contains("回复中文"));
        assert!(full.contains("192.0.2.8"));
        assert!(card_for("ghost in the shell", md).is_none());
        let hosts_only = "# Prefs\n\n# Hosts\n- ssh = ops@192.0.2.8\n";
        assert!(card_for("写一条 commit 标题", hosts_only).is_none());
        assert!(card_for("dev ssh 地址？", hosts_only)
            .unwrap()
            .contains("192.0.2.8"));
    }
}
