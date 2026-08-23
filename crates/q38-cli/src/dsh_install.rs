//! Optional: dsh CLI + this plugin + q38 binary. Product shell is `q38 web`.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{bail, Context, Result};
use q38_loop::config::Config;

const PROFILE: &str = "q38";
const PLUGIN_PACKAGE: &str = "dsh-plugin-q38";

pub struct DshInstallOpts {
    pub profile: String,
    pub skip_dsh: bool,
    pub dry_run: bool,
}

impl Default for DshInstallOpts {
    fn default() -> Self {
        Self {
            profile: PROFILE.into(),
            skip_dsh: false,
            dry_run: false,
        }
    }
}

pub fn run(opts: DshInstallOpts) -> Result<ExitCode> {
    let plugin_src = find_plugin_dir().context("find dsh-plugin-q38")?;
    let q38_bin = resolve_q38_bin(opts.dry_run).context("resolve q38 binary")?;
    let dest = Config::home_dir()?.join(PLUGIN_PACKAGE);
    let dsh_home = dsh_home_dir();
    let profile_dir = dsh_home.join("profiles").join(&opts.profile);

    println!("plugin source  {}", plugin_src.display());
    println!("install copy   {}", dest.display());
    println!("q38 binary     {}", q38_bin.display());
    println!(
        "dsh profile    {} ({})",
        opts.profile,
        profile_dir.display()
    );

    if opts.dry_run {
        println!("dry-run: no installs, no copies");
        return Ok(ExitCode::SUCCESS);
    }

    copy_dir(&plugin_src, &dest).context("copy plugin into ~/.q38-agent")?;
    build_plugin(&dest).context("build plugin (tsc)")?;
    ensure_runtime(opts.skip_dsh).context("install dsh runtime")?;
    add_profile_plugin(&opts.profile, &dest).context("dsh plugin add")?;
    write_command_overlay(&profile_dir, &q38_bin).context("write profile overlay")?;

    println!();
    println!("ok. this plugin is optional — product shell is `q38 web`.");
    println!("  q38 web");
    println!("dsh (if you still want it):");
    println!("  dsh --profile {}", opts.profile);
    println!("  dsh web --profile {}", opts.profile);
    println!("models/keys still live in ~/.q38-agent/config.toml");
    Ok(ExitCode::SUCCESS)
}

fn dsh_home_dir() -> PathBuf {
    env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Config::home_dir()
                .ok()
                .and_then(|p| p.parent().map(|h| h.join(".dsh")))
                .unwrap_or_else(|| PathBuf::from(".dsh"))
        })
}

fn looks_like_plugin(dir: &Path) -> bool {
    dir.join("cordis.patch.yml").is_file() && dir.join("package.json").is_file()
}

fn find_plugin_dir() -> Result<PathBuf> {
    if let Ok(explicit) = env::var("Q38_DSH_PLUGIN") {
        let p = PathBuf::from(explicit);
        if looks_like_plugin(&p) {
            return Ok(p);
        }
        bail!("Q38_DSH_PLUGIN is not a plugin dir: {}", p.display());
    }
    let mut candidates = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        candidates.extend(walk_parents(&cwd));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.extend(walk_parents(dir));
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/dsh-plugin-q38"));
    if let Ok(home) = Config::home_dir() {
        candidates.push(home.join(PLUGIN_PACKAGE));
    }
    for root in candidates {
        let plugin = if looks_like_plugin(&root) {
            root
        } else {
            root.join("plugins").join(PLUGIN_PACKAGE)
        };
        if looks_like_plugin(&plugin) {
            return Ok(plugin.canonicalize().unwrap_or(plugin));
        }
    }
    bail!("cannot find plugins/dsh-plugin-q38 (run from the q-harness checkout, or set Q38_DSH_PLUGIN)");
}

fn walk_parents(start: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(dir) = cur {
        out.push(dir.to_path_buf());
        cur = dir.parent();
        if out.len() > 12 {
            break;
        }
    }
    out
}

fn resolve_q38_bin(dry_run: bool) -> Result<PathBuf> {
    if let Ok(explicit) = env::var("Q38_BIN") {
        return Ok(PathBuf::from(explicit));
    }
    if let Ok(exe) = env::current_exe() {
        let name = exe.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == "q38" || name.starts_with("q38-") {
            return Ok(exe);
        }
    }
    if let Some(path) = which("q38") {
        return Ok(path);
    }
    let repo = find_repo_root();
    if dry_run {
        if let Some(root) = repo {
            return Ok(root.join("target/release/q38"));
        }
        bail!("q38 not on PATH");
    }
    let Some(root) = repo else {
        bail!("q38 not on PATH and no checkout Cargo.toml found; cargo install -p q38-cli first");
    };
    eprintln!("building q38 (cargo build -p q38-cli --release)…");
    let status = Command::new("cargo")
        .args(["build", "-p", "q38-cli", "--release"])
        .current_dir(&root)
        .status()
        .context("cargo build")?;
    if !status.success() {
        bail!("cargo build -p q38-cli failed");
    }
    let bin = root.join("target/release/q38");
    if !bin.is_file() {
        bail!("build succeeded but {} is missing", bin.display());
    }
    Ok(bin)
}

fn find_repo_root() -> Option<PathBuf> {
    let mut cur = env::current_dir().ok();
    while let Some(dir) = cur {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file() && dir.join("plugins").join(PLUGIN_PACKAGE).is_dir() {
            return Some(dir);
        }
        cur = dir.parent().map(Path::to_path_buf);
    }
    let baked = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if baked.join("plugins").join(PLUGIN_PACKAGE).is_dir() {
        return Some(baked.canonicalize().unwrap_or(baked));
    }
    None
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    if src.canonicalize().ok() == dest.canonicalize().ok() {
        return Ok(());
    }
    if dest.exists() {
        fs::remove_dir_all(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    copy_tree(src, dest)
}

fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "node_modules" || name == "target" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(name);
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn build_plugin(dir: &Path) -> Result<()> {
    let dist = dir.join("dist/plugin.js");
    if dist.is_file() {
        return Ok(());
    }
    if which("node").is_none() {
        bail!("node is required to build the dsh plugin (https://nodejs.org)");
    }
    let npm = if which("npm").is_some() {
        "npm"
    } else {
        bail!("npm is required to build the dsh plugin");
    };
    eprintln!("npm install in {}…", dir.display());
    run_cmd(Command::new(npm).args(["install"]).current_dir(dir))?;
    eprintln!("tsc…");
    let tsc = dir.join("node_modules/.bin/tsc");
    if tsc.is_file() {
        run_cmd(Command::new(&tsc).current_dir(dir))?;
    } else {
        run_cmd(Command::new("npx").args(["tsc"]).current_dir(dir))?;
    }
    if !dist.is_file() {
        bail!("plugin build produced no {}", dist.display());
    }
    Ok(())
}

fn ensure_runtime(skip_dsh: bool) -> Result<()> {
    if which("node").is_none() {
        bail!("node >= 18 is required (dsh and the plugin are Node). Install Node, then re-run.");
    }
    if which("pnpm").is_none() {
        eprintln!("pnpm missing; trying corepack enable pnpm…");
        let _ = Command::new("corepack").args(["enable", "pnpm"]).status();
        if which("pnpm").is_none() && which("npm").is_some() {
            eprintln!("pnpm still missing; npm install -g pnpm…");
            run_cmd(Command::new("npm").args(["install", "-g", "pnpm"]))?;
        }
        if which("pnpm").is_none() {
            bail!("pnpm is required (`dsh plugin` forwards to pnpm). Install pnpm and re-run.");
        }
    }
    if skip_dsh {
        return Ok(());
    }
    if which("dsh").is_some() {
        return Ok(());
    }
    if which("npm").is_none() {
        bail!("dsh CLI not found and npm is missing; install https://github.com/deepseek-ai/deepseek-harness");
    }
    eprintln!("dsh missing; npm install -g @deepseek-ai/dsh …");
    let status = Command::new("npm")
        .args(["install", "-g", "@deepseek-ai/dsh"])
        .status()
        .context("npm install -g @deepseek-ai/dsh")?;
    if !status.success() {
        eprintln!("retrying @deepseek-ai/dsh@next …");
        run_cmd(Command::new("npm").args(["install", "-g", "@deepseek-ai/dsh@next"]))?;
    }
    if which("dsh").is_none() {
        bail!("dsh installed but not on PATH; open a new shell or add npm's global bin");
    }
    Ok(())
}

fn add_profile_plugin(profile: &str, plugin: &Path) -> Result<()> {
    eprintln!("dsh plugin --profile {profile} add {}…", plugin.display());
    run_cmd(
        Command::new("dsh")
            .args([
                "plugin",
                "--profile",
                profile,
                "add",
                &plugin.display().to_string(),
            ])
            .current_dir(plugin),
    )
}

fn write_command_overlay(profile_dir: &Path, q38_bin: &Path) -> Result<()> {
    fs::create_dir_all(profile_dir)?;
    let path = profile_dir.join("cordis.patch.yml");
    let overlay = format!(
        "# managed by q38 dsh-install — q38 binary for this machine\n\
         - id: q38-loop\n\
           config:\n\
             command: {}\n",
        yaml_string(&q38_bin.display().to_string()),
    );
    if path.is_file() {
        let existing = fs::read_to_string(&path)?;
        if existing.contains("id: q38-loop") && existing.contains("command:") {
            let rewritten = replace_command(&existing, &q38_bin.display().to_string());
            fs::write(&path, rewritten)?;
            return Ok(());
        }
        let mut f = fs::OpenOptions::new().append(true).open(&path)?;
        if !existing.ends_with('\n') {
            f.write_all(b"\n")?;
        }
        f.write_all(overlay.as_bytes())?;
        return Ok(());
    }
    fs::write(&path, overlay)?;
    Ok(())
}

fn replace_command(yaml: &str, command: &str) -> String {
    let quoted = yaml_string(command);
    let mut out = String::new();
    let mut in_q38 = false;
    for line in yaml.lines() {
        if line.contains("id: q38-loop") {
            in_q38 = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_q38 && line.trim_start().starts_with("command:") {
            let indent = line
                .split_once("command:")
                .map(|(i, _)| i)
                .unwrap_or("             ");
            out.push_str(indent);
            out.push_str("command: ");
            out.push_str(&quoted);
            out.push('\n');
            in_q38 = false;
            continue;
        }
        if in_q38 && line.starts_with("- id:") {
            in_q38 = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn yaml_string(s: &str) -> String {
    if s.chars().any(|c| matches!(c, ':' | '#' | '"' | '\'' | ' ')) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let output = Command::new("sh")
        .args(["-lc", &format!("command -v {name}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawn {cmd:?}"))?;
    if !status.success() {
        bail!("command failed: {cmd:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_quotes_paths_with_spaces() {
        assert_eq!(yaml_string("/tmp/q38"), "/tmp/q38");
        assert_eq!(yaml_string("/tmp/my bin/q38"), "\"/tmp/my bin/q38\"");
    }

    #[test]
    fn replace_keeps_other_rows() {
        let src = "- id: other\n  config: {}\n- id: q38-loop\n  config:\n    command: old\n";
        let out = replace_command(src, "/opt/q38");
        assert!(out.contains("id: other"));
        assert!(out.contains("command: /opt/q38"));
        assert!(!out.contains("command: old"));
    }

    #[test]
    fn plugin_dir_helper_matches_checkout() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/dsh-plugin-q38");
        assert!(looks_like_plugin(&root), "{}", root.display());
    }
}
