//! Workspace code index. One `search` tool covers Cursor's glob / exact-symbol
//! / keyword paths and returns function-sized spans, not grep dumps. Not a
//! fifth frozen tool JSON — the agent appends `search_tool()` after the frozen
//! four.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection, Transaction};

use super::{arg_str, folded_response, ToolLimits, Workspace};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};

const HIT_CAP: usize = 8;
const CHUNK_LINES: usize = 80;
const RENDER_CHARS: usize = 4000;
const MAX_FILES: usize = 50_000;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const INDEX_SCHEMA: i64 = 2;

const SKIP_DIR: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".q38",
    "blobs",
    "AppData",
    "Application Data",
    "Local Settings",
    "Library",
    "Caches",
];

pub struct CodeIndex {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub(crate) struct Hit {
    path: String,
    start: u32,
    end: u32,
    body: String,
}

const MAX_NESTED_GIT: usize = 64;

impl CodeIndex {
    pub fn build(root: &Path) -> Self {
        // Packaged Electron used to spawn with cwd=home. Indexing a Windows
        // profile (AppData / OneDrive / Downloads) blocks the first model hop
        // for minutes while the UI says "正在调用模型".
        if is_user_home(root) {
            return Self::empty();
        }
        let git_backed = git_dir(root).is_some();
        let files = collect_index_files(root);
        // Git workspaces get a global cache. Scratch/non-repository folders
        // stay in memory, so q38 never leaves project-local index artifacts.
        let idx = if git_backed {
            Self::persistent(root).unwrap_or_else(Self::empty)
        } else {
            Self::empty()
        };
        idx.sync_root(root, files);
        idx
    }

    fn empty() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        init_schema(&conn).expect("fts5 chunks");
        Self {
            conn: Mutex::new(conn),
        }
    }

    fn persistent(root: &Path) -> Option<Self> {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let key = crate::vendor::sha256_hex(canonical.to_string_lossy().as_bytes());
        let dir = crate::config::Config::home_dir().ok()?.join("code-index");
        std::fs::create_dir_all(&dir).ok()?;
        let conn = Connection::open(dir.join(format!("{}.sqlite3", &key[..24]))).ok()?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        init_schema(&conn).ok()?;
        Some(Self {
            conn: Mutex::new(conn),
        })
    }

    fn sync_root(&self, root: &Path, files: Vec<PathBuf>) {
        let known = {
            let conn = crate::lock_unpoison(&self.conn);
            let Ok(mut stmt) = conn.prepare("SELECT path, size, mtime_ns FROM files") else {
                return;
            };
            let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            }) else {
                return;
            };
            rows.flatten()
                .map(|(p, size, mtime)| (p, (size, mtime)))
                .collect::<HashMap<_, _>>()
        };

        let mut seen = HashSet::new();
        let mut updates = Vec::new();
        for rel in files.into_iter().take(MAX_FILES) {
            if !should_index(&rel) {
                continue;
            }
            let abs = root.join(&rel);
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            if !meta.is_file() || meta.len() as usize > MAX_FILE_BYTES {
                continue;
            }
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            let stamp = file_stamp(&meta);
            seen.insert(rel_s.clone());
            if known.get(&rel_s) == Some(&stamp) {
                continue;
            }
            let content = std::fs::read_to_string(&abs)
                .ok()
                .filter(|s| !s.contains('\0') && s.len() <= MAX_FILE_BYTES);
            updates.push((rel_s, stamp, content));
        }

        let stale: Vec<String> = known
            .keys()
            .filter(|path| !seen.contains(*path))
            .cloned()
            .collect();
        let mut conn = crate::lock_unpoison(&self.conn);
        let Ok(tx) = conn.transaction() else {
            return;
        };
        for path in stale {
            drop_path_tx(&tx, &path);
        }
        for (path, stamp, content) in updates {
            if let Some(content) = content {
                upsert_file_tx(&tx, &path, &content, stamp);
            } else {
                drop_path_tx(&tx, &path);
            }
        }
        let _ = tx.commit();
    }

    pub fn refresh(&self, ws: &Workspace, raw_path: &str) {
        let shown = ws.shown(raw_path);
        let Ok(abs) = ws.resolve(raw_path) else {
            self.drop_path(&shown);
            return;
        };
        if !should_index(Path::new(&shown)) {
            self.drop_path(&shown);
            return;
        }
        match std::fs::read_to_string(&abs) {
            Ok(content) if !content.contains('\0') && content.len() <= MAX_FILE_BYTES => {
                let stamp = std::fs::metadata(&abs)
                    .map(|m| file_stamp(&m))
                    .unwrap_or((content.len() as i64, 0));
                self.upsert_file(&shown, &content, stamp);
            }
            _ => self.drop_path(&shown),
        }
    }

    fn drop_path(&self, path: &str) {
        let conn = crate::lock_unpoison(&self.conn);
        let _ = conn.execute("DELETE FROM chunks WHERE path = ?1", params![path]);
        let _ = conn.execute("DELETE FROM files WHERE path = ?1", params![path]);
    }

    fn upsert_file(&self, path: &str, content: &str, stamp: (i64, i64)) {
        let mut conn = crate::lock_unpoison(&self.conn);
        if let Ok(tx) = conn.transaction() {
            upsert_file_tx(&tx, path, content, stamp);
            let _ = tx.commit();
        };
    }

    pub(crate) fn search(&self, query: &str, path_filter: Option<&str>, limit: usize) -> Vec<Hit> {
        let cap = limit.clamp(1, HIT_CAP);
        let mut out = Vec::new();
        let mut seen = HashSet::new();

        if is_glob(query) {
            if let Ok(hits) = self.search_glob(query, path_filter, cap as i64) {
                merge_hits(&mut out, &mut seen, cap, hits);
            }
            if !out.is_empty() {
                return out;
            }
        } else if looks_like_filename(query) {
            if let Ok(hits) = self.search_filename(query, path_filter, cap as i64) {
                merge_hits(&mut out, &mut seen, cap, hits);
            }
            if !out.is_empty() {
                return out;
            }
        }

        let idents = ident_tokens(query);
        let mut exact_idents = Vec::new();
        for ident in &idents {
            if let Ok(hits) = self.search_symbol(&ident, path_filter, cap as i64) {
                if !hits.is_empty() {
                    exact_idents.push(ident.clone());
                }
                merge_hits(&mut out, &mut seen, cap, hits);
            }
        }
        // An explicit identifier is a much stronger signal than surrounding
        // prose. Add reference chunks for that identifier, then converge;
        // never fill the result with unrelated matches for words like
        // "Windows", "bug", or "where".
        if !exact_idents.is_empty() {
            for ident in &exact_idents {
                let exact = format!("\"{}\"", ident.replace('"', ""));
                if let Ok(hits) = self.search_fts(&exact, path_filter, cap as i64) {
                    merge_hits(&mut out, &mut seen, cap, hits);
                }
            }
            return out;
        }

        let fts = fts_query(query);
        if !fts.is_empty() && out.len() < cap {
            if let Ok(hits) = self.search_fts(&fts, path_filter, cap as i64) {
                merge_hits(&mut out, &mut seen, cap, hits);
            }
        }
        if out.len() < cap {
            if let Ok(hits) = self.search_like(&search_tokens(query), path_filter, cap as i64) {
                merge_hits(&mut out, &mut seen, cap, hits);
            }
        }
        out
    }

    fn search_symbol(
        &self,
        ident: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let conn = crate::lock_unpoison(&self.conn);
        if let Some(p) = path_filter {
            let like = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE symbol = ?1 AND path LIKE ?2 ESCAPE '\\'
                 ORDER BY path LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![ident, like, limit], row_to_hit)?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE symbol = ?1 ORDER BY path LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![ident, limit], row_to_hit)?;
            rows.collect()
        }
    }

    fn search_filename(
        &self,
        name: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let needle = name.trim().trim_start_matches("./").replace('\\', "/");
        let like = format!("%{}", like_escape(&needle));
        let conn = crate::lock_unpoison(&self.conn);
        if let Some(p) = path_filter {
            let pf = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                   AND path LIKE ?3 ESCAPE '\\'
                 ORDER BY path, start LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![needle, like, pf, limit], row_to_hit)?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'
                 ORDER BY path, start LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![needle, like, limit], row_to_hit)?;
            rows.collect()
        }
    }

    fn search_glob(
        &self,
        pattern: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let glob = pattern.replace("**/", "*").replace("**", "*");
        let conn = crate::lock_unpoison(&self.conn);
        let hits = if let Some(p) = path_filter {
            let like = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE path GLOB ?1 AND path LIKE ?2 ESCAPE '\\'
                 ORDER BY path, start LIMIT ?3",
            )?;
            let mapped = stmt.query_map(params![glob, like, limit], row_to_hit)?;
            let collected = mapped.collect::<rusqlite::Result<Vec<_>>>()?;
            collected
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE path GLOB ?1 ORDER BY path, start LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![glob, limit], row_to_hit)?;
            let collected = mapped.collect::<rusqlite::Result<Vec<_>>>()?;
            collected
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for h in hits {
            if seen.insert(h.path.clone()) {
                out.push(h);
            }
        }
        Ok(out)
    }

    fn search_fts(
        &self,
        fts: &str,
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
        let conn = crate::lock_unpoison(&self.conn);
        if let Some(p) = path_filter {
            let like = format!("%{}%", like_escape(p));
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE chunks MATCH ?1 AND path LIKE ?2 ESCAPE '\\'
                 ORDER BY rank LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![fts, like, limit], row_to_hit)?;
            rows.collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT path, start, end, body FROM chunks
                 WHERE chunks MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fts, limit], row_to_hit)?;
            rows.collect()
        }
    }

    fn search_like(
        &self,
        tokens: &[String],
        path_filter: Option<&str>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Hit>> {
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
        let sql = if let Some(p) = path_filter {
            bind.push(rusqlite::types::Value::Text(format!(
                "%{}%",
                like_escape(p)
            )));
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT path, start, end, body FROM chunks
                 WHERE ({like_sql}) AND path LIKE ?{} ESCAPE '\\' LIMIT ?{}",
                tokens.len() + 1,
                tokens.len() + 2
            )
        } else {
            bind.push(rusqlite::types::Value::Integer(limit));
            format!(
                "SELECT path, start, end, body FROM chunks WHERE {like_sql} LIMIT ?{}",
                tokens.len() + 1
            )
        };
        let conn = crate::lock_unpoison(&self.conn);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind), row_to_hit)?;
        rows.collect()
    }
}

impl CodeIndex {
    pub fn render_query(&self, query: &str, path_filter: Option<&str>) -> Option<String> {
        let hits = self.search(query, path_filter, HIT_CAP);
        if hits.is_empty() {
            None
        } else {
            Some(render_hits(&hits, RENDER_CHARS))
        }
    }
}

pub fn run_search(index: &CodeIndex, call: &ToolCall, limits: ToolLimits) -> ToolResponse {
    let query = arg_str(&call.arguments, "query").unwrap_or_default();
    if query.trim().is_empty() {
        return ToolResponse::text(&call.id, "Error: search needs `query`.", ToolState::Error);
    }
    let path = arg_str(&call.arguments, "path").filter(|s| !s.trim().is_empty());
    let hits = index.search(&query, path.as_deref(), HIT_CAP);
    if hits.is_empty() {
        return ToolResponse::text(&call.id, "No matches.", ToolState::Success);
    }
    folded_response(
        &call.id,
        render_hits(&hits, RENDER_CHARS),
        ToolState::Success,
        limits,
        None,
    )
}

fn merge_hits(out: &mut Vec<Hit>, seen: &mut HashSet<(String, u32)>, cap: usize, hits: Vec<Hit>) {
    for h in hits {
        if out.len() >= cap {
            break;
        }
        if seen.insert((h.path.clone(), h.start)) {
            out.push(h);
        }
    }
}

fn row_to_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    let start: i64 = row.get(1)?;
    let end: i64 = row.get(2)?;
    Ok(Hit {
        path: row.get(0)?,
        start: start.max(1) as u32,
        end: end.max(start).max(1) as u32,
        body: row.get(3)?,
    })
}

fn render_hits(hits: &[Hit], cap: usize) -> String {
    // This tiny result-local hint replaces a standing prompt lecture. Exact,
    // bounded spans should normally be consumed before another grep round.
    let mut out = String::from("[index] bounded spans; grep only if evidence is missing.\n");
    let mut wrote_hit = false;
    for h in hits {
        let block = format_hit(h);
        if wrote_hit && out.chars().count() + block.chars().count() > cap {
            break;
        }
        out.push('\n');
        out.push_str(&block);
        wrote_hit = true;
    }
    out
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 0 && version != INDEX_SCHEMA {
        conn.execute_batch("DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS chunks;")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files(
           path TEXT PRIMARY KEY,
           size INTEGER NOT NULL,
           mtime_ns INTEGER NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
           path,
           start UNINDEXED,
           end UNINDEXED,
           symbol,
           body,
           tokenize = \"unicode61 tokenchars '_'\"
         );",
    )?;
    conn.pragma_update(None, "user_version", INDEX_SCHEMA)?;
    Ok(())
}

fn file_stamp(meta: &std::fs::Metadata) -> (i64, i64) {
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    (meta.len().min(i64::MAX as u64) as i64, modified)
}

fn drop_path_tx(tx: &Transaction<'_>, path: &str) {
    let _ = tx.execute("DELETE FROM chunks WHERE path = ?1", params![path]);
    let _ = tx.execute("DELETE FROM files WHERE path = ?1", params![path]);
}

fn upsert_file_tx(tx: &Transaction<'_>, path: &str, content: &str, stamp: (i64, i64)) {
    let _ = tx.execute("DELETE FROM chunks WHERE path = ?1", params![path]);
    for ch in chunk_file(content) {
        let _ = tx.execute(
            "INSERT INTO chunks (path, start, end, symbol, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![path, ch.start as i64, ch.end as i64, ch.symbol, ch.body],
        );
    }
    let _ = tx.execute(
        "INSERT INTO files(path, size, mtime_ns) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime_ns=excluded.mtime_ns",
        params![path, stamp.0, stamp.1],
    );
}

fn format_hit(h: &Hit) -> String {
    let mut s = format!("## {}:{}-{}\n", h.path, h.start, h.end);
    for (i, line) in h.body.lines().enumerate() {
        s.push_str(&format!("{:>6}|{}\n", h.start as usize + i, line));
    }
    s
}

#[derive(Debug)]
struct Chunk {
    start: u32,
    end: u32,
    symbol: String,
    body: String,
}

fn chunk_file(content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.is_empty() || (lines.len() == 1 && lines[0].is_empty()) {
        return Vec::new();
    }
    let n = lines.len();
    let mut bounds = vec![0usize];
    for (i, line) in lines.iter().enumerate() {
        if i > 0 && is_def_line(line) {
            bounds.push(i);
        }
    }
    bounds.push(n);
    let mut out = Vec::new();
    for w in bounds.windows(2) {
        let mut a = w[0];
        let b = w[1];
        while a < b {
            let end = (a + CHUNK_LINES).min(b);
            let slice = &lines[a..end];
            let body = slice.join("\n");
            if !body.trim().is_empty() {
                out.push(Chunk {
                    start: (a + 1) as u32,
                    end: end as u32,
                    symbol: slice
                        .iter()
                        .find(|l| is_def_line(l))
                        .map(|l| symbol_of(l))
                        .unwrap_or_default(),
                    body,
                });
            }
            a = end;
        }
    }
    out
}

fn is_def_line(line: &str) -> bool {
    let mut t = line.trim_start();
    if t.starts_with("//") || t.starts_with("/*") {
        return false;
    }
    if t.starts_with('#') && !t.starts_with("#define") && !t.starts_with("#!") {
        return false;
    }
    loop {
        if let Some(rest) = t.strip_prefix("pub(crate) ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("pub ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("export ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("async ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("unsafe ") {
            t = rest;
        } else {
            break;
        }
    }
    [
        "fn ",
        "fn(",
        "struct ",
        "enum ",
        "impl ",
        "impl<",
        "trait ",
        "mod ",
        "type ",
        "class ",
        "def ",
        "function ",
        "interface ",
        "const ",
        "static ",
        "macro_rules!",
    ]
    .iter()
    .any(|k| t.starts_with(k))
}

fn symbol_of(line: &str) -> String {
    let mut t = line.trim_start();
    loop {
        if let Some(rest) = t.strip_prefix("pub(crate) ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("pub ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("export ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("async ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix("unsafe ") {
            t = rest;
        } else {
            break;
        }
    }
    for key in [
        "fn",
        "struct",
        "enum",
        "impl",
        "trait",
        "mod",
        "type",
        "class",
        "def",
        "function",
        "interface",
        "const",
        "static",
        "macro_rules!",
    ] {
        if let Some(rest) = t.strip_prefix(key) {
            t = rest.trim_start().trim_start_matches(['<', '(']);
            break;
        }
    }
    t.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn should_index(rel: &Path) -> bool {
    if skip_rel(rel) {
        return false;
    }
    let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".lock") || name.ends_with(".min.js") {
        return false;
    }
    if matches!(
        name,
        "package-lock.json" | "Cargo.lock" | "yarn.lock" | "pnpm-lock.yaml"
    ) {
        return false;
    }
    let ext = rel
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "rs" | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "go"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "java"
            | "kt"
            | "toml"
            | "md"
            | "json"
            | "yaml"
            | "yml"
            | "sh"
            | "bash"
            | "sql"
            | "html"
            | "css"
            | "txt"
    ) || matches!(name, "Makefile" | "Dockerfile" | "CMakeLists.txt")
}

fn skip_rel(rel: &Path) -> bool {
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.starts_with("eval/nightly/work/") {
        return true;
    }
    s.split('/').any(|c| SKIP_DIR.contains(&c))
}

fn is_user_home(root: &Path) -> bool {
    let Some(home) = crate::config::user_home() else {
        return false;
    };
    let a = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let b = std::fs::canonicalize(&home).unwrap_or(home);
    a == b
}

fn git_dir(root: &Path) -> Option<PathBuf> {
    let marker = root.join(".git");
    marker.exists().then_some(marker)
}

fn collect_index_files(root: &Path) -> Vec<PathBuf> {
    if git_dir(root).is_none() {
        return walk_fallback(root);
    }
    let mut files = match git_ls_files(root) {
        Some(files) => files,
        None => walk_fallback(root),
    };
    merge_nested_git(root, &mut files);
    if files.len() > MAX_FILES {
        files.truncate(MAX_FILES);
    }
    files
}

fn merge_nested_git(root: &Path, files: &mut Vec<PathBuf>) {
    let mut seen: HashSet<PathBuf> = files.iter().cloned().collect();
    let mut nested = Vec::new();
    find_nested_git(root, root, &mut nested);
    for rel in nested {
        if files.len() >= MAX_FILES {
            return;
        }
        let abs = root.join(&rel);
        let listed = git_ls_files(&abs).unwrap_or_else(|| walk_fallback(&abs));
        for f in listed {
            let p = rel.join(f);
            if seen.insert(p.clone()) {
                files.push(p);
                if files.len() >= MAX_FILES {
                    return;
                }
            }
        }
    }
}

fn find_nested_git(workspace: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_NESTED_GIT {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_NESTED_GIT {
            return;
        }
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if SKIP_DIR.contains(&name_s.as_ref()) {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if git_dir(&path).is_some() {
            if let Ok(rel) = path.strip_prefix(workspace) {
                if !rel.as_os_str().is_empty() {
                    out.push(rel.to_path_buf());
                }
            }
        }
        find_nested_git(workspace, &path, out);
    }
}

fn git_ls_files(root: &Path) -> Option<Vec<PathBuf>> {
    let git_dir = git_dir(root)?;
    let mut cmd = Command::new("git");
    crate::proc_spawn::hide_window(&mut cmd);
    let out = cmd
        .arg("--git-dir")
        .arg(&git_dir)
        .arg("--work-tree")
        .arg(root)
        .args(["ls-files", "-z", "-c", "-o", "--exclude-standard"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "")
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).as_ref()))
            .collect(),
    )
}

fn walk_fallback(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out);
    out
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            if SKIP_DIR.contains(&name_s.as_ref()) {
                continue;
            }
            walk_dir(root, &path, out);
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

fn fts_query(raw: &str) -> String {
    let tokens = search_tokens(raw);
    if tokens.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .filter(|t| t.len() > 2)
        .collect();
    if quoted.is_empty() {
        return String::new();
    }
    // Identifiers: AND (precise, Instant Grep-like). NL leftover: OR.
    let sep = if tokens.iter().any(|t| is_ident(t)) {
        " "
    } else {
        " OR "
    };
    quoted.join(sep)
}

fn search_tokens(raw: &str) -> Vec<String> {
    let all: Vec<String> = fts_tokens(raw)
        .into_iter()
        .filter(|t| t.chars().count() > 1)
        .collect();
    let idents: Vec<String> = all.iter().filter(|t| is_ident(t)).cloned().collect();
    if !idents.is_empty() {
        return idents;
    }
    all.into_iter().filter(|t| !is_stopword(t)).collect()
}

fn ident_tokens(raw: &str) -> Vec<String> {
    fts_tokens(raw)
        .into_iter()
        .filter(|t| is_ident(t))
        .collect()
}

fn is_ident(t: &str) -> bool {
    t.contains('_')
        || t.contains('/')
        || (t.chars().any(|c| c.is_ascii_uppercase())
            && t.chars().any(|c| c.is_ascii_lowercase())
            && t.len() >= 3)
}

fn is_stopword(t: &str) -> bool {
    matches!(
        t.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "the"
            | "is"
            | "are"
            | "was"
            | "be"
            | "do"
            | "does"
            | "how"
            | "where"
            | "what"
            | "which"
            | "who"
            | "we"
            | "to"
            | "of"
            | "in"
            | "on"
            | "for"
            | "and"
            | "or"
            | "find"
            | "search"
            | "code"
            | "file"
            | "function"
            | "please"
            | "show"
            | "me"
    ) || matches!(t, "在哪" | "哪里" | "怎么" | "如何" | "什么" | "查找")
}

fn is_glob(q: &str) -> bool {
    q.contains('*') || q.contains('?')
}

fn looks_like_filename(q: &str) -> bool {
    let t = q.trim();
    if t.is_empty() || t.contains(' ') {
        return false;
    }
    t.contains('/') || (t.contains('.') && !t.starts_with('.'))
}

/// Pattern the model asked grep/rg to find. None if this is not a repo search.
pub fn bash_search_query(command: &str) -> Option<String> {
    let tokens = shell_words(command);
    if tokens.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('-') {
        i += 1;
    }
    if i >= tokens.len() {
        return None;
    }
    let cmd = tokens[i].as_str();
    let rest_i = if cmd == "git" && tokens.get(i + 1).map(|s| s.as_str()) == Some("grep") {
        i + 2
    } else if matches!(cmd, "rg" | "ripgrep" | "ag" | "ack") {
        i + 1
    } else if cmd == "grep" {
        let recursive = tokens.iter().skip(i + 1).any(|t| {
            matches!(t.as_str(), "-r" | "-R" | "-n" | "-H" | "--recursive")
                || t.starts_with("-n")
                || t.starts_with("-r")
                || looks_like_filename(t)
        });
        if !recursive {
            return None;
        }
        i + 1
    } else {
        return None;
    };
    grep_pattern(&tokens[rest_i..])
}

pub fn search_dump_too_big(text: &str) -> bool {
    text.chars().count() > 1500 || text.lines().count() > 20
}

fn grep_pattern(tokens: &[String]) -> Option<String> {
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if t == "--" {
            return tokens.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
        if matches!(t, "-e" | "-F" | "-E" | "--regexp" | "--fixed-strings") {
            return tokens.get(i + 1).cloned().filter(|s| !s.is_empty());
        }
        if let Some(rest) = t.strip_prefix("-e") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        if !t.is_empty() {
            return Some(t.to_string());
        }
        i += 1;
    }
    None
}

fn shell_words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() || matches!(c, '|' | ';' | '&' | '<' | '>') => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                if !c.is_whitespace() {
                    break;
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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
        '\u{4e00}'..='\u{9fff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{3000}'..='\u{303f}'
            | '\u{3040}'..='\u{30ff}'
            | '\u{ff00}'..='\u{ffef}'
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
    use serde_json::json;

    fn scratch() -> (std::path::PathBuf, Workspace) {
        let dir = std::env::temp_dir().join(format!("q38-idx-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/policy.rs"),
            "pub struct ThinkPolicy {\n    pub max_think_tokens: u32,\n}\n\n\
             fn ignored() {}\n\n\
             fn upgrade_medium(&mut self) {\n    if self.user_locked { return; }\n    self.policy.max_think_tokens = 2048;\n}\n",
        )
        .unwrap();
        let ws = Workspace::open(&dir, true).unwrap();
        (dir, ws)
    }

    #[test]
    fn chunks_split_on_fn() {
        let src = "fn a() {}\n\nfn b() {}\n";
        let ch = chunk_file(src);
        assert!(ch.len() >= 2, "{ch:?}");
        assert_eq!(ch[0].symbol, "a");
        assert_eq!(ch[1].symbol, "b");
    }

    #[test]
    fn search_returns_span_not_whole_file() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium", None, 8);
        assert!(!hits.is_empty(), "expected a hit");
        let h = &hits[0];
        assert!(h.path.contains("policy.rs"), "{}", h.path);
        assert!(h.body.contains("fn upgrade_medium"));
        assert!(
            !h.body.contains("struct ThinkPolicy"),
            "preamble leaked: {}",
            h.body
        );
        let call = ToolCall {
            id: "t".into(),
            name: "search".into(),
            arguments: json!({"query": "upgrade_medium"}),
        };
        let out = run_search(&idx, &call, ToolLimits::default());
        let text = out.joined_text();
        assert!(text.contains("## "), "{text}");
        assert!(text.contains("|"), "{text}");
        assert!(text.contains("upgrade_medium"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refresh_picks_up_new_fn() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        assert!(idx.search("note_thrash", None, 8).is_empty());
        std::fs::write(
            dir.join("src/policy.rs"),
            std::fs::read_to_string(dir.join("src/policy.rs")).unwrap()
                + "\nfn note_thrash(&mut self) { self.upgrade_medium(); }\n",
        )
        .unwrap();
        idx.refresh(&ws, "src/policy.rs");
        let hits = idx.search("note_thrash", None, 8);
        assert!(hits.iter().any(|h| h.body.contains("note_thrash")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn file_backed_sync_reuses_unchanged_chunks() {
        let (dir, ws) = scratch();
        let db = dir.join("index.sqlite3");
        let conn = Connection::open(&db).unwrap();
        init_schema(&conn).unwrap();
        let idx = CodeIndex {
            conn: Mutex::new(conn),
        };
        idx.sync_root(ws.root(), walk_fallback(ws.root()));
        let before: Vec<i64> = {
            let conn = crate::lock_unpoison(&idx.conn);
            let mut stmt = conn
                .prepare("SELECT rowid FROM chunks ORDER BY rowid")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        idx.sync_root(ws.root(), walk_fallback(ws.root()));
        let after: Vec<i64> = {
            let conn = crate::lock_unpoison(&idx.conn);
            let mut stmt = conn
                .prepare("SELECT rowid FROM chunks ORDER BY rowid")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(before, after, "unchanged files should not be re-indexed");
        drop(idx);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn path_filter_narrows() {
        let (dir, ws) = scratch();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("other/x.rs"), "fn upgrade_medium() {}\n").unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium", Some("src/"), 8);
        assert!(hits.iter().all(|h| h.path.starts_with("src/")), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nl_query_strips_stopwords() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("where is the think cap", None, 8);
        assert!(
            hits.iter()
                .any(|h| h.body.contains("max_think_tokens") || h.body.contains("ThinkPolicy")),
            "{hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn filename_query_finds_path() {
        let (dir, ws) = scratch();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("policy.rs", None, 8);
        assert!(
            hits.iter().any(|h| h.path.ends_with("policy.rs")),
            "{hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn glob_lists_matching_files() {
        let (dir, ws) = scratch();
        std::fs::write(dir.join("src/other.py"), "def unused():\n    pass\n").unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("src/*.rs", None, 8);
        assert!(hits.iter().any(|h| h.path == "src/policy.rs"), "{hits:?}");
        assert!(hits.iter().all(|h| h.path.ends_with(".rs")), "{hits:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn symbol_hit_ranks_before_body_mentions() {
        let (dir, ws) = scratch();
        std::fs::write(
            dir.join("src/call.rs"),
            "fn other() {\n    upgrade_medium();\n}\n",
        )
        .unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium", None, 8);
        assert!(!hits.is_empty());
        assert!(
            hits[0].body.contains("fn upgrade_medium"),
            "definition should lead: {}",
            hits[0].body
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exact_identifier_does_not_fill_with_background_words() {
        let (dir, ws) = scratch();
        std::fs::write(
            dir.join("README.md"),
            "Windows PATH setup and general troubleshooting notes.\n",
        )
        .unwrap();
        let idx = CodeIndex::build(ws.root());
        let hits = idx.search("upgrade_medium on Windows PATH", None, 8);
        assert!(!hits.is_empty());
        assert!(
            hits.iter().all(|h| h.body.contains("upgrade_medium")),
            "background prose leaked into exact-symbol results: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bash_search_query_extracts_rg_and_grep() {
        assert_eq!(
            bash_search_query("rg upgrade_medium").as_deref(),
            Some("upgrade_medium")
        );
        assert_eq!(
            bash_search_query("grep -n drive loop.rs").as_deref(),
            Some("drive")
        );
        assert_eq!(
            bash_search_query("git grep -n 'ThinkPolicy'").as_deref(),
            Some("ThinkPolicy")
        );
        assert!(bash_search_query("ps aux | grep foo").is_none());
        assert!(bash_search_query("python3 -m unittest").is_none());
        assert!(search_dump_too_big(&"x\n".repeat(25)));
        assert!(!search_dump_too_big("ok\n"));
    }

    #[test]
    fn skip_rel_drops_windows_profile_junk() {
        assert!(skip_rel(Path::new("AppData/Local/foo.rs")));
        assert!(skip_rel(Path::new("Library/Caches/bar.py")));
        assert!(!skip_rel(Path::new("src/foo.rs")));
    }

    fn git_ok() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git_init(dir: &Path) {
        let st = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .expect("git init");
        assert!(st.success(), "git init {dir:?}");
    }

    #[test]
    fn parent_git_does_not_steal_workspace_paths() {
        if !git_ok() {
            return;
        }
        let outer =
            std::env::temp_dir().join(format!("q38-git-parent-{}", uuid::Uuid::new_v4().simple()));
        let ws = outer.join("proj");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        git_init(&outer);
        std::fs::write(outer.join("unrelated.rs"), "fn outsider() {}\n").unwrap();
        std::fs::write(ws.join("src/inside.rs"), "fn nested_hit() {}\n").unwrap();
        let idx = CodeIndex::build(&ws);
        let hits = idx.search("nested_hit", None, 8);
        assert!(
            hits.iter()
                .any(|h| h.path.ends_with("inside.rs") && h.body.contains("nested_hit")),
            "{hits:?}"
        );
        assert!(
            hits.iter().all(|h| !h.path.contains("unrelated")),
            "parent git leaked: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(outer);
    }

    #[test]
    fn nested_git_repo_is_indexed() {
        if !git_ok() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("q38-git-nested-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("vendor/dep/src")).unwrap();
        git_init(&dir);
        git_init(&dir.join("vendor/dep"));
        std::fs::write(dir.join("src/root.rs"), "fn root_fn() {}\n").unwrap();
        std::fs::write(dir.join("vendor/dep/src/lib.rs"), "fn nested_dep() {}\n").unwrap();
        let idx = CodeIndex::build(&dir);
        let root_hits = idx.search("root_fn", None, 8);
        let nested_hits = idx.search("nested_dep", None, 8);
        assert!(
            root_hits.iter().any(|h| h.body.contains("root_fn")),
            "{root_hits:?}"
        );
        assert!(
            nested_hits
                .iter()
                .any(|h| h.path.replace('\\', "/").contains("vendor/dep")
                    && h.body.contains("nested_dep")),
            "{nested_hits:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
