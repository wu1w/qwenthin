//! Shared param decode and session-start surface for sidecar RPC.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::channel::ContentPart;
use crate::config::Config;
use crate::mcp::McpRegistry;
use crate::media::MediaPart;
use crate::policy::ThinkPolicy;
use crate::prompt::{coding_prompt, periphery_section, session_prompt};
use crate::session::{tools_hash, SessionMode, SessionStart, SlashCmd};
use crate::skills::SkillCatalog;
use crate::tools_schema::{
    agent_tools, code_tools, mcp_tool, memory_search_tool, search_tool, view_tool, web_only_tools,
};

use super::types::{PolicyCaps, RpcError};

#[derive(Debug, Deserialize)]
pub(crate) struct SessionOpenParams {
    #[serde(default)]
    pub(crate) session: Option<String>,
    #[serde(default)]
    pub(crate) workspace: Option<String>,
    #[serde(default)]
    pub(crate) mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TurnStartParams {
    #[serde(default)]
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) content_parts: Vec<ContentPart>,
}

impl TurnStartParams {
    pub(crate) fn prompt(&self) -> String {
        if !self.prompt.is_empty() {
            self.prompt.clone()
        } else {
            self.text.clone()
        }
    }

    pub(crate) fn parts(&self) -> Vec<MediaPart> {
        self.content_parts
            .iter()
            .filter_map(ContentPart::media_part)
            .collect()
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SessionSearchParams {
    #[serde(default)]
    pub(crate) search: Option<String>,
    #[serde(default)]
    pub(crate) session: Option<String>,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) prompt: Option<String>,
    #[serde(default)]
    pub(crate) channel: Option<String>,
    #[serde(default)]
    pub(crate) sessions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SlashParams {
    pub(crate) text: String,
}

pub(crate) fn parse_params<T: for<'de> Deserialize<'de>>(
    params: Option<&Value>,
) -> std::result::Result<T, RpcError> {
    serde_json::from_value(params.cloned().unwrap_or_else(|| json!({})))
        .map_err(|e| RpcError::invalid_params(e.to_string()))
}

pub(crate) fn make_start(
    id: &str,
    workspace: &std::path::Path,
    mode: SessionMode,
    policy: ThinkPolicy,
    channel: &str,
    home: Option<&std::path::Path>,
) -> SessionStart {
    let ws = workspace.display().to_string();
    let home_buf = home
        .map(|p| p.to_path_buf())
        .or_else(|| Config::home_dir().ok());
    let home = home_buf.as_deref();
    let (system, tools) = match mode {
        SessionMode::Chat => (coding_prompt(&ws), web_only_tools()),
        SessionMode::Code => (coding_prompt(&ws), code_tools()),
        SessionMode::Agent | SessionMode::Think => sidecar_agent_surface(&ws, workspace, home),
    };
    let mut start = SessionStart::new(
        id,
        ws,
        mode,
        system,
        tools_hash(&tools),
        policy.with_preserved_thinking(),
    );
    if !channel.is_empty() {
        start.channel = channel.to_string();
    }
    start
}

pub(crate) fn sidecar_agent_surface(
    _display: &str,
    workspace: &std::path::Path,
    home: Option<&std::path::Path>,
) -> (String, Vec<serde_json::Value>) {
    let mut tools = agent_tools();
    tools.push(search_tool());
    let memory_ok = home.map(|h| h.is_dir()).unwrap_or(false);
    if memory_ok {
        tools.push(memory_search_tool());
    }
    let cfg = isolated_or_disk_config(home);
    let mcp = McpRegistry::load(home, workspace, &cfg.mcp);
    if !mcp.servers.is_empty() {
        tools.push(mcp_tool());
    }
    if cfg.media.enabled {
        tools.push(view_tool());
    }
    let skills_md = if cfg.features.skills_auto_catalog {
        SkillCatalog::load(home.unwrap_or_else(|| std::path::Path::new("")), workspace)
            .catalog_markdown()
    } else {
        String::new()
    };
    let mcp_md = if cfg.features.mcp_auto_catalog {
        mcp.catalog_markdown()
    } else {
        String::new()
    };
    let system = {
        let mut s = session_prompt(workspace, home, crate::prompt::AGENT_MD_NAME, false);
        s.push_str(&periphery_section(&skills_md, &mcp_md));
        s.push('\n');
        s.push_str(crate::cron::CRON_SYSTEM_LINE);
        s.push('\n');
        s
    };
    (system, tools)
}

/// Isolated `home` (SaaS tenant dir) must not inherit the host `~/.q38-agent/config.toml`.
fn isolated_or_disk_config(home: Option<&std::path::Path>) -> Config {
    let Some(home) = home else {
        return Config::load_file_or_default();
    };
    if Config::home_dir()
        .ok()
        .as_deref()
        .is_some_and(|d| d == home)
    {
        return Config::load_file_or_default();
    }
    let path = home.join("config.toml");
    if path.is_file() {
        Config::load_from(&path).unwrap_or_default()
    } else {
        Config::default()
    }
}

pub(crate) fn slash_policy(cmd: &SlashCmd, caps: &PolicyCaps) -> ThinkPolicy {
    let b = caps.think_budget();
    match cmd {
        SlashCmd::Off => ThinkPolicy::off_with(&b),
        SlashCmd::Think(effort) => ThinkPolicy::effort_with(&b, *effort),
        SlashCmd::Mode(_) => unreachable!("mode slash forks before policy"),
        _ => unreachable!("slash_policy only for think/off"),
    }
}
