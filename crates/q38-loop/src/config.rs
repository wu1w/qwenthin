use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::family::{EngineProfile, Family};
use crate::mcp::McpConfig;

/// Coding-session context baseline. Matches Qwen3.8 native 262k; do not compact down to 16k.
pub const CODING_CTX_TOKENS: u32 = 262_144;
const WORKING_WINDOW_MIN: u32 = 1_024;
const WORKING_WINDOW_MAX: u32 = 2_097_152;
const MAX_TOKENS_MIN: u32 = 256;
const MAX_TOKENS_MAX: u32 = 131_072;

pub fn parse_working_window(n: u32) -> Result<u32> {
    if n < WORKING_WINDOW_MIN || n > WORKING_WINDOW_MAX {
        return Err(Error::msg(format!(
            "working_window must be {WORKING_WINDOW_MIN}..={WORKING_WINDOW_MAX}, got {n}"
        )));
    }
    Ok(n)
}

pub fn parse_max_tokens(n: u32) -> Result<u32> {
    if n < MAX_TOKENS_MIN || n > MAX_TOKENS_MAX {
        return Err(Error::msg(format!(
            "max_tokens must be {MAX_TOKENS_MIN}..={MAX_TOKENS_MAX}, got {n}"
        )));
    }
    Ok(n)
}

/// Runtime-only: `Q38_WORKING_WINDOW` replaced the on-disk value.
/// Never serialized — `mutate_disk` / `save_to` must not persist it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkingWindowOverlay {
    pub from_file: u32,
    pub from_env: u32,
}

/// Apply a `Q38_WORKING_WINDOW` value on top of the file's window.
/// Invalid / empty env leaves the file value. Equal values are a no-op notice.
pub fn apply_working_window_overlay(
    file: u32,
    env_raw: Option<&str>,
) -> (u32, Option<WorkingWindowOverlay>) {
    let Some(raw) = env_raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return (file, None);
    };
    let Ok(n) = raw.parse::<u32>() else {
        return (file, None);
    };
    let Ok(n) = parse_working_window(n) else {
        return (file, None);
    };
    if n == file {
        return (n, None);
    }
    (
        n,
        Some(WorkingWindowOverlay {
            from_file: file,
            from_env: n,
        }),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub context: ContextConfig,
    pub policy: PolicyConfig,
    pub prompt: PromptConfig,
    pub features: FeatureConfig,
    pub sidecar: SidecarConfig,
    pub tools: ToolsConfig,
    pub code_mode: CodeModeConfig,
    pub mcp: McpConfig,
    pub media: MediaConfig,
    pub web: WebConfig,
    pub channels: crate::channel::ChannelsConfig,
    /// Last folder chosen in the console Files page. `q38 web` uses this
    /// when `--workspace` is omitted.
    #[serde(default, skip_serializing_if = "ConsoleConfig::is_unset")]
    pub console: ConsoleConfig,
    /// Set by [`Config::apply_env`]. Not written to `config.toml`.
    #[serde(skip)]
    pub working_window_overlay: Option<WorkingWindowOverlay>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            context: ContextConfig::default(),
            policy: PolicyConfig::default(),
            prompt: PromptConfig::default(),
            features: FeatureConfig::default(),
            sidecar: SidecarConfig::default(),
            tools: ToolsConfig::default(),
            code_mode: CodeModeConfig::default(),
            mcp: McpConfig::default(),
            media: MediaConfig::default(),
            web: WebConfig::default(),
            channels: crate::channel::ChannelsConfig::default(),
            console: ConsoleConfig::default(),
            working_window_overlay: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub profile: EngineProfile,
    pub family: Family,
    pub connect_timeout_s: u64,
    pub read_timeout_s: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080/v1".into(),
            api_key: "local".into(),
            model: String::new(),
            profile: EngineProfile::Auto,
            family: Family::Auto,
            connect_timeout_s: 5,
            read_timeout_s: 1800,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub working_window: u32,
    pub hard_cap: u32,
    /// Soft compact threshold: prefix+reserve > window * ratio.
    pub compact_ratio: f64,
    pub agents_md_max_tokens: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            working_window: CODING_CTX_TOKENS,
            hard_cap: CODING_CTX_TOKENS,
            compact_ratio: 0.70,
            agents_md_max_tokens: 400,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub default_mode: String,
    pub default_effort: String,
    pub max_think_tokens_low: u32,
    pub max_think_tokens_medium: u32,
    pub max_think_tokens_xhigh: u32,
    pub max_steps: u32,
    pub max_steps_think: u32,
    pub max_wall_seconds: u64,
    pub max_tokens: u32,
    pub think_mode_max_tokens: u32,
    /// User switch: tighter doom/parse/repeat guards. Off = Q8 / high-precision defaults.
    pub low_precision: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default_mode: "agent".into(),
            // `auto` maps to neutral official `medium`: no depth instruction.
            default_effort: "auto".into(),
            max_think_tokens_low: 512,
            max_think_tokens_medium: 2048,
            max_think_tokens_xhigh: 4096,
            max_steps: 80,
            max_steps_think: 100,
            max_wall_seconds: 1800,
            max_tokens: 8192,
            think_mode_max_tokens: 16384,
            low_precision: false,
        }
    }
}

impl PolicyConfig {
    pub fn think_budget(&self) -> crate::policy::ThinkBudget {
        use crate::policy::{Effort, ThinkBudget};
        let default_effort = match Effort::from_config(&self.default_effort) {
            Some(Effort::Xhigh) | None => Effort::Medium,
            Some(e) => e,
        };
        ThinkBudget {
            max_tokens: self.max_tokens,
            think_mode_max_tokens: self.think_mode_max_tokens,
            max_think_low: self.max_think_tokens_low,
            max_think_medium: self.max_think_tokens_medium,
            max_think_xhigh: self.max_think_tokens_xhigh,
            default_effort,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Builtin identity is 编程助手 when no AGENT.md exists.
    pub coding: bool,
    /// Looked up in the workspace, then `~/.q38-agent`. Empty = `AGENT.md`.
    pub file: String,
    /// One-line progress narration on interactive channels (TUI/web).
    /// `--print` and IM bridges are never narrated regardless of this flag.
    pub narrate: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            coding: false,
            file: crate::prompt::AGENT_MD_NAME.into(),
            narrate: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureConfig {
    pub code_mode: bool,
    pub tui: bool,
    pub skills_auto_catalog: bool,
    pub mcp_auto_catalog: bool,
    pub workspace_write_only: bool,
    /// TUI permission mode: ask | auto | yolo. `--print` never prompts.
    pub approvals: String,
}

impl Default for FeatureConfig {
    fn default() -> Self {
        Self {
            code_mode: true,
            tui: false,
            skills_auto_catalog: false,
            mcp_auto_catalog: false,
            workspace_write_only: true,
            approvals: "ask".into(),
        }
    }
}

/// `web` tool. Works with zero config: keyless engines answer out of the box;
/// a Tavily key (here, env `TAVILY_API_KEY`, or an existing tavily MCP mount)
/// upgrades the same tool to Tavily REST with builtin fallback.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub enabled: bool,
    /// auto | builtin | tavily. auto = tavily when a key is found.
    pub provider: String,
    pub tavily_api_key: String,
    /// Keyless engines, tried in order until one returns results.
    pub engines: Vec<String>,
    pub timeout_s: u64,
    pub max_results: usize,
    pub fetch_max_bytes: usize,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "auto".into(),
            tavily_api_key: String::new(),
            engines: vec!["bing".into(), "duckduckgo".into()],
            timeout_s: 12,
            max_results: 5,
            fetch_max_bytes: 2_000_000,
        }
    }
}

/// Console Files-page workspace. Separate from `[web]` (the search tool).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ConsoleConfig {
    /// Absolute folder. Empty = `q38 web` falls back to the process cwd.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub workspace: String,
}

impl ConsoleConfig {
    fn is_unset(&self) -> bool {
        self.workspace.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SidecarConfig {
    pub transport: String,
    pub socket_path: String,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            transport: "stdio-jsonrpc".into(),
            socket_path: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub read_default_lines: u32,
    pub result_max_chars: u32,
    pub result_head_chars: u32,
    pub result_tail_chars: u32,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            // 600 lines ≈ one comfortable page. 2000-line pages hit the 27B
            // with 30-60k-char prefill spikes on big files; offset/limit paging
            // costs one extra hop only when the file is genuinely long.
            read_default_lines: 600,
            result_max_chars: 10_000,
            result_head_chars: 8_000,
            result_tail_chars: 2_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeModeConfig {
    pub timeout_s: u64,
    pub inherit_env: bool,
}

impl Default for CodeModeConfig {
    fn default() -> Self {
        Self {
            timeout_s: 60,
            inherit_env: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub enabled: bool,
    pub max_bytes: u64,
    /// Absolute path to `ffmpeg`, or a directory containing it. Empty = PATH / `Q38_FFMPEG`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ffmpeg: String,
    /// Absolute path to `whisper-cli`, or a directory containing it. Empty = PATH / `Q38_WHISPER`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub whisper: String,
    /// ggml Whisper weights (`.bin`). Empty = `Q38_WHISPER_MODEL` or `~/.q38-agent/whisper/ggml-tiny.bin`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub whisper_model: String,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: crate::media::MAX_INLINE_MEDIA_BYTES as u64,
            ffmpeg: String::new(),
            whisper: String::new(),
            whisper_model: String::new(),
        }
    }
}

fn overlay_nonempty(key: &str, dest: &mut String) {
    if let Ok(v) = std::env::var(key) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            *dest = v;
        }
    }
}

/// User home on macOS / Linux (`HOME`) and Windows (`USERPROFILE`, then `HOMEDRIVE`+`HOMEPATH`).
pub fn user_home() -> Option<PathBuf> {
    fn nonempty(key: &str) -> Option<PathBuf> {
        std::env::var_os(key).and_then(|v| {
            if v.is_empty() {
                None
            } else {
                Some(PathBuf::from(v))
            }
        })
    }
    nonempty("HOME")
        .or_else(|| nonempty("USERPROFILE"))
        .or_else(|| {
            let drive = std::env::var("HOMEDRIVE").ok()?;
            let path = std::env::var("HOMEPATH").ok()?;
            if drive.is_empty() || path.is_empty() {
                None
            } else {
                Some(PathBuf::from(format!("{drive}{path}")))
            }
        })
}

impl Config {
    pub fn home_dir() -> Result<PathBuf> {
        let home = user_home()
            .ok_or_else(|| Error::Config("home directory is unset (HOME / USERPROFILE)".into()))?;
        Ok(home.join(".q38-agent"))
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(Self::home_dir()?.join("config.toml"))
    }

    pub fn probe_path() -> Result<PathBuf> {
        Ok(Self::home_dir()?.join("probe.json"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    /// Read `~/.q38-agent/config.toml` if it exists. Never creates directories
    /// or writes files (unlike [`load_or_init`]).
    pub fn load_file_or_default() -> Self {
        Self::default_path()
            .ok()
            .filter(|p| p.is_file())
            .and_then(|p| Self::load_from(&p).ok())
            .unwrap_or_default()
    }

    /// Load `~/.q38-agent/config.toml`, creating it from defaults if missing.
    /// Does not apply `Q38_*` — use [`load_or_init`] for CLI/tests that want the overlay.
    pub fn load_or_init_file() -> Result<(Self, PathBuf)> {
        let dir = Self::home_dir()?;
        fs::create_dir_all(&dir)?;
        let path = dir.join("config.toml");
        let cfg = if !path.exists() {
            let cfg = Self::default();
            cfg.save_to(&path)?;
            cfg
        } else {
            Self::load_from(&path)?
        };
        let agent_md = dir.join(crate::prompt::AGENT_MD_NAME);
        if !agent_md.exists() {
            let body = crate::prompt::builtin_role_boundary(cfg.prompt.coding);
            let _ = fs::write(&agent_md, body);
        }
        Ok((cfg, path))
    }

    /// Load `~/.q38-agent/config.toml`, creating it from defaults if missing.
    /// `Q38_BASE_URL` / `Q38_API_KEY` / `Q38_MODEL` overlay the file (never commit those).
    /// The overlay is runtime-only; [`mutate_disk`] will not write it back.
    pub fn load_or_init() -> Result<(Self, PathBuf)> {
        let (mut cfg, path) = Self::load_or_init_file()?;
        cfg.apply_env();
        Ok((cfg, path))
    }

    pub fn apply_env(&mut self) {
        if let Ok(u) = std::env::var("Q38_BASE_URL") {
            let u = u.trim().to_string();
            if !u.is_empty() {
                self.server.base_url = u;
            }
        }
        if let Ok(k) = std::env::var("Q38_API_KEY") {
            let k = k.trim().to_string();
            if !k.is_empty() {
                self.server.api_key = k;
            }
        }
        if let Ok(m) = std::env::var("Q38_MODEL") {
            let m = m.trim().to_string();
            if !m.is_empty() {
                self.server.model = m;
            }
        }
        overlay_nonempty("Q38_FFMPEG", &mut self.media.ffmpeg);
        overlay_nonempty("Q38_WHISPER", &mut self.media.whisper);
        overlay_nonempty("Q38_WHISPER_MODEL", &mut self.media.whisper_model);
        let env_w = std::env::var("Q38_WORKING_WINDOW").ok();
        let (window, overlay) =
            apply_working_window_overlay(self.context.working_window, env_w.as_deref());
        self.context.working_window = window;
        self.working_window_overlay = overlay;
    }

    pub fn env_overlay_set() -> serde_json::Value {
        serde_json::json!({
            "base_url": env_nonempty("Q38_BASE_URL"),
            "api_key": env_nonempty("Q38_API_KEY"),
            "model": env_nonempty("Q38_MODEL"),
            "working_window": env_nonempty("Q38_WORKING_WINDOW"),
        })
    }

    /// Patch the on-disk file. Never starts from an env-overlaid clone, so
    /// `Q38_*` cannot replace `[server]` / `working_window` on save.
    pub fn mutate_disk(path: &Path, f: impl FnOnce(&mut Self)) -> Result<Self> {
        let mut disk = if path.exists() {
            Self::load_from(path)?
        } else {
            Self::default()
        };
        backup_commented_original(path);
        f(&mut disk);
        disk.save_to(path)?;
        Ok(disk)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        if let Err(e) = fs::write(&tmp, self.to_toml()) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        match replace_tmp(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|_| String::new())
    }
}

/// `save_to` 是 serde 全量重写，会抹掉手写注释。首次改写一个「含注释且
/// 尚无备份」的 config.toml 前，把原文件留一份 config.toml.orig；已存在
/// 则不覆盖。失败只静默放弃，不阻塞保存。
fn backup_commented_original(path: &Path) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    if !raw.lines().any(|l| l.trim_start().starts_with('#')) {
        return;
    }
    let orig = path.with_extension("toml.orig");
    if orig.exists() {
        return;
    }
    let _ = fs::write(&orig, raw);
}

/// Replace `dest` with `tmp`. Never unlinks `dest` to make room for the rename.
fn replace_tmp(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    match fs::rename(tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            #[cfg(windows)]
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                return replace_existing_windows(tmp, dest);
            }
            Err(e)
        }
    }
}

/// Park `dest` at `*.toml.bak`, move `tmp` into place, then drop the backup.
/// If the second rename fails, restore `bak` → `dest`.
#[cfg(windows)]
fn replace_existing_windows(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    let bak = dest.with_extension("toml.bak");
    fs::rename(dest, &bak)?;
    match fs::rename(tmp, dest) {
        Ok(()) => {
            let _ = fs::remove_file(&bak);
            Ok(())
        }
        Err(e) => {
            let _ = fs::rename(&bak, dest);
            Err(e)
        }
    }
}

fn env_nonempty(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .is_some_and(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_baseline_is_262k() {
        let c = Config::default();
        assert_eq!(c.context.working_window, CODING_CTX_TOKENS);
        assert_eq!(c.context.hard_cap, CODING_CTX_TOKENS);
        assert_eq!(c.policy.max_steps, 80);
        assert!(!c.policy.low_precision);
        assert_eq!(c.policy.max_wall_seconds, 1800);
        assert_eq!(c.server.read_timeout_s, 1800);
        assert_eq!(c.tools.read_default_lines, 600);
        assert_eq!(c.tools.result_max_chars, 10_000);
        assert_eq!(c.tools.result_head_chars, 8_000);
        assert_eq!(c.tools.result_tail_chars, 2_000);
        assert_eq!(c.context.agents_md_max_tokens, 400);
    }

    #[test]
    fn working_window_bounds() {
        assert!(parse_working_window(0).is_err());
        assert!(parse_working_window(1023).is_err());
        assert_eq!(parse_working_window(8000).unwrap(), 8000);
        assert_eq!(
            parse_working_window(CODING_CTX_TOKENS).unwrap(),
            CODING_CTX_TOKENS
        );
        assert!(parse_working_window(WORKING_WINDOW_MAX + 1).is_err());
        assert_eq!(parse_max_tokens(8192).unwrap(), 8192);
        assert!(parse_max_tokens(1).is_err());
    }

    #[test]
    fn overlay_records_shrink_and_ignores_same_or_garbage() {
        let file = CODING_CTX_TOKENS;
        let (n, o) = apply_working_window_overlay(file, Some("8000"));
        assert_eq!(n, 8000);
        assert_eq!(
            o,
            Some(WorkingWindowOverlay {
                from_file: file,
                from_env: 8000,
            })
        );
        let (n, o) = apply_working_window_overlay(file, Some("262144"));
        assert_eq!(n, file);
        assert!(o.is_none());
        let (n, o) = apply_working_window_overlay(file, Some("nope"));
        assert_eq!(n, file);
        assert!(o.is_none());
        let (n, o) = apply_working_window_overlay(file, Some("12"));
        assert_eq!(n, file);
        assert!(o.is_none());
        let (n, o) = apply_working_window_overlay(file, None);
        assert_eq!(n, file);
        assert!(o.is_none());
    }

    #[test]
    fn overlay_is_not_written_to_toml() {
        let mut c = Config::default();
        c.working_window_overlay = Some(WorkingWindowOverlay {
            from_file: CODING_CTX_TOKENS,
            from_env: 8000,
        });
        let toml = c.to_toml();
        assert!(
            !toml.contains("working_window_overlay") && !toml.contains("from_env"),
            "{toml}"
        );
        assert!(
            !toml.contains("[console]"),
            "empty console workspace should stay off disk: {toml}"
        );
    }

    #[test]
    fn user_home_is_set_on_this_host() {
        assert!(user_home().is_some());
        assert!(Config::home_dir().unwrap().ends_with(".q38-agent"));
    }

    #[test]
    fn mutate_disk_does_not_write_runtime_overlay() {
        let dir = std::env::temp_dir().join(format!("q38-cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut saved = Config::default();
        saved.server.base_url = "https://saved.example/v1".into();
        saved.server.model = "keep-me".into();
        saved.server.api_key = "file-key".into();
        saved.context.working_window = CODING_CTX_TOKENS;
        saved.save_to(&path).unwrap();

        let mut runtime = Config::load_from(&path).unwrap();
        runtime.server.base_url = "http://127.0.0.1:9/v1".into();
        runtime.context.working_window = 8000;
        runtime.save_to(&path).unwrap();
        // Restore file as the user copy, then patch via mutate_disk from a
        // dirty runtime clone would be the old bug. mutate_disk reloads.
        saved.save_to(&path).unwrap();
        Config::mutate_disk(&path, |c| {
            c.features.mcp_auto_catalog = true;
        })
        .unwrap();
        let again = Config::load_from(&path).unwrap();
        assert_eq!(again.server.base_url, "https://saved.example/v1");
        assert_eq!(again.server.model, "keep-me");
        assert_eq!(again.server.api_key, "file-key");
        assert_eq!(again.context.working_window, CODING_CTX_TOKENS);
        assert!(again.features.mcp_auto_catalog);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mutate_disk_backs_up_commented_original_once() {
        let dir = std::env::temp_dir().join(format!("q38-cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let orig = path.with_extension("toml.orig");
        // 手写配置：带注释。首次 mutate_disk 前必须留底。
        let hand_written = "# my precious notes\n[server]\nmodel = \"hand\"\n";
        std::fs::write(&path, hand_written).unwrap();

        Config::mutate_disk(&path, |c| c.features.mcp_auto_catalog = true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&orig).unwrap(),
            hand_written,
            "commented original must be preserved as config.toml.orig"
        );

        // 二次改写不得覆盖已有备份。
        Config::mutate_disk(&path, |c| c.features.mcp_auto_catalog = false).unwrap();
        assert_eq!(std::fs::read_to_string(&orig).unwrap(), hand_written);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mutate_disk_skips_backup_without_comments() {
        let dir = std::env::temp_dir().join(format!("q38-cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        Config::default().save_to(&path).unwrap();
        Config::mutate_disk(&path, |c| c.features.mcp_auto_catalog = true).unwrap();
        assert!(
            !path.with_extension("toml.orig").exists(),
            "machine-written file (no comments) needs no backup"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_to_roundtrip() {
        let dir = std::env::temp_dir().join(format!("q38-cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.server.model = "roundtrip-model".into();
        cfg.context.working_window = 8000;
        cfg.save_to(&path).unwrap();
        assert!(path.is_file());
        assert!(!path.with_extension("toml.tmp").exists());
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.server.model, "roundtrip-model");
        assert_eq!(loaded.context.working_window, 8000);
        assert_eq!(loaded.context.compact_ratio, cfg.context.compact_ratio);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_to_replaces_existing_dest() {
        let dir = std::env::temp_dir().join(format!("q38-cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut first = Config::default();
        first.server.model = "v1".into();
        first.save_to(&path).unwrap();
        let mut second = Config::default();
        second.server.model = "v2".into();
        second.save_to(&path).unwrap();
        assert!(path.is_file());
        assert!(!path.with_extension("toml.tmp").exists());
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.server.model, "v2");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_tmp_keeps_dest_when_rename_cannot_complete() {
        let dir = std::env::temp_dir().join(format!("q38-cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("config.toml");
        fs::write(&dest, "keep-me").unwrap();
        let tmp = dest.with_extension("toml.tmp");
        let err = replace_tmp(&tmp, &dest).unwrap_err();
        assert!(
            dest.is_file(),
            "dest must survive a failed replace, err={err}"
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "keep-me");
        assert!(!tmp.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_to_drops_tmp_when_rename_fails() {
        let dir = std::env::temp_dir().join(format!("q38-cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("config.toml");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("occupant"), "x").unwrap();
        let err = Config::default().save_to(&dest).unwrap_err();
        assert!(dest.exists(), "dest must remain, err={err}");
        assert!(!dest.with_extension("toml.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
