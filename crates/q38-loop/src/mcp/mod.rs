//! One `mcp(server, method, args)` tool. Never expand `tools/list` into OpenAI
//! `tools[]` — that is the QwenPaw zoo that wrecks 27B tool choice and the
//! Jinja tools hash.
//!
//! Config is a mount list, same overlay as skills: `config.toml` `[mcp]`,
//! `~/.q38-agent/mcp.toml` + `mcp/*.toml`, then workspace `.q38/mcp.toml` +
//! `.q38/mcp/*.toml` (later wins). Catalog stays out of the frozen system.
//! The one `mcp` blob is appended at session start only when servers exist.
//! Harness injects at most one hidden card after the live query.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools::{arg_str, folded_response, BlobStore, ToolLimits};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub servers: Vec<McpServer>,
    pub timeout_s: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            timeout_s: 30,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Inherit host env. Default false: `env_clear` plus PATH/HOME whitelist.
    pub inherit_env: bool,
    /// Working directory; empty = inherit.
    pub cwd: String,
    /// One-line trigger for catalog / hidden card. Never command or env.
    pub description: String,
    /// Optional method names. `tools/list` still works via mcp().
    pub methods: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct McpRegistry {
    pub servers: Vec<McpServer>,
    pub timeout: Duration,
    /// Transport failures this Agent turn (shared across clones). Next user
    /// turn builds a new registry, so a later cron/chat can retry.
    failed: Arc<Mutex<HashSet<String>>>,
}

impl McpRegistry {
    pub fn from_config(cfg: &McpConfig) -> Self {
        Self::load(None, Path::new(""), cfg)
    }

    pub fn load(home: Option<&Path>, workspace: &Path, base: &McpConfig) -> Self {
        let mut servers = base.servers.clone();
        let mut timeout_s = base.timeout_s.max(1);
        if let Some(home) = home {
            merge_file(&mut servers, &mut timeout_s, &home.join("mcp.toml"));
            merge_dir(&mut servers, &home.join("mcp"));
        }
        merge_file(
            &mut servers,
            &mut timeout_s,
            &workspace.join(".q38").join("mcp.toml"),
        );
        merge_dir(&mut servers, &workspace.join(".q38").join("mcp"));
        servers.retain(|s| !s.name.trim().is_empty() && !s.command.trim().is_empty());
        Self {
            servers,
            timeout: Duration::from_secs(timeout_s),
            ..Self::default()
        }
    }

    pub fn with_servers(servers: Vec<McpServer>, timeout: Duration) -> Self {
        Self {
            servers,
            timeout,
            ..Self::default()
        }
    }

    /// Name + trigger line. Empty when there are no servers. Not secrets.
    pub fn catalog_markdown(&self) -> String {
        if self.servers.is_empty() {
            return String::new();
        }
        let mut s = String::from("mcp:\n");
        for srv in &self.servers {
            s.push_str(&format!("- {}: {}\n", srv.name, trigger_line(srv)));
        }
        s
    }

    pub fn get(&self, name: &str) -> Option<&McpServer> {
        self.servers
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name.trim()))
    }

    fn server_names(&self) -> String {
        self.servers
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn is_dead(&self, name: &str) -> bool {
        let key = name.trim().to_ascii_lowercase();
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
    }

    fn mark_dead(&self, name: &str) {
        let key = name.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
    }
}

fn merge_file(servers: &mut Vec<McpServer>, timeout_s: &mut u64, path: &Path) {
    let Some(cfg) = read_bundle(path) else {
        return;
    };
    // 只有 overlay 文件显式写了 timeout_s 才覆盖，否则保留 base
    // （config.toml）的值——serde default 会把缺省当 30 抹掉用户配置。
    if let Some(t) = cfg.timeout_s {
        *timeout_s = t.max(1);
    }
    overlay(servers, cfg.servers);
}

fn merge_dir(servers: &mut Vec<McpServer>, dir: &Path) {
    overlay(servers, read_dir_servers(dir));
}

fn overlay(dst: &mut Vec<McpServer>, extra: Vec<McpServer>) {
    for s in extra {
        if let Some(i) = dst
            .iter()
            .position(|x| x.name.eq_ignore_ascii_case(&s.name))
        {
            dst[i] = s;
        } else {
            dst.push(s);
        }
    }
}

/// Console GET omits `env`. A round-trip save of the editable list must not
/// wipe tokens still in `config.toml`.
pub fn merge_mcp_servers(old: &[McpServer], incoming: Vec<McpServer>) -> Vec<McpServer> {
    incoming
        .into_iter()
        .map(|mut s| {
            if let Some(prev) = old.iter().find(|p| p.name.eq_ignore_ascii_case(&s.name)) {
                if s.env.is_empty() {
                    s.env = prev.env.clone();
                }
                if s.cwd.is_empty() && !prev.cwd.is_empty() {
                    s.cwd = prev.cwd.clone();
                }
            }
            s
        })
        .collect()
}

pub fn upsert_mcp_server(list: &mut Vec<McpServer>, mut add: McpServer) {
    if let Some(prev) = list.iter().find(|p| p.name.eq_ignore_ascii_case(&add.name)) {
        if add.env.is_empty() {
            add.env = prev.env.clone();
        }
        if add.cwd.is_empty() && !prev.cwd.is_empty() {
            add.cwd = prev.cwd.clone();
        }
    }
    if let Some(i) = list
        .iter()
        .position(|p| p.name.eq_ignore_ascii_case(&add.name))
    {
        list[i] = add;
    } else {
        list.push(add);
    }
}

pub fn remove_mcp_server(list: &mut Vec<McpServer>, name: &str) {
    list.retain(|s| !s.name.eq_ignore_ascii_case(name));
}

/// overlay 文件的原样形状：timeout_s 缺省是 None，不是 30。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct McpBundle {
    servers: Vec<McpServer>,
    timeout_s: Option<u64>,
}

fn read_bundle(path: &Path) -> Option<McpBundle> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_mcp_toml(&raw, path.file_stem().and_then(|s| s.to_str()))
}

fn read_dir_servers(dir: &Path) -> Vec<McpServer> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    files.sort();
    for path in files {
        if let Some(cfg) = read_bundle(&path) {
            out.extend(cfg.servers);
        }
    }
    out
}

fn parse_mcp_toml(raw: &str, stem: Option<&str>) -> Option<McpBundle> {
    if let Ok(cfg) = toml::from_str::<McpBundle>(raw) {
        if !cfg.servers.is_empty() {
            return Some(cfg);
        }
    }
    let mut server = toml::from_str::<McpServer>(raw).ok()?;
    if server.command.trim().is_empty() {
        return None;
    }
    if server.name.trim().is_empty() {
        server.name = stem.unwrap_or("").to_string();
    }
    Some(McpBundle {
        servers: vec![server],
        timeout_s: None,
    })
}

fn trigger_line(srv: &McpServer) -> String {
    let t = if !srv.description.trim().is_empty() {
        srv.description.trim().to_string()
    } else if !srv.methods.is_empty() {
        srv.methods.join(", ")
    } else {
        "on demand".into()
    };
    t.chars().take(40).collect()
}

pub fn hidden_card(server: &McpServer) -> Option<String> {
    let mut body = format!("[mcp: {}]", server.name);
    let trig = trigger_line(server);
    if trig != "on demand" {
        body.push('\n');
        body.push_str(&trig);
    }
    if !server.methods.is_empty() {
        body.push_str("\nmethods: ");
        body.push_str(&server.methods.join(", "));
    }
    if crate::sticky::tokens(&body) > crate::sticky::SKILL_BODY_MAX_TOKENS {
        return None;
    }
    Some(body)
}

fn catalog_card(reg: &McpRegistry) -> Option<String> {
    let md = reg.catalog_markdown();
    if md.is_empty() {
        return None;
    }
    let body = format!("[mcp]\n{}", md.trim());
    if crate::sticky::tokens(&body) > crate::sticky::SKILL_BODY_MAX_TOKENS {
        return None;
    }
    Some(body)
}

/// Hidden MCP card, or `None` when this turn should stay zero-recall.
///
/// A bare server name is not enough (`docs/foo.md` must not mount `docs`).
/// The gate is the token `mcp` (or `[mcp:name]` / a forced name). Once that
/// fires, `named_in_text` may still mount a server named in the rest of the
/// message — e.g. "use mcp to write docs/foo.md" mounts `docs`.
pub fn card_for(reg: &McpRegistry, user: &str, forced: Option<&str>) -> Option<String> {
    if reg.servers.is_empty() {
        return None;
    }
    if let Some(name) = forced {
        return reg.get(name).and_then(hidden_card);
    }
    let (forced, rest) = crate::sticky::split_mcp_prefix(user);
    if let Some(name) = forced {
        return reg.get(&name).and_then(hidden_card);
    }
    if !mentions_mcp(&rest) {
        return None;
    }
    if let Some(srv) = named_in_text(reg, &rest) {
        return hidden_card(srv);
    }
    if reg.servers.len() == 1 {
        return hidden_card(&reg.servers[0]);
    }
    catalog_card(reg)
}

fn mentions_mcp(user: &str) -> bool {
    user.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w.eq_ignore_ascii_case("mcp"))
}

fn named_in_text<'a>(reg: &'a McpRegistry, text: &str) -> Option<&'a McpServer> {
    for srv in &reg.servers {
        if has_name_token(text, &srv.name) {
            return Some(srv);
        }
    }
    None
}

fn has_name_token(hay: &str, name: &str) -> bool {
    if name.chars().any(|c| !c.is_ascii()) {
        return hay.contains(name);
    }
    hay.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .any(|w| w.eq_ignore_ascii_case(name))
}

pub async fn run_mcp(
    registry: &McpRegistry,
    call: &ToolCall,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let method = arg_str(&call.arguments, "method").unwrap_or_default();
    if method.is_empty() || method == "list" {
        if registry.servers.is_empty() {
            return ToolResponse::text(
                &call.id,
                "No MCP servers configured. Add [[mcp.servers]] in config.toml.",
                ToolState::Success,
            );
        }
        let names: Vec<&str> = registry.servers.iter().map(|s| s.name.as_str()).collect();
        return ToolResponse::text(&call.id, names.join("\n"), ToolState::Success);
    }
    let Some(server) = arg_str(&call.arguments, "server") else {
        let names = registry.server_names();
        let msg = if names.is_empty() {
            "Error: mcp needs `server`.".into()
        } else {
            format!("Error: mcp needs `server`. Configured: {names}")
        };
        return ToolResponse::text(&call.id, msg, ToolState::Error);
    };
    let Some(spec) = registry.get(&server) else {
        return ToolResponse::text(
            &call.id,
            format!("Error: unknown MCP server '{server}'."),
            ToolState::Error,
        );
    };
    if registry.is_dead(&server) {
        return ToolResponse::text(
            &call.id,
            format!(
                "Error: MCP server '{server}' already failed this turn (timeout or stdio crash). Do not retry it; use bash/curl or continue without it."
            ),
            ToolState::Error,
        );
    }
    let args = call.arguments.get("args").cloned().unwrap_or(json!({}));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    match tokio::time::timeout(
        registry.timeout,
        dispatch_mcp(spec, &method, args, &stderr_buf),
    )
    .await
    {
        Ok(Ok(value)) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            folded_response(&call.id, text, ToolState::Success, limits, blobs)
        }
        Ok(Err(e)) => {
            let msg = e.to_string();
            if is_transport_fail(&msg) {
                registry.mark_dead(&server);
                return ToolResponse::text(
                    &call.id,
                    format!(
                        "Error: {msg}. Do not retry MCP server '{server}' this turn; use bash/curl or continue without it."
                    ),
                    ToolState::Error,
                );
            }
            ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error)
        }
        Err(_) => {
            registry.mark_dead(&server);
            let tail = stderr_tail(&stderr_buf, 512);
            let hint = format!(
                " Do not retry MCP server '{server}' this turn; use bash/curl or continue without it."
            );
            let msg = if tail.is_empty() {
                format!("Error: MCP timeout.{hint}")
            } else {
                format!("Error: MCP timeout. stderr: {tail}.{hint}")
            };
            ToolResponse::text(&call.id, msg, ToolState::Error)
        }
    }
}

fn is_transport_fail(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("mcp eof")
        || m.contains("broken pipe")
        || m.contains("mcp stdin")
        || m.contains("mcp stdout")
        || m.contains("connection reset")
        || m.contains("no such file")
}

async fn dispatch_mcp(
    spec: &McpServer,
    method: &str,
    arguments: Value,
    stderr_buf: &Arc<Mutex<Vec<u8>>>,
) -> Result<Value> {
    if method == "tools/list" {
        jsonrpc_tools_list(spec, stderr_buf).await
    } else {
        jsonrpc_call(spec, method, arguments, stderr_buf).await
    }
}

async fn jsonrpc_tools_list(spec: &McpServer, stderr_buf: &Arc<Mutex<Vec<u8>>>) -> Result<Value> {
    let mut child = spawn(spec)?;
    let mut stdin = child.stdin.take().ok_or_else(|| Error::msg("mcp stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::msg("mcp stdout"))?;
    drain_stderr(&mut child, stderr_buf);
    let mut reader = BufReader::new(stdout);
    handshake(&mut stdin, &mut reader).await?;
    write_rpc(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await?;
    let reply = read_rpc(&mut reader).await?;
    let _ = child.kill().await;
    rpc_result(reply)
}

async fn jsonrpc_call(
    spec: &McpServer,
    method: &str,
    arguments: Value,
    stderr_buf: &Arc<Mutex<Vec<u8>>>,
) -> Result<Value> {
    let mut child = spawn(spec)?;
    let mut stdin = child.stdin.take().ok_or_else(|| Error::msg("mcp stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::msg("mcp stdout"))?;
    drain_stderr(&mut child, stderr_buf);
    let mut reader = BufReader::new(stdout);
    handshake(&mut stdin, &mut reader).await?;
    write_rpc(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": method, "arguments": arguments}
        }),
    )
    .await?;
    let reply = read_rpc(&mut reader).await?;
    let _ = child.kill().await;
    rpc_result(reply)
}

fn spawn(spec: &McpServer) -> Result<tokio::process::Child> {
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !spec.cwd.is_empty() {
        cmd.current_dir(PathBuf::from(&spec.cwd));
    }
    apply_mcp_env(&mut cmd, spec);
    crate::proc_spawn::hide_window_async(&mut cmd);
    cmd.spawn().map_err(Error::msg)
}

/// Same PATH/HOME keep-list as `run_code::apply_env` when `inherit_env` is false.
const MCP_ENV_WHITELIST: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "LANG",
    "SystemRoot",
    "PATHEXT",
    "COMSPEC",
];

fn apply_mcp_env(cmd: &mut Command, spec: &McpServer) {
    if spec.inherit_env {
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        return;
    }
    cmd.env_clear();
    for (k, v) in mcp_child_env(spec, std::env::vars()) {
        cmd.env(k, v);
    }
}

/// Child environment after whitelist / inherit, then `spec.env` overlays.
fn mcp_child_env(
    spec: &McpServer,
    host: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let host: BTreeMap<String, String> = host.into_iter().collect();
    let mut out = BTreeMap::new();
    if spec.inherit_env {
        out.extend(host);
    } else {
        for key in MCP_ENV_WHITELIST {
            let value = host.get(*key).or_else(|| {
                cfg!(windows)
                    .then(|| host.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
                    .flatten()
                    .map(|(_, v)| v)
            });
            if let Some(v) = value {
                out.insert((*key).to_string(), v.clone());
            }
        }
    }
    for (k, v) in &spec.env {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Detach child's stderr and drain it so a noisy server cannot fill the OS
/// pipe and deadlock stdout JSON-RPC. Keeps the last 4 KiB for diagnostics.
fn drain_stderr(child: &mut tokio::process::Child, stderr_buf: &Arc<Mutex<Vec<u8>>>) {
    let stderr = match child.stderr.take() {
        Some(s) => s,
        None => return,
    };
    let buf = Arc::clone(stderr_buf);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut tmp = [0u8; 1024];
        let mut cap: Vec<u8> = Vec::new();
        const MAX: usize = 4096;
        loop {
            match reader.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    cap.extend_from_slice(&tmp[..n]);
                    if cap.len() > MAX {
                        let drain = cap.len() - MAX;
                        cap.drain(..drain);
                    }
                    if let Ok(mut guard) = buf.lock() {
                        *guard = cap.clone();
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Trailing `max` bytes as lossy UTF-8 (suffix, not prefix).
fn stderr_tail(buf: &Arc<Mutex<Vec<u8>>>, max: usize) -> String {
    let Ok(guard) = buf.lock() else {
        return String::new();
    };
    let skip = guard.len().saturating_sub(max);
    let bytes = &guard[skip..];
    let start = bytes.iter().position(|b| *b < 0x80).unwrap_or(0);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

const MCP_MAX_SKIP: usize = 64;

async fn handshake<W, R>(stdin: &mut W, reader: &mut R) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    write_rpc(
        stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "q38", "version": "0.1.0"}
            }
        }),
    )
    .await?;
    let _init = read_rpc(reader).await?;
    write_rpc(
        stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await?;
    Ok(())
}

fn rpc_result(reply: Value) -> Result<Value> {
    if let Some(err) = reply.get("error") {
        return Err(Error::msg(err.to_string()));
    }
    Ok(reply.get("result").cloned().unwrap_or(reply))
}

async fn write_rpc<W: AsyncWriteExt + Unpin>(w: &mut W, body: Value) -> Result<()> {
    let bytes = serde_json::to_vec(&body).map_err(Error::msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
    w.write_all(header.as_bytes()).await.map_err(Error::msg)?;
    w.write_all(&bytes).await.map_err(Error::msg)?;
    w.flush().await.map_err(Error::msg)?;
    Ok(())
}

fn content_length_of(header_line: &str) -> Option<usize> {
    let (name, rest) = header_line.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("content-length") {
        return None;
    }
    rest.trim().parse().ok()
}

fn is_rpc_response(msg: &Value) -> bool {
    match msg.get("id") {
        None | Some(Value::Null) => false,
        Some(_) => true,
    }
}

/// One LSP-style frame. Header names are case-insensitive.
async fn read_rpc_frame<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<Value> {
    let mut content_len = 0usize;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await.map_err(Error::msg)?;
        if n == 0 {
            return Err(Error::msg("mcp eof"));
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(n) = content_length_of(t) {
            content_len = n;
        }
    }
    if content_len == 0 || content_len > 8_000_000 {
        return Err(Error::msg("mcp bad Content-Length"));
    }
    let mut buf = vec![0u8; content_len];
    r.read_exact(&mut buf).await.map_err(Error::msg)?;
    serde_json::from_slice(&buf).map_err(Error::msg)
}

/// Skip JSON-RPC notifications (no `id`) until a response frame.
async fn read_rpc<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<Value> {
    for _ in 0..MCP_MAX_SKIP {
        let msg = read_rpc_frame(r).await?;
        if is_rpc_response(&msg) {
            return Ok(msg);
        }
    }
    Err(Error::msg("mcp too many notifications before response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python_server(name: &str, script: &Path) -> McpServer {
        let mut args = Vec::new();
        #[cfg(windows)]
        args.push("-3".to_string());
        args.push(script.to_string_lossy().into_owned());
        McpServer {
            name: name.into(),
            command: if cfg!(windows) { "py" } else { "python3" }.into(),
            args,
            ..McpServer::default()
        }
    }

    #[tokio::test]
    async fn fake_stdio_server_tools_call() {
        let dir = std::env::temp_dir().join(format!("q38-mcp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("echo_mcp.py");
        std::fs::write(
            &py,
            r#"
import json, sys

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise EOFError
        if line in (b"\r\n", b"\n"):
            break
        k, _, v = line.decode().partition(":")
        headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    buf = b""
    while len(buf) < n:
        chunk = sys.stdin.buffer.read(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return json.loads(buf)

def write_msg(obj):
    raw = json.dumps(obj).encode()
    sys.stdout.buffer.write(f"content-length: {len(raw)}\r\n\r\n".encode() + raw)
    sys.stdout.buffer.flush()

def write_note():
    write_msg({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hi"}})

while True:
    try:
        msg = read_msg()
    except EOFError:
        break
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        write_note()
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        write_note()
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"ping","description":"pong"}]}})
    elif method == "tools/call":
        write_note()
        name = (msg.get("params") or {}).get("name")
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"pong:"+str(name)}]}})
"#,
        )
        .unwrap();
        let registry = McpRegistry {
            servers: vec![python_server("echo", &py)],
            timeout: Duration::from_secs(8),
            ..McpRegistry::default()
        };
        let list = run_mcp(
            &registry,
            &crate::tool_calls::ToolCall {
                id: "c1".into(),
                name: "mcp".into(),
                arguments: json!({"server":"echo","method":"tools/list"}),
            },
            crate::tools::ToolLimits::default(),
            None,
        )
        .await;
        assert!(
            list.joined_text().contains("ping"),
            "{}",
            list.joined_text()
        );
        let ping = run_mcp(
            &registry,
            &crate::tool_calls::ToolCall {
                id: "c2".into(),
                name: "mcp".into(),
                arguments: json!({"server":"echo","method":"ping","args":{}}),
            },
            crate::tools::ToolLimits::default(),
            None,
        )
        .await;
        assert!(
            ping.joined_text().contains("pong"),
            "{}",
            ping.joined_text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn rpc_frame(header: &str, body: Value) -> Vec<u8> {
        let raw = serde_json::to_vec(&body).unwrap();
        let mut out = format!("{header}: {}\r\n\r\n", raw.len()).into_bytes();
        out.extend(raw);
        out
    }

    #[tokio::test]
    async fn read_rpc_skips_notifications_and_lowercase_length() {
        let mut bytes = rpc_frame(
            "content-length",
            json!({"jsonrpc":"2.0","method":"notifications/message","params":{"hi":1}}),
        );
        bytes.extend(rpc_frame(
            "Content-Length",
            json!({"jsonrpc":"2.0","id":2,"result":{"ok":true}}),
        ));
        let mut reader = BufReader::new(bytes.as_slice());
        let v = read_rpc(&mut reader).await.unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn content_length_header_is_case_insensitive() {
        assert_eq!(content_length_of("Content-Length: 12"), Some(12));
        assert_eq!(content_length_of("content-length:12"), Some(12));
        assert_eq!(content_length_of("Content-Type: application/json"), None);
    }

    #[test]
    fn workspace_mcp_toml_overlays_home() {
        let root =
            std::env::temp_dir().join(format!("q38-mcp-ov-{}", uuid::Uuid::new_v4().simple()));
        let home = root.join("home");
        let ws = root.join("ws");
        std::fs::create_dir_all(home.join("mcp")).unwrap();
        std::fs::create_dir_all(ws.join(".q38").join("mcp")).unwrap();
        std::fs::write(
            home.join("mcp.toml"),
            "[[servers]]\nname=\"docs\"\ncommand=\"python3\"\nargs=[\"home.py\"]\ndescription=\"home docs\"\n",
        )
        .unwrap();
        std::fs::write(
            ws.join(".q38").join("mcp.toml"),
            "[[servers]]\nname=\"docs\"\ncommand=\"python3\"\nargs=[\"ws.py\"]\ndescription=\"ws docs\"\nmethods=[\"search\"]\n",
        )
        .unwrap();
        std::fs::write(
            ws.join(".q38").join("mcp").join("gh.toml"),
            "name=\"gh\"\ncommand=\"npx\"\nargs=[\"-y\",\"github\"]\n",
        )
        .unwrap();
        let reg = McpRegistry::load(Some(&home), &ws, &McpConfig::default());
        assert_eq!(
            reg.servers.len(),
            2,
            "{:?}",
            reg.servers
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
        );
        let docs = reg.get("docs").unwrap();
        assert_eq!(docs.description, "ws docs");
        assert_eq!(docs.args, vec!["ws.py"]);
        assert!(reg.get("gh").is_some());
        assert!(card_for(&reg, "Write docs/ARCHITECTURE.md then stop.", None).is_none());
        let hit = card_for(&reg, "Use mcp with server docs and method search.", None).unwrap();
        assert!(hit.starts_with("[mcp: docs]"), "{hit}");
        assert!(hit.contains("search"), "{hit}");
        assert!(!hit.contains("python3"), "{hit}");
        let via_token = card_for(&reg, "use mcp to write docs/foo.md", None).unwrap();
        assert!(
            via_token.starts_with("[mcp: docs]"),
            "mcp token + server name in the rest of the message mounts: {via_token}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn overlay_without_timeout_keeps_base_timeout() {
        // overlay 文件不写 timeout_s 时不得用 serde default(30) 抹掉
        // config.toml 的值；显式写了才覆盖。
        let root =
            std::env::temp_dir().join(format!("q38-mcp-to-{}", uuid::Uuid::new_v4().simple()));
        let home = root.join("home");
        let ws = root.join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            home.join("mcp.toml"),
            "[[servers]]\nname=\"docs\"\ncommand=\"python3\"\n",
        )
        .unwrap();
        let base = McpConfig {
            timeout_s: 120,
            ..McpConfig::default()
        };
        let reg = McpRegistry::load(Some(&home), &ws, &base);
        assert_eq!(reg.timeout, Duration::from_secs(120), "base must survive");

        std::fs::write(
            home.join("mcp.toml"),
            "timeout_s = 7\n[[servers]]\nname=\"docs\"\ncommand=\"python3\"\n",
        )
        .unwrap();
        let reg = McpRegistry::load(Some(&home), &ws, &base);
        assert_eq!(reg.timeout, Duration::from_secs(7), "explicit overlay wins");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn round_trip_save_keeps_env() {
        let mut prev = McpServer {
            name: "docs".into(),
            command: "python3".into(),
            cwd: "/opt/mcp".into(),
            ..McpServer::default()
        };
        prev.env.insert("API_TOKEN".into(), "s3cret".into());
        let incoming = McpServer {
            name: "docs".into(),
            command: "python3".into(),
            args: vec!["ws.py".into()],
            ..McpServer::default()
        };
        let out = merge_mcp_servers(&[prev], vec![incoming]);
        assert_eq!(
            out[0].env.get("API_TOKEN").map(String::as_str),
            Some("s3cret")
        );
        assert_eq!(out[0].cwd, "/opt/mcp");
        assert_eq!(out[0].args, vec!["ws.py"]);
    }

    #[test]
    fn upsert_does_not_drop_siblings() {
        let mut list = vec![McpServer {
            name: "docs".into(),
            command: "python3".into(),
            ..McpServer::default()
        }];
        upsert_mcp_server(
            &mut list,
            McpServer {
                name: "tavily".into(),
                command: "npx".into(),
                ..McpServer::default()
            },
        );
        assert_eq!(list.len(), 2);
        remove_mcp_server(&mut list, "tavily");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "docs");
    }

    /// A server that floods stderr (>64 KiB) while still answering JSON-RPC.
    /// Before the drain, this deadlocked into "MCP timeout."
    #[tokio::test]
    async fn stderr_flood_does_not_deadlock() {
        let dir =
            std::env::temp_dir().join(format!("q38-mcp-stderr-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("flood_mcp.py");
        std::fs::write(
            &py,
            r#"
import json, sys

for _ in range(200):
    sys.stderr.buffer.write(b"X" * 512 + b"\n")
sys.stderr.buffer.flush()

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise EOFError
        if line in (b"\r\n", b"\n"):
            break
        k, _, v = line.decode().partition(":")
        headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    buf = b""
    while len(buf) < n:
        chunk = sys.stdin.buffer.read(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return json.loads(buf)

def write_msg(obj):
    raw = json.dumps(obj).encode()
    sys.stdout.buffer.write(f"content-length: {len(raw)}\r\n\r\n".encode() + raw)
    sys.stdout.buffer.flush()

while True:
    try:
        msg = read_msg()
    except EOFError:
        break
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"flood","version":"0"}}})
    elif method == "tools/call":
        name = (msg.get("params") or {}).get("name")
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"ok:"+str(name)}]}})
"#,
        )
        .unwrap();
        let registry = McpRegistry {
            servers: vec![python_server("flood", &py)],
            timeout: Duration::from_secs(8),
            ..McpRegistry::default()
        };
        let resp = run_mcp(
            &registry,
            &crate::tool_calls::ToolCall {
                id: "c-flood".into(),
                name: "mcp".into(),
                arguments: json!({"server":"flood","method":"ping","args":{}}),
            },
            crate::tools::ToolLimits::default(),
            None,
        )
        .await;
        let text = resp.joined_text();
        assert!(text.contains("ok:ping"), "expected success, got: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_includes_stderr_tail() {
        let dir =
            std::env::temp_dir().join(format!("q38-mcp-tmo-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("hang_mcp.py");
        std::fs::write(
            &py,
            r#"
import sys, time
sys.stderr.buffer.write(b"HANG_MARKER_12345\n")
sys.stderr.buffer.flush()
time.sleep(60)
"#,
        )
        .unwrap();
        let registry = McpRegistry {
            servers: vec![python_server("hang", &py)],
            timeout: Duration::from_millis(800),
            ..McpRegistry::default()
        };
        let resp = run_mcp(
            &registry,
            &crate::tool_calls::ToolCall {
                id: "c-hang".into(),
                name: "mcp".into(),
                arguments: json!({"server":"hang","method":"ping","args":{}}),
            },
            crate::tools::ToolLimits::default(),
            None,
        )
        .await;
        let text = resp.joined_text();
        assert!(
            text.contains("MCP timeout"),
            "expected timeout error, got: {text}"
        );
        assert!(
            text.contains("HANG_MARKER_12345"),
            "expected stderr tail in error, got: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_marks_server_dead_for_the_turn() {
        let dir =
            std::env::temp_dir().join(format!("q38-mcp-dead-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("hang_mcp.py");
        std::fs::write(&py, "import time\ntime.sleep(60)\n").unwrap();
        let registry = McpRegistry {
            servers: vec![python_server("hang", &py)],
            timeout: Duration::from_millis(400),
            ..McpRegistry::default()
        };
        let call = crate::tool_calls::ToolCall {
            id: "c-hang".into(),
            name: "mcp".into(),
            arguments: json!({"server":"hang","method":"ping","args":{}}),
        };
        let first = run_mcp(&registry, &call, crate::tools::ToolLimits::default(), None).await;
        assert!(
            first.joined_text().contains("MCP timeout"),
            "got {}",
            first.joined_text()
        );
        let t0 = std::time::Instant::now();
        let second = run_mcp(&registry, &call, crate::tools::ToolLimits::default(), None).await;
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "dead-server retry must not wait out the timeout"
        );
        let text = second.joined_text();
        assert!(
            text.contains("already failed this turn"),
            "expected circuit-break, got: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_server_lists_configured_names() {
        let registry = McpRegistry {
            servers: vec![McpServer {
                name: "tavily".into(),
                command: "npx".into(),
                ..McpServer::default()
            }],
            ..McpRegistry::default()
        };
        let resp = run_mcp(
            &registry,
            &crate::tool_calls::ToolCall {
                id: "c1".into(),
                name: "mcp".into(),
                arguments: json!({"method":"search","args":{"query":"x"}}),
            },
            crate::tools::ToolLimits::default(),
            None,
        )
        .await;
        let text = resp.joined_text();
        assert!(text.contains("needs `server`"), "{text}");
        assert!(text.contains("tavily"), "{text}");
    }

    fn host_env_with_dummy_api_key() -> BTreeMap<String, String> {
        let mut host: BTreeMap<String, String> = std::env::vars().collect();
        host.insert("Q38_API_KEY".into(), "dummy-q38-api-key".into());
        host
    }

    #[test]
    fn apply_mcp_env_omits_host_q38_api_key_by_default() {
        let spec = McpServer {
            name: "t".into(),
            command: "true".into(),
            ..McpServer::default()
        };
        assert!(
            !spec.inherit_env,
            "inherit_env must default false (safe for workspace mcp.toml)"
        );
        let parsed: McpServer = toml::from_str("name=\"t\"\ncommand=\"true\"\n").unwrap();
        assert!(!parsed.inherit_env);

        let env = mcp_child_env(&spec, host_env_with_dummy_api_key());
        assert!(
            !env.contains_key("Q38_API_KEY"),
            "host API key must not leak when inherit_env is false"
        );
        if std::env::var("PATH").is_ok() {
            assert!(env.contains_key("PATH"));
        }

        let mut with_overlay = spec.clone();
        with_overlay
            .env
            .insert("SERVER_TOKEN".into(), "from-toml".into());
        let over = mcp_child_env(&with_overlay, host_env_with_dummy_api_key());
        assert_eq!(
            over.get("SERVER_TOKEN").map(String::as_str),
            Some("from-toml")
        );
        assert!(!over.contains_key("Q38_API_KEY"));

        let mut cmd = Command::new("true");
        apply_mcp_env(&mut cmd, &spec);
        let leaked = cmd
            .as_std()
            .get_envs()
            .any(|(k, v)| v.is_some() && k.to_string_lossy() == "Q38_API_KEY");
        assert!(!leaked);
    }

    #[test]
    fn apply_mcp_env_keeps_host_q38_api_key_when_inherit_env() {
        let spec = McpServer {
            name: "t".into(),
            command: "true".into(),
            inherit_env: true,
            ..McpServer::default()
        };
        let env = mcp_child_env(&spec, host_env_with_dummy_api_key());
        assert_eq!(
            env.get("Q38_API_KEY").map(String::as_str),
            Some("dummy-q38-api-key")
        );
    }

    #[test]
    fn stderr_tail_keeps_suffix_not_prefix() {
        let mut v = vec![b'x'; 100];
        v.extend_from_slice(b"MARK");
        let buf = Arc::new(Mutex::new(v));
        let s = stderr_tail(&buf, 8);
        assert!(s.ends_with("MARK"), "{s:?}");
        assert_eq!(s.as_bytes(), b"xxxxMARK");
    }
}
