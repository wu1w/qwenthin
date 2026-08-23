use std::path::PathBuf;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use q38_loop::channel::{
    catalog_json, endpoint_kind, fetch_qrcode, merge_channel_endpoints, poll_qrcode,
    remove_channel_endpoint, upsert_channel_endpoint, BusyPolicy, ChannelEndpoint,
};
use q38_loop::config::{parse_max_tokens, parse_working_window, Config};
use q38_loop::mcp::{merge_mcp_servers, remove_mcp_server, upsert_mcp_server, McpRegistry};
use q38_loop::permit::{ApprovalMode, PermitDecision};
use q38_loop::probe::ping_models;
use q38_loop::skills::SkillCatalog;
use q38_loop::slash::UsageRecap;
use q38_loop::tools_schema::{agent_tools, mcp_tool, skill_tool, view_tool, web_tool};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

use crate::cron::{
    drop_workspace_job, heartbeat_prompt, later_ts, overlay_cron_jobs, remove_cron_job,
    upsert_cron_job, CronJob, CronStore, Heartbeat,
};
use crate::files::{
    file_content_type, file_disposition, list_child_dirs, list_tree, max_upload, parent_dir,
    pick_folder_native, read_preview, resolve_workspace_dir, workspace_shortcuts, write_upload,
};
use crate::hub::{push_state, redact_key, AppState, Inner};

const FALLBACK_HTML: &str = include_str!("../../../web/console/dist/index.html");

pub fn router(state: AppState, dist: PathBuf) -> Router {
    let api = Router::new()
        .route("/rpc", post(rpc))
        .route("/events", get(ws_upgrade))
        .route("/state", get(state_get))
        .route("/history", get(history_get))
        .route("/usage", get(usage_get))
        .route("/upload", post(upload))
        .route("/files", get(files_get))
        .route("/tree", get(tree_get))
        .route("/workspace", get(workspace_get).post(workspace_post))
        .route("/workspace/pick", post(workspace_pick))
        .route("/workspace/ls", get(workspace_ls))
        .route("/config", get(config_get).post(config_post))
        .route("/permit", post(permit_post))
        .route("/skills", get(skills_get).post(skills_post))
        .route("/mcp", get(mcp_get).post(mcp_post))
        .route("/mcp/test", post(mcp_test))
        .route("/channels", get(channels_get).post(channels_post))
        .route("/channels/{kind}/qrcode", get(channel_qrcode))
        .route("/channels/{kind}/qrcode/status", get(channel_qrcode_status))
        .route("/tools", get(tools_get))
        .route("/jobs", get(jobs_get).post(jobs_post))
        .route("/heartbeat", get(heartbeat_get).post(heartbeat_post))
        .route("/model", get(model_ping))
        .layer(DefaultBodyLimit::max(12 * 1024 * 1024));

    let cors = CorsLayer::new()
        // The console is same-origin in production. Cross-origin access is
        // only needed by a localhost dev server; arbitrary websites do not
        // receive readable API responses.
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            is_loopback_origin(origin)
        }))
        .allow_methods(Any)
        .allow_headers(Any);

    let static_files = if dist.join("index.html").is_file() {
        Router::new().fallback_service(
            ServeDir::new(&dist).not_found_service(ServeFile::new(dist.join("index.html"))),
        )
    } else {
        Router::new().fallback(get(fallback_index))
    };

    Router::new()
        .nest("/api", api)
        .merge(static_files)
        .layer(cors)
        .with_state(state)
}

async fn fallback_index() -> Html<&'static str> {
    Html(FALLBACK_HTML)
}

/// Reload the on-disk file into RAM. `q38 web` does not apply `Q38_*` — those
/// are for CLI/tests and used to get written back, wiping the saved LLM endpoint.
fn sync_cfg_from_disk(g: &mut Inner) {
    if let Ok(disk) = Config::load_from(&g.cfg_path) {
        g.cfg = disk;
    }
}

fn persist_cfg(g: &mut Inner, patch: impl FnOnce(&mut Config)) -> Result<(), (StatusCode, String)> {
    let disk = Config::mutate_disk(&g.cfg_path, patch)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    g.cfg = disk;
    Ok(())
}

fn sync_cron_from_disk(g: &mut Inner) {
    g.cron = CronStore::reload(g.session.workspace(), &g.cron);
}

#[derive(Deserialize)]
struct RpcBody {
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

async fn rpc(State(st): State<AppState>, Json(body): Json<RpcBody>) -> Json<Value> {
    Json(st.rpc(&body.method, body.params).await)
}

async fn state_get(State(st): State<AppState>) -> Json<Value> {
    let g = st.inner.lock().await;
    let mut v = g.session.state_json();
    v["permit"] = json!(g.pending.front().map(|p| p.json()));
    v["jobs"] = json!(g.cron.jobs.len());
    Json(v)
}

async fn history_get(State(st): State<AppState>) -> Json<Value> {
    let g = st.inner.lock().await;
    Json(json!({"ok": true, "events": g.session.events()}))
}

async fn usage_get(State(st): State<AppState>) -> Json<Value> {
    let g = st.inner.lock().await;
    let mut v = UsageRecap::from_events(g.session.events()).json();
    v["ok"] = json!(true);
    v["session"] = json!(g.session.session_id());
    v["window"] = json!(g.session.window());
    Json(v)
}

async fn model_ping(State(st): State<AppState>) -> Json<Value> {
    let (cfg, fallback) = {
        let g = st.inner.lock().await;
        (g.cfg.clone(), g.session.model().to_string())
    };
    match ping_models(&cfg).await {
        Ok(model) => Json(json!({"ok": true, "model": model})),
        Err(e) => Json(json!({
            "ok": false,
            "model": if fallback.is_empty() { cfg.server.model } else { fallback },
            "error": e.to_string(),
        })),
    }
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(st): State<AppState>,
) -> Response {
    if !trusted_ws_origin(&headers) {
        return (StatusCode::FORBIDDEN, "untrusted websocket origin").into_response();
    }
    ws.on_upgrade(move |socket| client_ws(socket, st))
        .into_response()
}

fn is_loopback_origin(origin: &axum::http::HeaderValue) -> bool {
    let Ok(raw) = origin.to_str() else {
        return false;
    };
    let Some(authority) = raw
        .strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))
        .and_then(|s| s.split('/').next())
    else {
        return false;
    };
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority)
        .to_ascii_lowercase();
    host == "localhost"
        || host.starts_with("localhost:")
        || host == "127.0.0.1"
        || host.starts_with("127.0.0.1:")
        || host == "[::1]"
        || host.starts_with("[::1]:")
}

fn trusted_ws_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        // Native clients and local diagnostics do not send Origin.
        return true;
    };
    if is_loopback_origin(origin) {
        return true;
    }
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Ok(raw) = origin.to_str() else {
        return false;
    };
    raw.strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))
        .and_then(|s| s.split('/').next())
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

async fn client_ws(mut socket: WebSocket, st: AppState) {
    let mut rx = st.bus.subscribe();
    {
        let g = st.inner.lock().await;
        let hello = json!({
            "jsonrpc": "2.0",
            "method": "hello",
            "params": {
                "state": g.session.state_json(),
                "events": g.session.events(),
                "permit": g.pending.front().map(|p| p.json()),
            }
        });
        if socket
            .send(Message::Text(hello.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(v) => {
                        if socket.send(Message::Text(v.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let hello = {
                            let g = st.inner.lock().await;
                            json!({
                                "jsonrpc": "2.0",
                                "method": "hello",
                                "params": {
                                    "state": g.session.state_json(),
                                    "events": g.session.events(),
                                    "permit": g.pending.front().map(|p| p.json()),
                                }
                            })
                        };
                        if socket
                            .send(Message::Text(hello.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                                let params = v.get("params").cloned();
                                let _ = st.rpc(method, params).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn upload(
    State(st): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (workspace, cap) = {
        let g = st.inner.lock().await;
        (
            g.session.workspace().to_path_buf(),
            max_upload(g.cfg.media.max_bytes),
        )
    };
    let mut files = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let name = field
            .file_name()
            .or_else(|| field.name())
            .unwrap_or("upload")
            .to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        let uploaded = write_upload(&workspace, &name, &bytes, cap)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        files.push(uploaded);
    }
    Ok(Json(json!({"ok": true, "files": files})))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

async fn files_get(
    State(st): State<AppState>,
    Query(q): Query<FileQuery>,
) -> Result<Response, (StatusCode, String)> {
    // 只在锁内拷 workspace 路径,文件 IO 放到锁外的阻塞线程,
    // 大文件预览不再卡住整个控制台
    let workspace = {
        let g = st.inner.lock().await;
        g.session.workspace().to_path_buf()
    };
    let rel = q.path.clone();
    let (mime, body, _trunc) =
        tokio::task::spawn_blocking(move || read_preview(&workspace, &rel, 8 * 1024 * 1024))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, file_content_type(&mime));
    headers.insert(header::CONTENT_DISPOSITION, file_disposition(&q.path));
    Ok((headers, body).into_response())
}

async fn tree_get(State(st): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    let g = st.inner.lock().await;
    let root = g.session.workspace();
    let rows =
        list_tree(root, 2000).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({
        "ok": true,
        "root": root.display().to_string(),
        "parent": parent_dir(root),
        "entries": rows,
    })))
}

#[derive(Deserialize, Default)]
struct WorkspacePatch {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize, Default)]
struct WorkspaceLsQuery {
    #[serde(default)]
    path: String,
}

fn workspace_busy_err() -> (StatusCode, String) {
    (
        StatusCode::CONFLICT,
        "正在跑一轮，先等它结束或点停止，再换工作区。".into(),
    )
}

fn apply_workspace(g: &mut Inner, path: PathBuf) -> Result<Value, (StatusCode, String)> {
    if g.session.turn_in_flight() || g.live.is_some() {
        return Err(workspace_busy_err());
    }
    persist_cfg(g, |cfg| {
        cfg.console.workspace = path.display().to_string();
    })?;
    g.session.set_workspace(path);
    sync_cron_from_disk(g);
    push_state(g);
    Ok(json!({
        "ok": true,
        "workspace": g.session.workspace().display().to_string(),
        "parent": parent_dir(g.session.workspace()),
    }))
}

async fn workspace_get(State(st): State<AppState>) -> Json<Value> {
    let g = st.inner.lock().await;
    Json(json!({
        "ok": true,
        "workspace": g.session.workspace().display().to_string(),
        "parent": parent_dir(g.session.workspace()),
        "turn_in_flight": g.session.turn_in_flight() || g.live.is_some(),
        "saved": g.cfg.console.workspace,
        "shortcuts": workspace_shortcuts(),
    }))
}

async fn workspace_post(
    State(st): State<AppState>,
    Json(p): Json<WorkspacePatch>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let current = {
        let g = st.inner.lock().await;
        if g.session.turn_in_flight() || g.live.is_some() {
            return Err(workspace_busy_err());
        }
        g.session.workspace().to_path_buf()
    };
    let raw = p.path;
    let path = tokio::task::spawn_blocking(move || resolve_workspace_dir(&raw, Some(&current)))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut g = st.inner.lock().await;
    Ok(Json(apply_workspace(&mut g, path)?))
}

async fn workspace_pick(State(st): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    {
        let g = st.inner.lock().await;
        if g.session.turn_in_flight() || g.live.is_some() {
            return Err(workspace_busy_err());
        }
    }
    let picked = tokio::task::spawn_blocking(pick_folder_native)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let Some(path) = picked else {
        return Ok(Json(json!({"ok": true, "cancelled": true})));
    };
    let mut g = st.inner.lock().await;
    Ok(Json(apply_workspace(&mut g, path)?))
}

async fn workspace_ls(
    Query(q): Query<WorkspaceLsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let raw = q.path;
    let (path, parent, dirs) = tokio::task::spawn_blocking(move || list_child_dirs(&raw, 400))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(json!({
        "ok": true,
        "path": path.display().to_string(),
        "parent": parent,
        "dirs": dirs,
    })))
}

/// 进程里存在、但 `q38 web` 不会读取的 Q38_* 覆盖变量名。
/// `Q38_CONSOLE_DIR` 例外:web 模式确实会读它,不列为被忽略。
pub(crate) fn env_ignored_names() -> Vec<String> {
    let mut out: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("Q38_") && k != "Q38_CONSOLE_DIR")
        .collect();
    out.sort();
    out
}

fn config_public(cfg: &Config) -> Value {
    json!({
        "ok": true,
        "agent_scope": if cfg.features.workspace_write_only { "workspace" } else { "global" },
        "server": {
            "base_url": cfg.server.base_url,
            "api_key": redact_key(&cfg.server.api_key),
            "api_key_set": !cfg.server.api_key.is_empty(),
            "model": cfg.server.model,
            "family": format!("{}", cfg.server.family),
            "profile": format!("{}", cfg.server.profile),
        },
        "context": cfg.context,
        "policy": {
            "max_tokens": cfg.policy.max_tokens,
            "max_steps": cfg.policy.max_steps,
            "default_mode": cfg.policy.default_mode,
            "low_precision": cfg.policy.low_precision,
        },
        "features": cfg.features,
        "media": { "enabled": cfg.media.enabled, "max_bytes": cfg.media.max_bytes },
        "prompt": { "coding": cfg.prompt.coding, "file": cfg.prompt.file },
        "web": {
            "enabled": cfg.web.enabled,
            "provider": cfg.web.provider,
            "tavily_key_set": !cfg.web.tavily_api_key.trim().is_empty(),
        },
        "env_ignored": env_ignored_names(),
    })
}

async fn config_get(State(st): State<AppState>) -> Json<Value> {
    let mut g = st.inner.lock().await;
    sync_cfg_from_disk(&mut g);
    Json(config_public(&g.cfg))
}

#[derive(Deserialize, Default)]
struct ConfigPatch {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    approvals: Option<String>,
    #[serde(default)]
    workspace_write_only: Option<bool>,
    #[serde(default)]
    skills_auto_catalog: Option<bool>,
    #[serde(default)]
    mcp_auto_catalog: Option<bool>,
    #[serde(default)]
    low_precision: Option<bool>,
    #[serde(default)]
    working_window: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    max_steps: Option<u32>,
    #[serde(default)]
    web_enabled: Option<bool>,
    /// 空串=清除;缺省=不动(GET 不回显 key 本体,只给 tavily_key_set)
    #[serde(default)]
    web_tavily_api_key: Option<String>,
}

async fn config_post(
    State(st): State<AppState>,
    Json(p): Json<ConfigPatch>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut g = st.inner.lock().await;
    if let Some(ref u) = p.base_url {
        let u = u.trim().trim_end_matches('/');
        if u.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "endpoint empty".into()));
        }
    }
    if let Some(n) = p.working_window {
        parse_working_window(n).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(n) = p.max_tokens {
        parse_max_tokens(n).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(n) = p.max_steps {
        if n == 0 || n > 10_000 {
            return Err((
                StatusCode::BAD_REQUEST,
                "max_steps must be 1..=10000".into(),
            ));
        }
    }
    persist_cfg(&mut g, |cfg| {
        if let Some(u) = &p.base_url {
            cfg.server.base_url = u.trim().trim_end_matches('/').to_string();
        }
        if let Some(k) = p
            .api_key
            .clone()
            .filter(|s| !s.trim().is_empty() && !s.starts_with("****"))
        {
            cfg.server.api_key = k;
        }
        if let Some(m) = &p.model {
            cfg.server.model = m.trim().to_string();
        }
        if let Some(a) = &p.approvals {
            if let Some(mode) = ApprovalMode::parse(a) {
                cfg.features.approvals = mode.as_str().into();
            }
        }
        if let Some(v) = p.workspace_write_only {
            cfg.features.workspace_write_only = v;
        }
        if let Some(v) = p.skills_auto_catalog {
            cfg.features.skills_auto_catalog = v;
        }
        if let Some(v) = p.mcp_auto_catalog {
            cfg.features.mcp_auto_catalog = v;
        }
        if let Some(v) = p.low_precision {
            cfg.policy.low_precision = v;
        }
        if let Some(n) = p.working_window {
            if let Ok(n) = parse_working_window(n) {
                cfg.context.working_window = n;
                if cfg.context.hard_cap < n {
                    cfg.context.hard_cap = n;
                }
            }
        }
        if let Some(n) = p.max_tokens {
            if let Ok(n) = parse_max_tokens(n) {
                cfg.policy.max_tokens = n;
            }
        }
        if let Some(n) = p.max_steps {
            cfg.policy.max_steps = n;
        }
        if let Some(v) = p.web_enabled {
            cfg.web.enabled = v;
        }
        if let Some(k) = &p.web_tavily_api_key {
            cfg.web.tavily_api_key = k.trim().to_string();
        }
    })?;
    let model = g.cfg.server.model.clone();
    let low_precision = g.cfg.policy.low_precision;
    let window = g.cfg.context.working_window;
    let max_tokens = g.cfg.policy.max_tokens;
    if p.model.is_some() {
        g.session.set_model(model);
    }
    if let Some(a) = &p.approvals {
        if let Some(mode) = ApprovalMode::parse(a) {
            g.session.set_approvals_mode(mode);
            g.permit.set_mode(mode);
        }
    }
    if let Some(v) = p.workspace_write_only {
        g.session.set_workspace_confined(v);
    }
    if p.low_precision.is_some() {
        g.session.set_low_precision(low_precision);
    }
    if p.working_window.is_some() {
        g.session.set_window(window);
    }
    if p.max_tokens.is_some() {
        g.session.set_max_tokens_cap(max_tokens);
    }
    let public = config_public(&g.cfg);
    let state = g.session.state_json();
    let bus = st.bus.clone();
    drop(g);
    let _ = bus.send(crate::hub::notify("state", state));
    Ok(Json(public))
}

#[derive(Deserialize)]
struct PermitBody {
    id: u64,
    decision: String,
}

async fn permit_post(
    State(st): State<AppState>,
    Json(body): Json<PermitBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let decision = match body.decision.to_ascii_lowercase().as_str() {
        "allow" | "y" | "yes" => PermitDecision::Allow,
        "always" | "a" => PermitDecision::Always,
        "deny" | "n" | "no" => PermitDecision::Deny,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "decision must be allow|always|deny".into(),
            ))
        }
    };
    st.decide_permit(body.id, decision)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

async fn skills_get(State(st): State<AppState>) -> Json<Value> {
    let mut g = st.inner.lock().await;
    sync_cfg_from_disk(&mut g);
    let home = Config::home_dir().ok();
    let cat = SkillCatalog::load(
        home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
        g.session.workspace(),
    );
    let rows: Vec<Value> = cat
        .skills
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "path": s.path.display().to_string(),
            })
        })
        .collect();
    Json(json!({
        "ok": true,
        "auto_catalog": g.cfg.features.skills_auto_catalog,
        "skills": rows,
    }))
}

#[derive(Deserialize)]
struct FlagBody {
    #[serde(default)]
    auto_catalog: Option<bool>,
}

async fn skills_post(
    State(st): State<AppState>,
    Json(body): Json<FlagBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut g = st.inner.lock().await;
    if let Some(v) = body.auto_catalog {
        persist_cfg(&mut g, |cfg| {
            cfg.features.skills_auto_catalog = v;
        })?;
    }
    Ok(Json(
        json!({"ok": true, "auto_catalog": g.cfg.features.skills_auto_catalog}),
    ))
}

async fn mcp_get(State(st): State<AppState>) -> Json<Value> {
    let mut g = st.inner.lock().await;
    sync_cfg_from_disk(&mut g);
    let home = Config::home_dir().ok();
    let reg = McpRegistry::load(home.as_deref(), g.session.workspace(), &g.cfg.mcp);
    let config_names: std::collections::BTreeSet<String> = g
        .cfg
        .mcp
        .servers
        .iter()
        .map(|s| s.name.to_ascii_lowercase())
        .collect();
    let rows: Vec<Value> = reg
        .servers
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "command": s.command,
                "args": s.args,
                "description": s.description,
                "methods": s.methods,
                "cwd": s.cwd,
                "editable": config_names.contains(&s.name.to_ascii_lowercase()),
            })
        })
        .collect();
    let editable: Vec<Value> = g
        .cfg
        .mcp
        .servers
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "command": s.command,
                "args": s.args,
                "description": s.description,
                "methods": s.methods,
                "cwd": s.cwd,
                "env_set": !s.env.is_empty(),
            })
        })
        .collect();
    Json(json!({
        "ok": true,
        "auto_catalog": g.cfg.features.mcp_auto_catalog,
        "servers": rows,
        "editable": editable,
    }))
}

#[derive(Deserialize)]
struct McpPatch {
    #[serde(default)]
    auto_catalog: Option<bool>,
    #[serde(default)]
    add: Option<q38_loop::mcp::McpServer>,
    #[serde(default)]
    remove: Option<String>,
    #[serde(default)]
    servers: Option<Vec<q38_loop::mcp::McpServer>>,
}

async fn mcp_post(
    State(st): State<AppState>,
    Json(body): Json<McpPatch>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut g = st.inner.lock().await;
    if let Some(add) = &body.add {
        if add.name.trim().is_empty() || add.command.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "add requires name and command".into(),
            ));
        }
    }
    if let Some(name) = body.remove.as_deref().map(str::trim) {
        if name.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "remove requires name".into()));
        }
    }
    persist_cfg(&mut g, |cfg| {
        if let Some(v) = body.auto_catalog {
            cfg.features.mcp_auto_catalog = v;
        }
        if let Some(add) = body.add {
            upsert_mcp_server(&mut cfg.mcp.servers, add);
        }
        if let Some(name) = body.remove {
            remove_mcp_server(&mut cfg.mcp.servers, name.trim());
        }
        if let Some(servers) = body.servers {
            cfg.mcp.servers = merge_mcp_servers(&cfg.mcp.servers, servers);
        }
    })?;
    Ok(Json(
        json!({"ok": true, "auto_catalog": g.cfg.features.mcp_auto_catalog}),
    ))
}

#[derive(Deserialize)]
struct McpTestBody {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

/// 拉起命令跑一次 initialize + tools/list(超时 10s),不落盘不进注册表。
/// 复用 q38-loop 的 MCP 客户端(run_mcp),spawn 环境与真实运行一致。
async fn mcp_test(Json(body): Json<McpTestBody>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.command.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "command 不能为空"})),
        ));
    }
    let server = q38_loop::mcp::McpServer {
        name: "test".into(),
        command: body.command,
        args: body.args,
        env: body.env,
        ..Default::default()
    };
    let reg = McpRegistry::with_servers(vec![server], std::time::Duration::from_secs(10));
    let call = q38_loop::ToolCall {
        id: "mcp-test".into(),
        name: "mcp".into(),
        arguments: json!({"server": "test", "method": "tools/list"}),
    };
    // head 放到极大关掉折叠,tools/list 回包保持原文可解析
    let limits = q38_loop::tools::ToolLimits {
        result_head_chars: usize::MAX,
        result_tail_chars: 0,
        ..Default::default()
    };
    let resp = q38_loop::mcp::run_mcp(&reg, &call, limits, None).await;
    let text = resp.joined_text();
    if resp.state == q38_loop::ToolState::Success {
        let tools: Vec<String> = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("tools").and_then(|t| t.as_array()).cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Json(json!({"ok": true, "tools": tools, "error": null})))
    } else {
        let msg = text.strip_prefix("Error: ").unwrap_or(&text).to_string();
        Ok(Json(json!({"ok": false, "tools": [], "error": msg})))
    }
}

async fn channels_get(State(st): State<AppState>) -> Json<Value> {
    let mut g = st.inner.lock().await;
    sync_cfg_from_disk(&mut g);
    let endpoints: Vec<Value> = g
        .cfg
        .channels
        .endpoints
        .iter()
        .map(|e| {
            // watcher 还没跑到的新 endpoint 用静态分类兜底
            let runtime = g
                .channel_runtime
                .get(&e.id)
                .cloned()
                .unwrap_or_else(|| crate::hub::endpoint_static_runtime(e));
            json!({
                "id": e.id,
                "kind": e.kind,
                "enabled": e.enabled,
                "bind": e.bind,
                "reply_url": e.reply_url,
                "require_mention": e.require_mention,
                "dm_policy": e.dm_policy,
                "group_policy": e.group_policy,
                "secret_set": !e.secret.is_empty(),
                "bot_token_set": extra_has_bot_token(&e.extra),
                "creds_set": extra_creds_set(&e.extra),
                "allow_from": e.allow_from,
                "deny_from": e.deny_from,
                "extra": extra_public(&e.extra),
                "runtime": runtime.json(),
            })
        })
        .collect();
    Json(json!({
        "ok": true,
        "busy": g.cfg.channels.busy,
        "enabled": g.cfg.channels.enabled,
        "builtin": g.cfg.channels.list_json(),
        "catalog": catalog_json(),
        "in_process": ["telegram", "webhook", "qq", "wechat", "wecom", "dingtalk", "feishu"],
        "endpoints": endpoints,
    }))
}

fn extra_key_secretish(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("token") || k.contains("secret") || k.contains("key")
}

fn extra_has_bot_token(extra: &std::collections::BTreeMap<String, String>) -> bool {
    extra
        .get("bot_token")
        .or_else(|| extra.get("token"))
        .is_some_and(|s| !s.is_empty())
}

fn extra_creds_set(extra: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    extra
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, _)| k.clone())
        .collect()
}

fn extra_public(extra: &std::collections::BTreeMap<String, String>) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in extra {
        if extra_key_secretish(k) {
            continue;
        }
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

#[derive(Deserialize)]
struct ChannelsPatch {
    #[serde(default)]
    busy: Option<String>,
    #[serde(default)]
    upsert: Option<ChannelEndpoint>,
    #[serde(default)]
    rename: Option<String>,
    #[serde(default)]
    remove: Option<String>,
    #[serde(default)]
    endpoints: Option<Vec<ChannelEndpoint>>,
}

fn check_endpoint_kind(ep: &ChannelEndpoint) -> Result<(), (StatusCode, String)> {
    if !endpoint_kind(&ep.kind) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown channel kind {}", ep.kind),
        ));
    }
    Ok(())
}

fn qr_http(err: q38_loop::channel::qrcode::QrError) -> (StatusCode, String) {
    use q38_loop::channel::qrcode::QrError;
    match err {
        QrError::UnknownKind(_) => (StatusCode::NOT_FOUND, err.to_string()),
        QrError::BadToken(_) => (StatusCode::BAD_REQUEST, err.to_string()),
        QrError::Upstream(_) => (StatusCode::BAD_GATEWAY, err.to_string()),
        QrError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

#[derive(Deserialize)]
struct QrQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    domain: Option<String>,
}

async fn channel_qrcode(
    Path(kind): Path<String>,
    Query(q): Query<QrQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let fetched = fetch_qrcode(&kind, q.domain.as_deref())
        .await
        .map_err(qr_http)?;
    Ok(Json(json!({
        "ok": true,
        "image": fetched.image,
        "poll_token": fetched.poll_token,
        "scan_url": fetched.scan_url,
    })))
}

async fn channel_qrcode_status(
    Path(kind): Path<String>,
    Query(q): Query<QrQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let polled = poll_qrcode(&kind, &q.token, q.domain.as_deref())
        .await
        .map_err(qr_http)?;
    Ok(Json(json!({
        "ok": true,
        "status": polled.status,
        "credentials": polled.credentials,
    })))
}

async fn channels_post(
    State(st): State<AppState>,
    Json(body): Json<ChannelsPatch>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut g = st.inner.lock().await;
    if let Some(add) = &body.upsert {
        check_endpoint_kind(add)?;
    }
    if let Some(eps) = &body.endpoints {
        for ep in eps {
            check_endpoint_kind(ep)?;
        }
    }
    if let Some(id) = body.remove.as_deref().map(str::trim) {
        if id.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "remove requires id".into()));
        }
    }
    persist_cfg(&mut g, |cfg| {
        if let Some(ref busy) = body.busy {
            cfg.channels.busy = busy.clone();
        }
        if let Some(old) = body
            .rename
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(add) = &body.upsert {
                if add.id.trim() != old {
                    remove_channel_endpoint(&mut cfg.channels.endpoints, old);
                }
            }
        }
        if let Some(add) = body.upsert {
            upsert_channel_endpoint(&mut cfg.channels.endpoints, add);
        }
        if let Some(id) = body.remove {
            remove_channel_endpoint(&mut cfg.channels.endpoints, id.trim());
        }
        if let Some(eps) = body.endpoints {
            cfg.channels.endpoints = merge_channel_endpoints(&cfg.channels.endpoints, eps);
        }
    })?;
    if let Some(busy) = &body.busy {
        if let Ok(p) = busy.parse::<BusyPolicy>() {
            g.session.set_busy(p);
        }
    }
    let state = g.session.state_json();
    let bus = st.bus.clone();
    drop(g);
    let _ = bus.send(crate::hub::notify("state", state));
    Ok(Json(json!({"ok": true})))
}

/// 当前 web 搜索后端:强制 builtin 之外,只要能找到 Tavily key
/// (config → 环境变量 → MCP env,与 tools/web.rs 的查找顺序一致)即 tavily。
fn web_provider(cfg: &Config, workspace: &std::path::Path) -> &'static str {
    if cfg.web.provider.eq_ignore_ascii_case("builtin") {
        return "builtin";
    }
    if !cfg.web.tavily_api_key.trim().is_empty() {
        return "tavily";
    }
    if std::env::var("TAVILY_API_KEY").is_ok_and(|v| !v.trim().is_empty()) {
        return "tavily";
    }
    let home = Config::home_dir().ok();
    let reg = McpRegistry::load(home.as_deref(), workspace, &cfg.mcp);
    if reg.servers.iter().any(|s| {
        s.env
            .get("TAVILY_API_KEY")
            .is_some_and(|k| !k.trim().is_empty())
    }) {
        return "tavily";
    }
    "builtin"
}

async fn tools_get(State(st): State<AppState>) -> Json<Value> {
    let mut g = st.inner.lock().await;
    sync_cfg_from_disk(&mut g);
    let provider = web_provider(&g.cfg, g.session.workspace());
    let mut web = web_tool();
    web["function"]["description"] = json!(format!(
        "Web search (query) or fetch a page (url). provider: {provider}"
    ));
    Json(json!({
        "ok": true,
        "frozen": agent_tools(),
        "note": "OpenAI tools[] stays frozen (read/write/edit/bash). view, mcp, and memory_search may be appended after that prefix; skill is never in tools[] (hidden cards / slash). Session start tools_hash includes whatever was appended at open.",
        "view": view_tool(),
        "skill": skill_tool(),
        "mcp": mcp_tool(),
        "web": web,
    }))
}

async fn jobs_get(State(st): State<AppState>) -> Json<Value> {
    let mut g = st.inner.lock().await;
    sync_cron_from_disk(&mut g);
    Json(g.cron.json())
}

#[derive(Deserialize)]
struct JobsPatch {
    #[serde(default)]
    add: Option<CronJob>,
    #[serde(default)]
    remove: Option<String>,
    #[serde(default)]
    jobs: Option<Vec<CronJob>>,
}

fn jobs_err(code: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({"error": msg})))
}

async fn jobs_post(
    State(st): State<AppState>,
    Json(body): Json<JobsPatch>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // interval_s=0 的任务调度层永不触发,保存时直接拒绝
    let zero_interval = body.add.iter().chain(body.jobs.iter().flatten());
    if zero_interval.into_iter().any(|j| j.interval_s < 1) {
        return Err(jobs_err(
            StatusCode::BAD_REQUEST,
            "定时任务间隔 interval_s 必须 ≥ 1 秒",
        ));
    }
    let mut g = st.inner.lock().await;
    sync_cron_from_disk(&mut g);
    if let Some(add) = body.add {
        upsert_cron_job(&mut g.cron.jobs, add);
    }
    if let Some(id) = body.remove {
        let id = id.trim();
        if id.is_empty() {
            return Err(jobs_err(StatusCode::BAD_REQUEST, "remove 需要 id"));
        }
        remove_cron_job(&mut g.cron.jobs, id);
        drop_workspace_job(g.session.workspace(), id);
    }
    if let Some(jobs) = body.jobs {
        g.cron.jobs = overlay_cron_jobs(&g.cron.jobs, jobs);
    }
    g.cron
        .save_with_workspace(g.session.workspace())
        .map_err(|e| jobs_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(g.cron.json()))
}

async fn heartbeat_get(State(st): State<AppState>) -> Json<Value> {
    let mut g = st.inner.lock().await;
    sync_cron_from_disk(&mut g);
    Json(json!({
        "ok": true,
        "heartbeat": g.cron.heartbeat,
        "resolved_prompt": heartbeat_prompt(&g.cron, g.session.workspace()),
    }))
}

#[derive(Deserialize)]
struct HbPatch {
    #[serde(flatten)]
    heartbeat: Heartbeat,
}

async fn heartbeat_post(
    State(st): State<AppState>,
    Json(body): Json<HbPatch>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut g = st.inner.lock().await;
    sync_cron_from_disk(&mut g);
    let mut hb = body.heartbeat;
    hb.last_run = later_ts(g.cron.heartbeat.last_run, hb.last_run);
    g.cron.heartbeat = hb;
    g.cron
        .save()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({"ok": true, "heartbeat": g.cron.heartbeat})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_origins_are_local_or_same_host() {
        let local = axum::http::HeaderValue::from_static("http://127.0.0.1:5173");
        let remote = axum::http::HeaderValue::from_static("https://evil.example");
        assert!(is_loopback_origin(&local));
        assert!(!is_loopback_origin(&remote));

        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "http://192.168.5.10:3848".parse().unwrap());
        headers.insert(header::HOST, "192.168.5.10:3848".parse().unwrap());
        assert!(trusted_ws_origin(&headers));
        headers.insert(header::ORIGIN, remote);
        assert!(!trusted_ws_origin(&headers));
    }

    #[test]
    fn config_patch_accepts_agent_scope_switch() {
        let patch: ConfigPatch = serde_json::from_value(json!({
            "workspace_write_only": false
        }))
        .unwrap();
        assert_eq!(patch.workspace_write_only, Some(false));
    }

    #[test]
    fn public_config_reports_agent_scope() {
        let mut cfg = Config::default();
        assert_eq!(config_public(&cfg)["agent_scope"], "workspace");
        cfg.features.workspace_write_only = false;
        assert_eq!(config_public(&cfg)["agent_scope"], "global");
    }

    const ECHO_MCP_PY: &str = r#"
import json, sys, os

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
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"0"}}})
    elif method == "tools/list":
        name = os.environ.get("TOOL_NAME", "ping")
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":name,"description":"pong"}]}})
"#;

    #[tokio::test]
    async fn mcp_test_lists_tools_and_passes_env() {
        let dir = std::env::temp_dir().join(format!("q38-web-mcpt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("echo_mcp.py");
        std::fs::write(&py, ECHO_MCP_PY).unwrap();
        let mut args = Vec::new();
        #[cfg(windows)]
        args.push("-3".to_string());
        args.push(py.to_string_lossy().into_owned());
        let body = McpTestBody {
            command: if cfg!(windows) { "py" } else { "python3" }.into(),
            args,
            env: [("TOOL_NAME".to_string(), "search".to_string())].into(),
        };
        let out = mcp_test(Json(body)).await.unwrap().0;
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["tools"], json!(["search"]), "{out}");
        assert_eq!(out["error"], Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mcp_test_reports_spawn_failure() {
        let body = McpTestBody {
            command: "/nonexistent/q38-mcp-test-binary".into(),
            args: Vec::new(),
            env: Default::default(),
        };
        let out = mcp_test(Json(body)).await.unwrap().0;
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["tools"], json!([]));
        assert!(out["error"].is_string(), "{out}");
    }

    #[tokio::test]
    async fn mcp_test_rejects_empty_command() {
        let body = McpTestBody {
            command: "  ".into(),
            args: Vec::new(),
            env: Default::default(),
        };
        let (code, err) = mcp_test(Json(body)).await.unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(err.0["error"].is_string());
    }
}
