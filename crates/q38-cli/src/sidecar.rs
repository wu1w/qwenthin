//! CLI glue: NDJSON JSON-RPC on stdio, `Agent::run` for `turn.start`.

use std::process::ExitCode;

use anyhow::{Context, Result};
use q38_loop::config::Config;
use q38_loop::session::SessionMode;
use q38_loop::sidecar::{execute_turn, serve_rpc, PolicyCaps, SidecarOpts, SidecarSession};
use q38_loop::vendor;
use q38_loop::ApprovalMode;
use tokio::io::BufReader;

use super::Cli;

pub async fn run(cli: Cli) -> Result<ExitCode> {
    let (cfg, _) = Config::load_or_init().context("load config")?;
    vendor::verify_qwen38().ok();

    let workspace = cli
        .workspace
        .clone()
        .unwrap_or(std::env::current_dir().context("cwd")?);
    let policy = super::think_policy(&cfg, cli.fast, cli.think.as_deref(), cli.mode.as_deref());
    let mode = cli
        .mode
        .as_deref()
        .and_then(|s| s.parse::<SessionMode>().ok())
        .unwrap_or(SessionMode::Agent);

    let session = SidecarSession::new(SidecarOpts {
        session_id: cli.session.clone().unwrap_or_default(),
        workspace,
        mode,
        policy,
        caps: PolicyCaps::from_config(&cfg),
        persist: true,
        effort_locked: cli.fast
            || cli.think.is_some()
            || matches!(mode, SessionMode::Think | SessionMode::Chat),
        model: cfg.server.model.clone(),
        family: cfg.server.family,
        window: cfg.context.working_window,
        busy: cfg.channels.busy_policy(),
        channels: cfg.channels.clone(),
        channel: "sidecar".into(),
        low_precision: cfg.policy.low_precision,
        approvals: ApprovalMode::parse(&cfg.features.approvals).unwrap_or(ApprovalMode::Ask),
    });

    let agents_md = cli.agents_md && !cli.no_agents_md;
    let agents_md_head = cli.agents_md_head;
    let reader = BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();

    serve_rpc(reader, writer, session, move |req| {
        let cfg = cfg.clone();
        async move { execute_turn(cfg, agents_md, agents_md_head, req).await }
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(ExitCode::SUCCESS)
}
