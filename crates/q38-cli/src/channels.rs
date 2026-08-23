//! `q38 --channels`: QwenPaw-form inbound (webhook + telegram) → Agent → reply.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use q38_loop::channel::{reply_text, run_channels, ContentPart, NativePayload};
use q38_loop::config::Config;
use q38_loop::session::SessionMode;
use q38_loop::slash::{low_precision_text, mcp_text, parse_slash_with_periphery, skills_text};
use q38_loop::vendor;
use q38_loop::{Agent, HttpCompleter, RunOpts, ToolSet};

use super::Cli;

pub async fn run(cli: Cli) -> Result<ExitCode> {
    let (cfg, path) = Config::load_or_init().context("load config")?;
    vendor::verify_qwen38().ok();
    eprintln!("config: {}", path.display());

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

    let cfg = Arc::new(cfg);
    let cfg_h = cfg.clone();
    let workspace = workspace.clone();
    let agents_md = cli.agents_md && !cli.no_agents_md;
    let agents_md_head = cli.agents_md_head;
    let effort_locked =
        cli.fast || cli.think.is_some() || matches!(mode, SessionMode::Think | SessionMode::Chat);

    run_channels(cfg.channels.clone(), move |env: NativePayload| {
        let cfg = cfg_h.clone();
        let workspace = workspace.clone();
        let policy = policy.clone();
        async move {
            handle_inbound(
                cfg,
                workspace,
                mode,
                policy,
                effort_locked,
                agents_md,
                agents_md_head,
                env,
            )
            .await
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(ExitCode::SUCCESS)
}

async fn handle_inbound(
    cfg: Arc<Config>,
    workspace: std::path::PathBuf,
    mode: SessionMode,
    policy: q38_loop::policy::ThinkPolicy,
    effort_locked: bool,
    agents_md: bool,
    agents_md_head: bool,
    env: NativePayload,
) -> q38_loop::Result<Vec<ContentPart>> {
    let home = Config::home_dir().ok();
    let skills = q38_loop::skills::SkillCatalog::load(
        home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
        &workspace,
    );
    let mcp = q38_loop::mcp::McpRegistry::load(home.as_deref(), &workspace, &cfg.mcp);
    let query = env.query_text();
    let mut msg = env.to_chat_message();
    if let Some(cmd) = parse_slash_with_periphery(&query, &skills, Some(&mcp)) {
        if let Some(text) = local_slash_text(&cmd) {
            return Ok(reply_text(text));
        }
        match cmd {
            q38_loop::SlashCmd::Skills => return Ok(reply_text(skills_text(&skills))),
            q38_loop::SlashCmd::Mcp => return Ok(reply_text(mcp_text(&mcp))),
            q38_loop::SlashCmd::InvokeSkill { name, args } => {
                msg.content = Some(q38_loop::sticky::skill_turn_prompt(&name, &args));
            }
            q38_loop::SlashCmd::InvokeMcp { name, args } => {
                msg.content = Some(q38_loop::sticky::mcp_turn_prompt(&name, &args));
            }
            q38_loop::SlashCmd::Cron { args } => {
                return Ok(reply_text(q38_loop::cron::apply_slash(&workspace, &args)));
            }
            q38_loop::SlashCmd::LowPrecision { on } => {
                let flag = match on {
                    Some(v) => {
                        if let Ok(path) = Config::default_path() {
                            let _ = Config::mutate_disk(&path, |c| {
                                c.policy.low_precision = v;
                            });
                        }
                        v
                    }
                    None => cfg.policy.low_precision,
                };
                return Ok(reply_text(low_precision_text(flag)));
            }
            _ => {}
        }
    }

    let mut opts = RunOpts::from_config(&cfg, workspace);
    opts.print = false;
    opts.session_id = if env.session_id.is_empty() {
        "channel".into()
    } else {
        env.session_id.clone()
    };
    opts.persist_session = true;
    opts.session_mode = mode;
    opts.agents_md = agents_md;
    opts.agents_md_head = agents_md_head;
    opts.effort_locked = effort_locked || !policy.enabled;
    opts.generation_reserve = policy.max_tokens.saturating_add(policy.max_think_tokens);
    match mode {
        SessionMode::Chat => {
            opts.with_tools = false;
            opts.tool_set = ToolSet::None;
        }
        SessionMode::Code => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Code;
        }
        SessionMode::Think => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Agent;
            opts.max_steps = cfg.policy.max_steps_think;
        }
        SessionMode::Agent => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Agent;
        }
    }

    let completer = HttpCompleter::connect(&cfg, policy).await?;
    let mut agent = Agent::new(completer, opts)?;
    let out = agent.run_message(msg).await?;
    if out.text.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(reply_text(out.text))
    }
}

fn local_slash_text(cmd: &q38_loop::SlashCmd) -> Option<String> {
    use q38_loop::slash::{help_text, unsupported_text, version_text};
    match cmd {
        q38_loop::SlashCmd::Help => Some(help_text()),
        q38_loop::SlashCmd::Version => Some(version_text()),
        q38_loop::SlashCmd::Unsupported { name } => Some(unsupported_text(name)),
        _ => None,
    }
}
