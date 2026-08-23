use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use q38_loop::config::Config;
use q38_loop::policy::{Effort, ThinkPolicy, XHIGH_WARN};
use q38_loop::probe::{run_probe, write_report};
use q38_loop::session::{catalog, SessionLog};
use q38_loop::sidecar::{Dispatch, PolicyCaps, RpcRequest, SidecarOpts, SidecarSession};
use q38_loop::skills::SkillCatalog;
use q38_loop::slash::parse_slash_with_periphery;
use q38_loop::vendor;
use q38_loop::{
    new_session_id, Agent, ApprovalMode, CancelFlag, HttpCompleter, RunOpts, SessionMode, SlashCmd,
    ToolSet,
};
use serde_json::{json, Value};

mod channels;
mod dsh_install;
mod sidecar;

#[derive(Parser, Debug)]
#[command(
    name = "q38",
    about = "q-harness CLI — reasoning-economy coding agent (primary: Qwen3.8-27B). `q38 web` is the console; a TTY with no prompt opens the TUI; --print is oneshot.",
    version,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Workspace root (default: cwd).
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,

    /// Non-interactive run (v1). Implied when a prompt is given.
    #[arg(long)]
    print: bool,

    #[arg(long, value_name = "MODE")]
    mode: Option<String>,

    #[arg(long, value_name = "ID")]
    session: Option<String>,

    /// Resume the most recent session JSONL.
    #[arg(long = "continue")]
    continue_session: bool,

    /// Resume a session by id or title substring.
    #[arg(long, value_name = "QUERY")]
    resume: Option<String>,

    #[arg(long)]
    new: bool,

    /// Thinking effort: low | medium | xhigh. Bare `--think` means medium.
    #[arg(
        long,
        value_name = "LEVEL",
        num_args = 0..=1,
        default_missing_value = "medium",
        value_parser = ["low", "medium", "xhigh"]
    )]
    think: Option<String>,

    /// Disable thinking (`enable_thinking=false`).
    #[arg(long)]
    fast: bool,

    #[arg(long)]
    sidecar: bool,

    /// Run external message channels (webhook + telegram).
    #[arg(long)]
    channels: bool,

    /// Opt in to appending workspace AGENTS.md. Off by default — system is role + boundary + tools.
    #[arg(long)]
    agents_md: bool,

    #[arg(long, hide = true)]
    no_agents_md: bool,

    /// Clip AGENTS.md to the token cap. Default omits the file if it is over the cap.
    #[arg(long)]
    agents_md_head: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    prompt: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Probe an OpenAI-compat endpoint (family / thinking / effort / timings).
    Probe {
        /// Print the JSON report on stdout.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        family: Option<String>,
    },
    /// Install q38 as a dsh profile (optional stdio client; not the product shell).
    #[command(name = "dsh-install")]
    DshInstall {
        /// dsh profile name. Default `q38`.
        #[arg(long, default_value = "q38")]
        profile: String,
        /// Do not npm-install the dsh CLI; still copy and register the plugin.
        #[arg(long)]
        skip_dsh: bool,
        /// Print paths; do not copy, build, or call dsh/npm.
        #[arg(long)]
        dry_run: bool,
    },
    /// Local Web console. Opens a browser on 127.0.0.1:3848.
    Web {
        #[arg(long, default_value = "127.0.0.1:3848")]
        bind: String,
        /// Do not open a browser.
        #[arg(long)]
        no_open: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("q38: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Probe {
            json,
            base_url,
            model,
            profile,
            family,
        }) => {
            vendor::verify_qwen38().context("qwen38 vendor set")?;
            let (mut cfg, cfg_path) = Config::load_or_init().context("load config")?;
            let overridden =
                base_url.is_some() || model.is_some() || profile.is_some() || family.is_some();
            if let Some(u) = &base_url {
                cfg.server.base_url = u.clone();
            }
            if let Some(m) = &model {
                cfg.server.model = m.clone();
            }
            if let Some(p) = &profile {
                cfg.server.profile = p.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            }
            if let Some(f) = &family {
                cfg.server.family = f.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            }
            eprintln!("config: {}", cfg_path.display());
            eprintln!("probing {} …", cfg.server.base_url);
            let report = run_probe(&cfg).await.context("probe")?;
            let out = write_report(&report).context("write probe.json")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_human(&report, &out);
            }
            // 探测通过且带了命令行覆盖 → 写盘，让「probe 成功 = 配置就绪」成立。
            if report.ok() && overridden {
                let server = cfg.server.clone();
                Config::mutate_disk(&cfg_path, |disk| {
                    if base_url.is_some() {
                        disk.server.base_url = server.base_url.clone();
                    }
                    if model.is_some() {
                        disk.server.model = server.model.clone();
                    }
                    if profile.is_some() {
                        disk.server.profile = server.profile;
                    }
                    if family.is_some() {
                        disk.server.family = server.family;
                    }
                })
                .context("persist probe overrides")?;
                eprintln!("saved: {}", cfg_path.display());
            }
            Ok(if report.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Some(Command::DshInstall {
            profile,
            skip_dsh,
            dry_run,
        }) => dsh_install::run(dsh_install::DshInstallOpts {
            profile,
            skip_dsh,
            dry_run,
        }),
        Some(Command::Web { bind, no_open }) => {
            // Empty = q38-web uses [console] workspace from config.toml, else cwd.
            let workspace = cli.workspace.clone().unwrap_or_default();
            q38_web::run(q38_web::WebOpts {
                bind,
                workspace,
                session_id: cli.session.clone().unwrap_or_default(),
                open_browser: !no_open,
                agents_md: cli.agents_md && !cli.no_agents_md,
                agents_md_head: cli.agents_md_head,
            })
            .await
        }
        None => {
            if cli.sidecar {
                return sidecar::run(cli).await;
            }
            if cli.channels {
                return channels::run(cli).await;
            }
            let mut prompt = cli.prompt.join(" ");
            if prompt.is_empty() && cli.print {
                let mut stdin = String::new();
                std::io::stdin().read_to_string(&mut stdin)?;
                prompt = stdin;
            }

            vendor::verify_qwen38().ok();
            let (cfg, _) = Config::load_or_init().context("load config")?;
            let workspace = cli
                .workspace
                .clone()
                .unwrap_or(std::env::current_dir().context("cwd")?);
            let mut policy =
                think_policy(&cfg, cli.fast, cli.think.as_deref(), cli.mode.as_deref());
            let mut mode = cli
                .mode
                .as_deref()
                .and_then(|s| s.parse::<SessionMode>().ok())
                .unwrap_or(SessionMode::Agent);
            let mut session_id = resolve_session_id(&cli)?;
            let mut effort_locked = cli.fast
                || cli.think.is_some()
                || matches!(mode, SessionMode::Think | SessionMode::Chat);
            let mut model = cfg.server.model.clone();

            if prompt.trim().is_empty() && !cli.print {
                if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                    return q38_tui::run(q38_tui::TuiOpts {
                        cfg,
                        workspace,
                        session_id,
                        mode,
                        policy,
                        effort_locked,
                        agents_md: cli.agents_md && !cli.no_agents_md,
                        agents_md_head: cli.agents_md_head,
                    })
                    .await;
                }
                if cli.continue_session || cli.resume.is_some() || cli.session.is_some() {
                    prompt = "/status".into();
                } else {
                    Cli::command().print_help()?;
                    println!();
                    return Ok(ExitCode::SUCCESS);
                }
            }

            if let Some(out) = intercept_slash(
                &cli,
                &cfg,
                &workspace,
                &prompt,
                &session_id,
                mode,
                policy.clone(),
            )? {
                match out {
                    SlashOutcome::Exit(code) => return Ok(code),
                    SlashOutcome::Turn {
                        prompt: next,
                        session_id: sid,
                        policy: next_policy,
                        effort_locked: locked,
                        mode: next_mode,
                        model: next_model,
                    } => {
                        prompt = next;
                        session_id = sid;
                        policy = next_policy;
                        effort_locked = locked;
                        mode = next_mode;
                        if !next_model.is_empty() {
                            model = next_model;
                        }
                    }
                }
            }

            let mut opts = RunOpts::from_config(&cfg, workspace);
            opts.print = true;
            opts.session_mode = mode;
            opts.persist_session = true;
            opts.with_tools = !matches!(mode, SessionMode::Chat);
            opts.agents_md = cli.agents_md && !cli.no_agents_md;
            opts.agents_md_head = cli.agents_md_head;
            opts.session_id = session_id;
            if matches!(mode, SessionMode::Think) {
                opts.max_steps = cfg.policy.max_steps_think;
            }
            opts.generation_reserve = policy.max_tokens.saturating_add(policy.max_think_tokens);
            opts.effort_locked = effort_locked;
            if matches!(mode, SessionMode::Code) {
                opts.tool_set = ToolSet::Code;
            }
            if matches!(mode, SessionMode::Chat) {
                opts.tool_set = ToolSet::None;
            }

            let mut cfg = cfg;
            if !model.is_empty()
                && std::env::var("Q38_MODEL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .is_none()
            {
                cfg.server.model = model;
            }
            let cancel = CancelFlag::new();
            let watch = arm_ctrl_c(cancel.clone());
            let completer = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    watch.abort();
                    return Ok(ExitCode::from(130));
                }
                c = HttpCompleter::connect(&cfg, policy) => c.context("connect")?,
            };
            eprintln!("model: {}", completer.model());
            eprintln!("session: {}", opts.session_id);
            let mut agent = Agent::new(completer, opts).context("agent")?;
            agent.set_cancel(cancel);
            let out = agent.run(prompt.trim()).await.context("run")?;
            watch.abort();
            if out.stop_reason.as_deref() == Some("aborted") {
                return Ok(ExitCode::from(130));
            }
            if !out.text.is_empty() {
                if out.streamed_text {
                    println!();
                } else {
                    println!("{}", out.text);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn arm_ctrl_c(cancel: CancelFlag) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(130);
    })
}

enum SlashOutcome {
    Exit(ExitCode),
    Turn {
        prompt: String,
        session_id: String,
        policy: ThinkPolicy,
        effort_locked: bool,
        mode: SessionMode,
        model: String,
    },
}

fn resolve_session_id(cli: &Cli) -> Result<String> {
    if cli.new {
        return Ok(new_session_id());
    }
    if let Some(id) = cli.session.as_deref().filter(|s| !s.is_empty()) {
        return Ok(id.to_string());
    }
    let dir = SessionLog::sessions_dir().map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(q) = cli.resume.as_deref() {
        return catalog::resolve(&dir, q)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map(|s| s.id)
            .ok_or_else(|| anyhow::anyhow!("no session matching {q}"));
    }
    if cli.continue_session {
        return catalog::latest(&dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map(|s| s.id)
            .ok_or_else(|| anyhow::anyhow!("no previous session"));
    }
    Ok(new_session_id())
}

fn intercept_slash(
    cli: &Cli,
    cfg: &Config,
    workspace: &std::path::Path,
    prompt: &str,
    session_id: &str,
    mode: SessionMode,
    policy: ThinkPolicy,
) -> Result<Option<SlashOutcome>> {
    let home = Config::home_dir().ok();
    let skills = SkillCatalog::load(
        home.as_deref().unwrap_or_else(|| std::path::Path::new("")),
        workspace,
    );
    let mcp = q38_loop::mcp::McpRegistry::load(home.as_deref(), workspace, &cfg.mcp);
    let Some(cmd) = parse_slash_with_periphery(prompt, &skills, Some(&mcp)) else {
        return Ok(None);
    };
    let keep = cli.continue_session || cli.resume.is_some() || cli.session.is_some();
    let persist = match &cmd {
        SlashCmd::Help
        | SlashCmd::Version
        | SlashCmd::Config
        | SlashCmd::Skills
        | SlashCmd::Mcp
        | SlashCmd::Tools
        | SlashCmd::Diff { .. }
        | SlashCmd::Unsupported { .. }
        | SlashCmd::Busy { .. }
        | SlashCmd::Queue { .. }
        | SlashCmd::Steer { .. }
        | SlashCmd::Stop
        | SlashCmd::Setup
        | SlashCmd::Approvals { .. }
        | SlashCmd::Plan { .. }
        | SlashCmd::LowPrecision { .. } => false,
        SlashCmd::Status
        | SlashCmd::Context { .. }
        | SlashCmd::History
        | SlashCmd::Usage
        | SlashCmd::Sessions { .. }
            if !keep =>
        {
            false
        }
        _ => true,
    };
    let mut session = SidecarSession::new(SidecarOpts {
        session_id: session_id.to_string(),
        workspace: workspace.to_path_buf(),
        mode,
        policy,
        caps: PolicyCaps::from_config(cfg),
        persist,
        effort_locked: cli.fast
            || cli.think.is_some()
            || matches!(mode, SessionMode::Think | SessionMode::Chat),
        model: cfg.server.model.clone(),
        family: cfg.server.family,
        window: cfg.context.working_window,
        busy: cfg.channels.busy_policy(),
        channels: cfg.channels.clone(),
        channel: "cli".into(),
        low_precision: cfg.policy.low_precision,
        approvals: ApprovalMode::parse(&cfg.features.approvals).unwrap_or(ApprovalMode::Ask),
    });
    let open = rpc(
        1,
        "session.open",
        json!({
            "session": session_id,
            "workspace": workspace.display().to_string(),
            "mode": mode.as_str(),
        }),
    );
    match session.handle(&open) {
        Dispatch::Result { .. } => {}
        Dispatch::Error(err) => anyhow::bail!("{}", err.message),
        other => anyhow::bail!("session.open: {other:?}"),
    }
    let slash = rpc(2, "slash", json!({"text": prompt}));
    match session.handle(&slash) {
        Dispatch::Result { result, .. } => {
            print_slash_result(&result);
            Ok(Some(SlashOutcome::Exit(ExitCode::SUCCESS)))
        }
        Dispatch::Error(err) => anyhow::bail!("{}", err.message),
        Dispatch::TurnStart { prompt, .. } => {
            let snap = session.snapshot();
            Ok(Some(SlashOutcome::Turn {
                prompt,
                session_id: snap.session_id,
                policy: snap.policy,
                effort_locked: snap.effort_locked,
                mode: snap.mode,
                model: snap.model,
            }))
        }
        Dispatch::Abort | Dispatch::AbortClear { .. } => {
            println!("stopped");
            Ok(Some(SlashOutcome::Exit(ExitCode::SUCCESS)))
        }
    }
}

fn rpc(id: u32, method: &str, params: Value) -> RpcRequest {
    RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: method.into(),
        params: Some(params),
    }
}

fn print_slash_result(result: &Value) {
    if let Some(text) = result.get("text").and_then(|v| v.as_str()) {
        println!("{text}");
        return;
    }
    if result.get("session").is_some() {
        print!("session={}", result["session"].as_str().unwrap_or(""));
        if let Some(mode) = result.get("mode").and_then(|v| v.as_str()) {
            print!(" mode={mode}");
        }
        if let Some(title) = result.get("title").and_then(|v| v.as_str()) {
            if !title.is_empty() {
                print!(" title={title}");
            }
        }
        println!();
        return;
    }
    println!("{result}");
}

fn think_policy(cfg: &Config, fast: bool, think: Option<&str>, mode: Option<&str>) -> ThinkPolicy {
    let p = ThinkPolicy::from_cli(&cfg.policy.think_budget(), fast, think, mode);
    if p.effort == Some(Effort::Xhigh) {
        eprintln!("{XHIGH_WARN}");
    }
    p
}

fn print_human(report: &q38_loop::ProbeReport, path: &std::path::Path) {
    println!("q-harness probe");
    println!("  family     {}", report.family);
    println!("  profile    {}", report.profile);
    println!("  model      {}", report.model);
    println!("  quant      {}", report.quant_label);
    println!(
        "  thinking   per-request off: {}",
        if report.enable_thinking { "yes" } else { "NO" }
    );
    println!("  effort     {:?}", report.effort_values);
    println!("  preserve   {:?}", report.preserve_thinking);
    println!("  mtp        {:?}", report.mtp);
    println!("  cached     {}", report.cached_tokens_field);
    println!("  prefill    {:?} tok/s", report.prefill_tok_s);
    println!("  decode     {:?} tok/s", report.decode_tok_s);
    println!(
        "  think tok  off={:?} low={:?}",
        report.think_tokens_off, report.think_tokens_low
    );
    println!(
        "  xt-cache   off={:?} on={:?}",
        report.cross_turn_off_hit_pct, report.cross_turn_on_hit_pct
    );
    println!(
        "  media      image={:?} video={:?} audio={:?} transcribe={:?}",
        report.supports_image,
        report.supports_video,
        report.supports_audio,
        report.supports_transcription
    );
    for n in &report.notes {
        println!("  note       {n}");
    }
    for y in &report.yellow {
        println!("  yellow     {y}");
    }
    for r in &report.red {
        println!("  RED        {r}");
    }
    println!("wrote {}", path.display());
}

#[cfg(test)]
mod clap_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn dsh_install_is_not_swallowed_as_a_prompt() {
        let cli = Cli::try_parse_from(["q38", "dsh-install", "--dry-run"]).expect("parse");
        match cli.command {
            Some(Command::DshInstall {
                dry_run,
                profile,
                skip_dsh,
            }) => {
                assert!(dry_run);
                assert!(!skip_dsh);
                assert_eq!(profile, "q38");
            }
            other => panic!("expected DshInstall, got {other:?}"),
        }
        assert!(cli.prompt.is_empty());
    }

    #[test]
    fn web_is_not_swallowed_as_a_prompt() {
        let cli = Cli::try_parse_from(["q38", "web", "--no-open", "--bind", "127.0.0.1:0"])
            .expect("parse");
        match cli.command {
            Some(Command::Web { bind, no_open }) => {
                assert!(no_open);
                assert_eq!(bind, "127.0.0.1:0");
            }
            other => panic!("expected Web, got {other:?}"),
        }
        assert!(cli.prompt.is_empty());
    }
}
