//! Three-task quality gate for q-harness.
//!
//! Default: validate fixtures and print the checklist (no model).
//! `--live`: run each task with q38's default adaptive thinking policy (or the
//! q38-loop Agent if the `q38` binary is missing).
//!
//! Wall-clock calibration (design Targets; numbers from probe.json when present):
//!   overhead_s      = 2.5
//!   ttft_cold_gate  = measured_prefix / measured_prefill + overhead_s
//!   ttft_hot_gate   = typical_suffix / measured_prefill + overhead_s
//!   wall_4step_off  = 4 * (hot + 200 / decode) + cold
//!   wall_4step_low  = 4 * (hot + (512 + 200) / decode) + cold
//! Native coding window is 262144. Do not compact.
//! quant_label is read from probe.json (not locked to UD-Q8).

mod tasks;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use q38_loop::config::Config;
use q38_loop::policy::ThinkPolicy;
use q38_loop::{
    Agent, HttpCompleter, ProbeReport, RunOpts, ToolSet, CODING_CTX_TOKENS, CODING_SYSTEM_PROMPT,
};
use serde::{Deserialize, Serialize};
use tasks::{
    copy_task, dummy_fat_prefix, evaluate_live, validate_fixtures, Task, FAT_PREFIX_TARGET_TOKENS,
    TASKS,
};

const AFTER_HELP: &str = "\
Wall-clock calibration (design Targets; numbers from ~/.q38-agent/probe.json when present):
  overhead_s      = 2.5
  ttft_cold_gate  = measured_prefix / measured_prefill + overhead_s
  ttft_hot_gate   = typical_suffix / measured_prefill + overhead_s
  wall_4step_off  = 4 * (hot + 200 / decode) + cold
  wall_4step_low  = 4 * (hot + (512 + 200) / decode) + cold
Native coding window is 262144. Do not compact. Do not lock UD-Q8; read quant_label from probe.json.
";

#[derive(Parser, Debug)]
#[command(
    name = "q38-bench",
    about = "Three-task quality gate for q-harness (family / profile / quant_label)",
    after_help = AFTER_HELP
)]
struct Cli {
    /// Run tasks against the configured endpoint with adaptive thinking.
    #[arg(long)]
    live: bool,

    /// Parent directory for live fixture copies (default: std temp dir).
    #[arg(long, value_name = "DIR")]
    workspace_parent: Option<PathBuf>,

    /// Time one run with a ~12k dummy system vs the lean coding prompt (needs --live).
    #[arg(long)]
    fat_prefix: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BenchReport {
    family: String,
    profile: String,
    quant_label: String,
    model: String,
    tasks: Vec<TaskResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fat_prefix: Option<FatPrefixReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TaskResult {
    name: String,
    ok: bool,
    wall_ms: u64,
    steps: u32,
    stop_reason: Option<String>,
    notes: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FatPrefixReport {
    lean_wall_ms: u64,
    fat_wall_ms: u64,
    fat_tokens: u32,
    lean_prompt_chars: usize,
    notes: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("q38-bench: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    if cli.fat_prefix && !cli.live {
        bail!("--fat-prefix requires --live");
    }

    let lines = validate_fixtures().context("fixture validators")?;
    let probe = load_probe();
    print_offline(&lines, probe.as_ref());

    if !cli.live {
        return Ok(ExitCode::SUCCESS);
    }

    let parent = cli.workspace_parent.unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&parent)
        .with_context(|| format!("workspace-parent {}", parent.display()))?;

    let report = run_live(&parent, cli.fat_prefix, probe).await?;
    let path = write_report(&report)?;
    println!();
    println!("report {}", path.display());
    for t in &report.tasks {
        let mark = if t.ok { "ok" } else { "FAIL" };
        println!(
            "  {mark}  {}  wall_ms={} steps={} stop={:?}  {}",
            t.name, t.wall_ms, t.steps, t.stop_reason, t.notes
        );
    }
    if let Some(fp) = &report.fat_prefix {
        println!(
            "  fat-prefix  lean={}ms fat={}ms tokens={}  {}",
            fp.lean_wall_ms, fp.fat_wall_ms, fp.fat_tokens, fp.notes
        );
    }
    let all_ok = report.tasks.iter().all(|t| t.ok);
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn print_offline(lines: &[String], probe: Option<&ProbeReport>) {
    println!("q38-bench quality gate (offline)");
    println!("Fixtures:");
    for line in lines {
        println!("  {line}");
    }
    println!();
    println!("Checklist:");
    println!("  - native context {CODING_CTX_TOKENS} (do not compact)");
    println!("  - report family / profile / quant_label from probe.json (not locked to UD-Q8)");
    println!("  - three tasks: file diff / content (03 optionally cargo test on --live)");
    println!("  - fat Hermes contrast is --fat-prefix (same endpoint, same 262k window)");
    println!("  - live uses default adaptive thinking and official Qwen3.8 sampling");
    println!();
    print_calibration(probe);
}

fn print_calibration(probe: Option<&ProbeReport>) {
    const OVERHEAD: f64 = 2.5;
    const PREFIX: f64 = 1200.0;
    const SUFFIX: f64 = 200.0;
    println!("Wall-clock calibration (design Targets):");
    println!("  overhead_s      = {OVERHEAD}");
    println!("  ttft_cold_gate  = measured_prefix / measured_prefill + overhead_s");
    println!("  ttft_hot_gate   = typical_suffix / measured_prefill + overhead_s");
    println!("  wall_4step_off  = 4 * (hot + 200 / decode) + cold");
    println!("  wall_4step_low  = 4 * (hot + (512 + 200) / decode) + cold");
    match probe {
        Some(p) => {
            println!(
                "  probe           family={} profile={} quant_label={} model={}",
                p.family, p.profile, p.quant_label, p.model
            );
            match (p.prefill_tok_s, p.decode_tok_s) {
                (Some(prefill), Some(decode)) if prefill > 0.0 && decode > 0.0 => {
                    let cold = PREFIX / prefill + OVERHEAD;
                    let hot = SUFFIX / prefill + OVERHEAD;
                    let off = 4.0 * (hot + 200.0 / decode) + cold;
                    let low = 4.0 * (hot + (512.0 + 200.0) / decode) + cold;
                    println!("  prefill         {prefill:.1} tok/s");
                    println!("  decode          {decode:.1} tok/s");
                    println!("  ttft_cold_gate  {cold:.1}s   (prefix={PREFIX})");
                    println!("  ttft_hot_gate   {hot:.1}s   (suffix={SUFFIX})");
                    println!("  wall_4step_off  {off:.1}s");
                    println!("  wall_4step_low  {low:.1}s");
                }
                _ => println!("  probe has no prefill/decode; formulas only"),
            }
        }
        None => match Config::probe_path() {
            Ok(p) => println!("  no probe.json at {} — run `q38 probe`", p.display()),
            Err(_) => println!("  no probe.json (HOME unset?)"),
        },
    }
}

fn load_probe() -> Option<ProbeReport> {
    let path = Config::probe_path().ok()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn identity(probe: Option<&ProbeReport>, model: &str) -> (String, String, String, String) {
    match probe {
        Some(p) => {
            let model = if p.model.is_empty() {
                model.to_string()
            } else {
                p.model.clone()
            };
            (
                p.family.clone(),
                p.profile.clone(),
                p.quant_label.clone(),
                model,
            )
        }
        None => (
            "unknown".into(),
            "unknown".into(),
            "unknown".into(),
            model.to_string(),
        ),
    }
}

fn report_path() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() || fs::create_dir_all(&p).is_ok() {
            return p.join("q38-bench-report.json");
        }
    }
    let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
    if ws.is_dir() || fs::create_dir_all(&ws).is_ok() {
        return ws.join("q38-bench-report.json");
    }
    PathBuf::from("q38-bench-report.json")
}

fn write_report(report: &BenchReport) -> Result<PathBuf> {
    let path = report_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(report)?;
    fs::write(&path, json)?;
    Ok(path)
}

async fn run_live(
    parent: &Path,
    fat_prefix: bool,
    probe: Option<ProbeReport>,
) -> Result<BenchReport> {
    let (cfg, _) = Config::load_or_init().context("load ~/.q38-agent/config.toml")?;
    let stamp = unix_stamp();
    let mut tasks = Vec::new();
    let mut model = String::new();

    for task in TASKS {
        let dest = parent.join(format!("q38-bench-{stamp}-{}", task.name));
        copy_task(task, &dest)?;
        eprintln!("task {} workspace {}", task.name, dest.display());
        let result = run_one_task(&cfg, task, &dest, false).await?;
        if model.is_empty() {
            if let Some(rest) = result.notes.split("model=").nth(1) {
                model = rest.split(';').next().unwrap_or("").trim().to_string();
            }
        }
        tasks.push(result);
    }

    let fat = if fat_prefix {
        Some(run_fat_contrast(&cfg, parent, stamp, probe.as_ref()).await?)
    } else {
        None
    };

    let (family, profile, quant_label, model) = identity(probe.as_ref(), &model);
    Ok(BenchReport {
        family,
        profile,
        quant_label,
        model,
        tasks,
        fat_prefix: fat,
    })
}

async fn run_one_task(
    cfg: &Config,
    task: &Task,
    workspace: &Path,
    fat_agents_md: bool,
) -> Result<TaskResult> {
    if fat_agents_md {
        return run_via_agent(cfg, task, workspace, true).await;
    }
    match find_q38_bin() {
        Some(bin) => run_via_q38(&bin, cfg, task, workspace).await,
        None => run_via_agent(cfg, task, workspace, false).await,
    }
}

fn find_q38_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("Q38_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("q38");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
    for profile in ["debug", "release"] {
        let cand = target.join(profile).join("q38");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

async fn run_via_q38(
    bin: &Path,
    cfg: &Config,
    task: &Task,
    workspace: &Path,
) -> Result<TaskResult> {
    let max_wall = Duration::from_secs(cfg.policy.max_wall_seconds.max(30));
    let t0 = Instant::now();
    let mut cmd = tokio::process::Command::new(bin);
    cmd.kill_on_drop(true)
        .arg("--print")
        .arg("--new")
        .arg("--no-agents-md")
        .arg("--workspace")
        .arg(workspace)
        .arg(task.prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let timed = tokio::time::timeout(max_wall, child.wait_with_output()).await;
    let wall_ms = t0.elapsed().as_millis() as u64;
    let (steps, stop_reason, spawn_notes) = match timed {
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprint!("{stderr}");
            let (steps, reason) = parse_spawn_meta(&stderr);
            let model = stderr
                .lines()
                .find_map(|l| l.strip_prefix("model: "))
                .unwrap_or("");
            let status = out.status;
            let notes = if status.success() {
                format!("runner=q38-bin {} model={model}", bin.display())
            } else {
                format!("runner=q38-bin exit={status} model={model}")
            };
            (steps, reason, notes)
        }
        Ok(Err(e)) => {
            return Ok(eval_after(
                task,
                workspace,
                wall_ms,
                0,
                Some("spawn-failed".into()),
                format!("runner=q38-bin error={e}"),
            ));
        }
        Err(_) => {
            return Ok(eval_after(
                task,
                workspace,
                wall_ms,
                0,
                Some("timeout".into()),
                "runner=q38-bin timed out".into(),
            ));
        }
    };
    Ok(eval_after(
        task,
        workspace,
        wall_ms,
        steps,
        stop_reason,
        spawn_notes,
    ))
}

fn parse_spawn_meta(stderr: &str) -> (u32, Option<String>) {
    let mut tools = 0u32;
    let mut reason = None;
    for line in stderr.lines() {
        if line.starts_with("[read]")
            || line.starts_with("[write]")
            || line.starts_with("[edit]")
            || line.starts_with("[bash]")
            || line.starts_with("[run_code]")
        {
            tools += 1;
        }
        if line.contains("budget:context") || line.contains("max iterations") {
            reason = Some(line.trim().to_string());
        }
    }
    (tools.saturating_add(1), reason)
}

fn eval_after(
    task: &Task,
    workspace: &Path,
    wall_ms: u64,
    steps: u32,
    stop_reason: Option<String>,
    runner_notes: String,
) -> TaskResult {
    let ev = evaluate_live(task, workspace);
    let notes = if ev.notes.is_empty() {
        runner_notes
    } else {
        format!("{}; {}", runner_notes, ev.notes)
    };
    TaskResult {
        name: task.name.into(),
        ok: ev.ok,
        wall_ms,
        steps,
        stop_reason,
        notes,
    }
}

async fn run_via_agent(
    cfg: &Config,
    task: &Task,
    workspace: &Path,
    fat_agents_md: bool,
) -> Result<TaskResult> {
    let policy = ThinkPolicy::native_with(&cfg.policy.think_budget());
    let completer = HttpCompleter::connect(cfg, policy.clone())
        .await
        .context("connect")?;
    let model = completer.model().to_string();
    let mut opts = RunOpts::from_config(cfg, workspace.to_path_buf());
    opts.print = true;
    opts.with_tools = true;
    opts.tool_set = ToolSet::Agent;
    opts.agents_md = fat_agents_md;
    opts.agents_md_max_tokens = if fat_agents_md { 24_000 } else { 0 };
    opts.working_window = CODING_CTX_TOKENS;
    opts.generation_reserve = policy.max_tokens.saturating_add(policy.max_think_tokens);
    opts.max_steps = cfg.policy.max_steps.min(24);
    opts.effort_locked = false;
    opts.session_id = format!("bench-{}-{}", task.name, unix_stamp());

    let t0 = Instant::now();
    let mut agent = Agent::new(completer, opts).context("agent")?;
    let out = match agent.run(task.prompt).await {
        Ok(o) => o,
        Err(e) => {
            let wall_ms = t0.elapsed().as_millis() as u64;
            let ev = evaluate_live(task, workspace);
            return Ok(TaskResult {
                name: task.name.into(),
                ok: ev.ok,
                wall_ms,
                steps: 0,
                stop_reason: Some("agent-error".into()),
                notes: format!("runner=agent model={model}; {e:#}; {}", ev.notes),
            });
        }
    };
    let wall_ms = t0.elapsed().as_millis() as u64;
    let ev = evaluate_live(task, workspace);
    Ok(TaskResult {
        name: task.name.into(),
        ok: ev.ok,
        wall_ms,
        steps: out.steps,
        stop_reason: out.stop_reason,
        notes: format!("runner=agent model={model}; {}", ev.notes),
    })
}

async fn run_fat_contrast(
    cfg: &Config,
    parent: &Path,
    stamp: u64,
    probe: Option<&ProbeReport>,
) -> Result<FatPrefixReport> {
    let task = &TASKS[0];
    let (filler, fat_tokens) = dummy_fat_prefix(FAT_PREFIX_TARGET_TOKENS)?;

    let lean_dir = parent.join(format!("q38-bench-{stamp}-fat-lean"));
    copy_task(task, &lean_dir)?;
    let lean = run_via_agent(cfg, task, &lean_dir, false).await?;

    let fat_dir = parent.join(format!("q38-bench-{stamp}-fat-dummy"));
    copy_task(task, &fat_dir)?;
    fs::write(fat_dir.join("AGENTS.md"), &filler)?;
    let fat = run_via_agent(cfg, task, &fat_dir, true).await?;

    let mut notes = format!(
        "same endpoint, window={CODING_CTX_TOKENS}; dummy filler via qwen38 tokenizer (not Hermes)"
    );
    if let Some(p) = probe {
        if let Some(prefill) = p.prefill_tok_s {
            if prefill > 0.0 {
                notes.push_str(&format!(
                    "; expected extra prefill ~{:.1}s",
                    f64::from(fat_tokens) / prefill
                ));
            }
        }
    }
    notes.push_str(&format!("; lean_ok={} fat_ok={}", lean.ok, fat.ok));

    Ok(FatPrefixReport {
        lean_wall_ms: lean.wall_ms,
        fat_wall_ms: fat.wall_ms,
        fat_tokens,
        lean_prompt_chars: CODING_SYSTEM_PROMPT.len(),
        notes,
    })
}

fn unix_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> BenchReport {
        BenchReport {
            family: "qwen38".into(),
            profile: "llamacpp".into(),
            quant_label: "ud-q8".into(),
            model: "Qwen3.8-27B".into(),
            tasks: vec![TaskResult {
                name: "01-read-edit".into(),
                ok: true,
                wall_ms: 1200,
                steps: 3,
                stop_reason: None,
                notes: "ok".into(),
            }],
            fat_prefix: Some(FatPrefixReport {
                lean_wall_ms: 1000,
                fat_wall_ms: 13000,
                fat_tokens: 12000,
                lean_prompt_chars: 200,
                notes: "dummy".into(),
            }),
        }
    }

    #[test]
    fn report_json_shape() {
        let v = serde_json::to_value(sample_report()).unwrap();
        for key in ["family", "profile", "quant_label", "model", "tasks"] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["family"], "qwen38");
        assert_eq!(v["quant_label"], "ud-q8");
        let t = &v["tasks"][0];
        for key in ["name", "ok", "wall_ms", "steps", "stop_reason", "notes"] {
            assert!(t.get(key).is_some(), "task missing {key}");
        }
        assert!(t["stop_reason"].is_null());
        let fp = &v["fat_prefix"];
        assert!(fp.get("lean_wall_ms").is_some());
        assert!(fp.get("fat_wall_ms").is_some());
        assert!(fp.get("fat_tokens").is_some());
    }

    #[test]
    fn report_omits_fat_prefix_when_none() {
        let mut r = sample_report();
        r.fat_prefix = None;
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("fat_prefix").is_none());
        let back: BenchReport = serde_json::from_value(v).unwrap();
        assert!(back.fat_prefix.is_none());
        assert_eq!(back.tasks.len(), 1);
    }

    #[test]
    fn coding_window_is_262k() {
        assert_eq!(CODING_CTX_TOKENS, 262_144);
        assert!(!CODING_SYSTEM_PROMPT.is_empty());
    }

    #[test]
    fn identity_does_not_invent_ud_q8() {
        let (fam, prof, quant, model) = identity(None, "foo");
        assert_eq!(fam, "unknown");
        assert_eq!(prof, "unknown");
        assert_eq!(quant, "unknown");
        assert_eq!(model, "foo");
        assert_ne!(quant, "ud-q8");
    }

    #[test]
    fn probe_load_is_offline() {
        let _ = load_probe();
    }

    #[tokio::test]
    #[ignore = "live OpenAI-compat endpoint"]
    async fn live_quality_gate() {
        let tmp = std::env::temp_dir().join(format!("q38-bench-live-test-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let report = run_live(&tmp, false, load_probe())
            .await
            .expect("live bench");
        assert!(report.tasks.iter().all(|t| t.ok), "{:#?}", report.tasks);
        assert_eq!(report.tasks.len(), 3);
    }
}
