//! In-process HTTP/WS host for the local console. Same `SidecarSession` as TUI.

mod cron;
mod files;
mod hub;
mod routes;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use q38_loop::config::Config;
use q38_loop::session::{catalog, new_session_id, SessionLog, SessionMode};
use q38_loop::sidecar::{PolicyCaps, SidecarOpts, SidecarSession};
use q38_loop::vendor;
use q38_loop::ApprovalMode;

use crate::hub::AppState;

pub struct WebOpts {
    pub bind: String,
    pub allow_lan: bool,
    pub workspace: PathBuf,
    pub session_id: String,
    pub open_browser: bool,
    pub agents_md: bool,
    pub agents_md_head: bool,
}

pub async fn run(opts: WebOpts) -> Result<std::process::ExitCode> {
    let (cfg, cfg_path) = Config::load_or_init_file().context("load config")?;
    vendor::verify_vendors().ok();
    // Q38_* overlay 仅 CLI/TUI 生效,web 模式只读 config.toml;启动时提醒一次
    let ignored = routes::env_ignored_names();
    if !ignored.is_empty() {
        eprintln!(
            "q38 web: Q38_* 环境变量在 web 模式下不生效，请在控制台设置页或 config.toml 配置（检测到: {}）",
            ignored.join(", ")
        );
    }

    let workspace = crate::files::resolve_web_workspace(&opts.workspace, &cfg.console.workspace)
        .context("workspace")?;
    let session_id = if !opts.session_id.is_empty() {
        opts.session_id
    } else {
        SessionLog::sessions_dir()
            .ok()
            .map(|dir| catalog::resume_console_id(&dir, ""))
            .unwrap_or_else(new_session_id)
    };
    let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
    let mut session = SidecarSession::new(SidecarOpts {
        session_id,
        workspace: workspace.clone(),
        mode: SessionMode::Agent,
        policy,
        caps: PolicyCaps::from_config(&cfg),
        persist: true,
        effort_locked: false,
        model: cfg.server.model.clone(),
        family: cfg.server.family,
        window: cfg.context.working_window,
        busy: cfg.channels.busy_policy(),
        channels: cfg.channels.clone(),
        channel: "console".into(),
        low_precision: cfg.policy.low_precision,
        workspace_confined: cfg.features.workspace_write_only,
        approvals: ApprovalMode::parse(&cfg.features.approvals).unwrap_or(ApprovalMode::Ask),
        sessions_dir: None,
        home: None,
    });
    let open = q38_loop::sidecar::RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(1)),
        method: "session.open".into(),
        params: Some(serde_json::json!({
            "session": session.session_id(),
            "workspace": workspace.display().to_string(),
            "mode": "agent",
        })),
    };
    match session.handle(&open) {
        q38_loop::sidecar::Dispatch::Result { .. } => {}
        q38_loop::sidecar::Dispatch::Error(err) => anyhow::bail!("{}", err.message),
        other => anyhow::bail!("session.open: {other:?}"),
    }

    let addr: SocketAddr = opts.bind.parse().context("bind address")?;
    if !addr.ip().is_loopback() && !opts.allow_lan {
        anyhow::bail!(
            "refusing non-loopback bind {addr}; pass --allow-lan to expose the local console"
        );
    }
    let state = AppState::new(session, cfg, cfg_path, opts.agents_md, opts.agents_md_head)?;
    state.spawn_background();

    let app = routes::router(state, console_dir());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let bound = listener.local_addr().context("local addr")?;
    let url = format!("http://{bound}/");
    eprintln!("q38 web  {url}");
    eprintln!("workspace {}", workspace.display());
    if opts.open_browser {
        open_browser(&url);
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve")?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn console_dir() -> PathBuf {
    if let Ok(p) = std::env::var("Q38_CONSOLE_DIR") {
        let p = PathBuf::from(p);
        if p.join("index.html").is_file() {
            return p;
        }
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .and_then(|dir| console_dir_near(&dir))
    {
        return dir;
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for cand in [
        manifest.join("../../web/console/dist"),
        std::env::current_dir()
            .ok()
            .map(|c| c.join("web/console/dist"))
            .unwrap_or_default(),
        Config::home_dir()
            .map(|h| h.join("console"))
            .unwrap_or_default(),
    ] {
        if cand.join("index.html").is_file() {
            return cand;
        }
    }
    manifest.join("../../web/console/dist")
}

/// Packaged layout: `Resources/bin/q38` + `Resources/console/`, or the two
/// sitting next to each other.
fn console_dir_near(exe_dir: &std::path::Path) -> Option<PathBuf> {
    let mut cands = vec![exe_dir.join("console")];
    if let Some(parent) = exe_dir.parent() {
        cands.push(parent.join("console"));
    }
    cands
        .into_iter()
        .find(|cand| cand.join("index.html").is_file())
}

fn open_browser(url: &str) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_bind_is_the_safe_default() {
        let local: SocketAddr = "127.0.0.1:3848".parse().unwrap();
        let lan: SocketAddr = "0.0.0.0:3848".parse().unwrap();
        assert!(local.ip().is_loopback());
        assert!(!lan.ip().is_loopback());
    }

    #[test]
    fn packaged_console_sits_beside_or_above_the_binary() {
        let root = std::env::temp_dir().join(format!("q38-pack-{}", std::process::id()));
        let bin = root.join("bin");
        let console = root.join("console");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&console).unwrap();
        std::fs::write(console.join("index.html"), "<html></html>").unwrap();
        assert_eq!(console_dir_near(&bin).as_deref(), Some(console.as_path()));
        assert_eq!(console_dir_near(&root).as_deref(), Some(console.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
