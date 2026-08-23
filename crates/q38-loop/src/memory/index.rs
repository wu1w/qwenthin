//! FTS5 over durable notes. MEMORY.md is never indexed (keep it whole; read it).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags};

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryHit {
    pub path: String,
    pub kind: String,
    pub snippet: String,
}

pub struct MemoryIndex {
    conn: Mutex<Connection>,
}

impl MemoryIndex {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let conn = Connection::open_with_flags(
            dir.join("memory.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .map_err(Error::msg)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE VIRTUAL TABLE IF NOT EXISTS notes USING fts5(
               path UNINDEXED,
               kind UNINDEXED,
               body,
               tokenize = 'unicode61'
             );",
        )
        .map_err(Error::msg)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert(&self, path: &str, kind: &str, body: &str) -> Result<()> {
        if path.rsplit('/').next() == Some("MEMORY.md") {
            return Ok(());
        }
        if body.trim().is_empty() {
            return Ok(());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("memory index poisoned"))?;
        conn.execute("DELETE FROM notes WHERE path = ?1", params![path])
            .map_err(Error::msg)?;
        conn.execute(
            "INSERT INTO notes (path, kind, body) VALUES (?1, ?2, ?3)",
            params![path, kind, body],
        )
        .map_err(Error::msg)?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryHit>> {
        let limit = limit.clamp(1, 20) as i64;
        let tokens = fts_tokens(query);
        let fts = fts_query(query);
        if !fts.is_empty() {
            match self.search_fts(&fts, limit) {
                Ok(hits) if !hits.is_empty() => return Ok(hits),
                _ => {}
            }
        }
        self.search_like(&tokens, limit)
    }

    fn search_fts(&self, fts: &str, limit: i64) -> rusqlite::Result<Vec<MemoryHit>> {
        let conn = self.conn.lock().expect("memory index poisoned");
        let mut stmt = conn.prepare(
            "SELECT path, kind, snippet(notes, 2, '[', ']', '…', 12)
             FROM notes WHERE notes MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts, limit], |row| {
            Ok(MemoryHit {
                path: row.get(0)?,
                kind: row.get(1)?,
                snippet: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    fn search_like(&self, tokens: &[String], limit: i64) -> Result<Vec<MemoryHit>> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from("SELECT path, kind, substr(body, 1, 160) FROM notes WHERE ");
        for i in 0..tokens.len() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str(&format!("body LIKE ?{} ESCAPE '\\'", i + 1));
        }
        sql.push_str(&format!(" LIMIT ?{}", tokens.len() + 1));
        let mut bind: Vec<rusqlite::types::Value> = tokens
            .iter()
            .map(|t| rusqlite::types::Value::Text(format!("%{}%", like_escape(t))))
            .collect();
        bind.push(rusqlite::types::Value::Integer(limit));
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("memory index poisoned"))?;
        let mut stmt = conn.prepare(&sql).map_err(Error::msg)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind), |row| {
                Ok(MemoryHit {
                    path: row.get(0)?,
                    kind: row.get(1)?,
                    snippet: row.get(2)?,
                })
            })
            .map_err(Error::msg)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::msg)
    }

    pub fn reindex_tree(&self, root: &Path) -> Result<()> {
        walk_index(self, root, root)
    }
}

fn walk_index(index: &MemoryIndex, root: &Path, dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_index(index, root, &path)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if path.file_name().and_then(|s| s.to_str()) == Some("MEMORY.md") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let kind = kind_for(&rel);
        index.upsert(&rel, kind, &body)?;
    }
    Ok(())
}

fn kind_for(rel: &str) -> &'static str {
    if rel.starts_with("digest/") {
        "digest"
    } else if rel.starts_with("memory/") {
        "daily"
    } else {
        "note"
    }
}

fn fts_query(raw: &str) -> String {
    fts_tokens(raw)
        .into_iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .filter(|t| t.len() > 2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn fts_tokens(raw: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut cur_ascii = true;
    for c in raw.chars() {
        let ascii = c.is_ascii_alphanumeric() || c == '_';
        let cjk = is_cjk(c);
        if !ascii && !cjk {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if !cur.is_empty() && cur_ascii != ascii {
            tokens.push(std::mem::take(&mut cur));
        }
        cur_ascii = ascii;
        cur.push(c);
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2CEAF}'
            | '\u{30000}'..='\u{3134F}'
    )
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("q38-mem-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fts_query_splits_hyphen_keeps_cjk() {
        let q = fts_query("q-harness audit 源码修改");
        assert!(!q.contains("qharness"), "{q}");
        assert!(q.contains("\"harness\""), "{q}");
        assert!(q.contains("\"audit\""), "{q}");
        assert!(q.contains("\"源码修改\""), "{q}");
        assert!(fts_query("源码").contains("源码"));
        assert!(fts_query("源码修改").contains("源码修改"));
    }

    #[test]
    fn hyphenated_and_cjk_query_hits() {
        let dir = tmp();
        let idx = MemoryIndex::open(&dir).unwrap();
        idx.upsert(
            "memory/2026-08-20/s-3.md",
            "daily",
            "compact note: q-harness audit 源码 and related work",
        )
        .unwrap();
        let hits = idx.search("q-harness audit 源码修改", 10).unwrap();
        assert!(hits.iter().any(|h| h.path.contains("s-3")), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hyphen_query_does_not_glue_tokens() {
        let dir = tmp();
        let idx = MemoryIndex::open(&dir).unwrap();
        idx.upsert("notes/repo.md", "note", "clone q-harness then probe")
            .unwrap();
        let q = fts_query("q-harness");
        assert!(!q.contains("qharness"), "{q}");
        assert!(q.contains("\"harness\""), "{q}");
        let hits = idx.search("q-harness", 10).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pure_cjk_query_hits() {
        let dir = tmp();
        let idx = MemoryIndex::open(&dir).unwrap();
        idx.upsert("memory/2026-08-20/s-1.md", "daily", "昨日完成源码修改")
            .unwrap();
        let hits = idx.search("源码", 10).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn skips_memory_md_on_upsert() {
        let dir = tmp();
        let idx = MemoryIndex::open(&dir).unwrap();
        idx.upsert("MEMORY.md", "note", "secret-zirconium-needle")
            .unwrap();
        idx.upsert("digest/wiki/MEMORY.md", "note", "secret-zirconium-needle")
            .unwrap();
        assert!(idx.search("zirconium", 10).unwrap().is_empty());
        idx.upsert("memory/2026-08-20/s-1.md", "daily", "linker rewrite")
            .unwrap();
        let hits = idx.search("linker", 10).unwrap();
        assert!(hits.iter().any(|h| h.path.contains("s-1")), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
