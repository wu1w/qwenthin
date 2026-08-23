//! Write-through FTS5 over session JSONL. Source of truth remains the JSONL.
//!
//! Thinking/`reasoning` is not indexed (27B think is noisy and was never
//! evidence). Recall-tool names are reserved so a later search cannot match
//! its own previous queries.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags};

use crate::error::{Error, Result};
use crate::session::event::SessionEvent;

const SKIP_TOOL_NAMES: &[&str] = &[
    "recall",
    "recall_history",
    "memory_search",
    "search",
    "skill",
    "mcp",
    "view",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub session_id: String,
    pub seq: i64,
    pub kind: String,
    pub name: Option<String>,
    pub blob: Option<String>,
    pub snippet: String,
}

pub struct HistoryIndex {
    conn: Mutex<Connection>,
}

impl HistoryIndex {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("history.sqlite");
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .map_err(Error::msg)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE VIRTUAL TABLE IF NOT EXISTS turns USING fts5(
               session_id UNINDEXED,
               seq UNINDEXED,
               kind UNINDEXED,
               name UNINDEXED,
               blob UNINDEXED,
               body,
               tokenize = 'unicode61'
             );",
        )
        .map_err(Error::msg)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert(&self, session_id: &str, seq: i64, event: &SessionEvent) -> Result<()> {
        let Some((kind, name, blob, body)) = index_body(event) else {
            return Ok(());
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("history index poisoned"))?;
        conn.execute(
            "DELETE FROM turns WHERE session_id = ?1 AND seq = ?2",
            params![session_id, seq],
        )
        .map_err(Error::msg)?;
        conn.execute(
            "INSERT INTO turns (session_id, seq, kind, name, blob, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, seq, kind, name, blob, body],
        )
        .map_err(Error::msg)?;
        Ok(())
    }

    pub fn reindex_session(&self, session_id: &str, events: &[SessionEvent]) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("history index poisoned"))?;
        conn.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(Error::msg)?;
        drop(conn);
        for (seq, event) in events.iter().enumerate() {
            self.upsert(session_id, seq as i64, event)?;
        }
        Ok(())
    }

    pub fn search(&self, query: &str, session_id: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
        let limit = limit.clamp(1, 50) as i64;
        let tokens = fts_tokens(query);
        let fts = fts_query(query);
        if !fts.is_empty() {
            match self.search_fts(&fts, session_id, limit) {
                Ok(hits) if !hits.is_empty() => return Ok(hits),
                _ => {}
            }
        }
        self.search_like(&tokens, session_id, limit)
    }

    fn search_fts(
        &self,
        fts: &str,
        session_id: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let conn = self.conn.lock().expect("history index poisoned");
        let sql = if session_id.is_some() {
            "SELECT session_id, seq, kind, name, blob,
                    snippet(turns, 5, '[', ']', '…', 12)
             FROM turns WHERE turns MATCH ?1 AND session_id = ?2
             ORDER BY rank LIMIT ?3"
        } else {
            "SELECT session_id, seq, kind, name, blob,
                    snippet(turns, 5, '[', ']', '…', 12)
             FROM turns WHERE turns MATCH ?1
             ORDER BY rank LIMIT ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(sid) = session_id {
            stmt.query_map(params![fts, sid, limit], row_to_hit)?
        } else {
            stmt.query_map(params![fts, limit], row_to_hit)?
        };
        rows.collect()
    }

    fn search_like(
        &self,
        tokens: &[String],
        session_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Hit>> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let mut like_sql = String::new();
        for i in 0..tokens.len() {
            if i > 0 {
                like_sql.push_str(" OR ");
            }
            like_sql.push_str(&format!("body LIKE ?{} ESCAPE '\\'", i + 1));
        }
        let mut bind: Vec<rusqlite::types::Value> = tokens
            .iter()
            .map(|t| rusqlite::types::Value::Text(format!("%{}%", like_escape(t))))
            .collect();
        let sql = if let Some(sid) = session_id {
            bind.push(rusqlite::types::Value::Text(sid.to_string()));
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT session_id, seq, kind, name, blob, substr(body, 1, 160)
             FROM turns WHERE ({like_sql}) AND session_id = ?{}
             LIMIT ?{}",
                tokens.len() + 1,
                tokens.len() + 2
            )
        } else {
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT session_id, seq, kind, name, blob, substr(body, 1, 160)
             FROM turns WHERE {like_sql} LIMIT ?{}",
                tokens.len() + 1
            )
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::msg("history index poisoned"))?;
        let mut stmt = conn.prepare(&sql).map_err(Error::msg)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind), row_to_hit)
            .map_err(Error::msg)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::msg)
    }
}

fn row_to_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    Ok(Hit {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        kind: row.get(2)?,
        name: row.get(3)?,
        blob: row.get(4)?,
        snippet: row.get(5)?,
    })
}

fn index_body(
    event: &SessionEvent,
) -> Option<(&'static str, Option<String>, Option<String>, String)> {
    match event {
        SessionEvent::User(u) if !u.text.trim().is_empty() => {
            Some(("user", None, None, u.text.clone()))
        }
        SessionEvent::Assistant(a) => {
            let mut body = a.content.clone();
            if let Some(calls) = &a.tool_calls {
                for c in calls {
                    body.push('\n');
                    body.push_str(&c.function.name);
                    body.push(' ');
                    body.push_str(&c.function.arguments);
                }
            }
            if body.trim().is_empty() {
                return None;
            }
            Some(("assistant", None, None, body))
        }
        SessionEvent::Tool(t) => {
            if SKIP_TOOL_NAMES.contains(&t.name.as_str()) {
                return None;
            }
            let mut body = t.name.clone();
            body.push('\n');
            body.push_str(&t.output);
            Some(("tool", Some(t.name.clone()), t.blob.clone(), body))
        }
        _ => None,
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
    use crate::session::event::SessionEvent;

    fn tmp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("q38-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn indexes_user_and_tool_not_reasoning() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::user("fix the prefix cache miss"))
            .unwrap();
        idx.upsert(
            "s",
            2,
            &SessionEvent::assistant("ok", "I will secretly mention zirconium", None),
        )
        .unwrap();
        idx.upsert(
            "s",
            3,
            &SessionEvent::tool("c1", "bash", "cargo test passed"),
        )
        .unwrap();

        let hits = idx.search("prefix cache", Some("s"), 10).unwrap();
        assert!(hits.iter().any(|h| h.kind == "user"), "{hits:?}");

        let secret = idx.search("zirconium", Some("s"), 10).unwrap();
        assert!(
            secret.is_empty(),
            "reasoning must not be searchable: {secret:?}"
        );

        let cargo = idx.search("cargo", Some("s"), 10).unwrap();
        assert!(cargo.iter().any(|h| h.kind == "tool"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn skips_recall_tool_rows() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert(
            "s",
            1,
            &SessionEvent::tool("c9", "recall", "unique-needle-xyz"),
        )
        .unwrap();
        assert!(idx
            .search("unique-needle-xyz", Some("s"), 10)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn skips_mcp_and_skill_rows() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::tool("c9", "mcp", "secret-token-xyz"))
            .unwrap();
        idx.upsert(
            "s",
            2,
            &SessionEvent::tool("c8", "skill", "unique-skill-body-xyz"),
        )
        .unwrap();
        assert!(idx
            .search("secret-token-xyz", Some("s"), 10)
            .unwrap()
            .is_empty());
        assert!(idx
            .search("unique-skill-body-xyz", Some("s"), 10)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(dir);
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
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert(
            "s",
            1,
            &SessionEvent::user("compact note: q-harness audit 源码 and related work"),
        )
        .unwrap();
        let hits = idx
            .search("q-harness audit 源码修改", Some("s"), 10)
            .unwrap();
        assert!(hits.iter().any(|h| h.kind == "user"), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hyphen_query_does_not_glue_tokens() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::user("clone q-harness then probe"))
            .unwrap();
        let q = fts_query("q-harness");
        assert!(!q.contains("qharness"), "{q}");
        assert!(q.contains("\"harness\""), "{q}");
        let hits = idx.search("q-harness", Some("s"), 10).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pure_cjk_query_hits() {
        let dir = tmp();
        let idx = HistoryIndex::open(&dir).unwrap();
        idx.upsert("s", 1, &SessionEvent::user("昨日完成源码修改"))
            .unwrap();
        let hits = idx.search("源码", Some("s"), 10).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
