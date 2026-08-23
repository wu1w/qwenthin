//! Run one in-process endpoint (used by `q38 --channels` and `q38 web`).

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{Agent, HttpCompleter, RunOpts, ToolSet};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::session::SessionMode;

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::outbound::reply_text;
use super::router::SessionRouter;
use super::ChannelEndpoint;

/// Start the live client for one enabled endpoint.
pub async fn serve_endpoint(cfg: Config, workspace: PathBuf, ep: ChannelEndpoint) -> Result<()> {
    let kind = ep.kind.to_ascii_lowercase();
    let router = SessionRouter::in_home()?;
    let cfg = Arc::new(cfg);
    let cfg_h = cfg.clone();
    let workspace = workspace.clone();
    let mgr = ChannelManager::start(cfg.channels.clone(), router, move |env: NativePayload| {
        let cfg = cfg_h.clone();
        let workspace = workspace.clone();
        async move { agent_inbound(&cfg, workspace, env).await }
    });
    match kind.as_str() {
        "telegram" => super::telegram::run_long_poll(ep, mgr).await,
        "webhook" | "http" | "console" => super::webhook::serve(ep, mgr).await,
        "qq" => super::qq::run_gateway(ep, mgr).await,
        "wechat" => super::wechat::run_poll(ep, mgr).await,
        "wecom" => super::wecom::run_gateway(ep, mgr).await,
        "dingtalk" => super::dingtalk::run_gateway(ep, mgr).await,
        "feishu" => super::feishu::run_ws(ep, mgr).await,
        other => Err(Error::msg(format!(
            "q38 channel {other}: no in-process client"
        ))),
    }
}

/// Back-compat for the QQ-only hub watcher.
pub async fn serve_qq(cfg: Config, workspace: PathBuf, ep: ChannelEndpoint) -> Result<()> {
    serve_endpoint(cfg, workspace, ep).await
}

async fn agent_inbound(
    cfg: &Config,
    workspace: PathBuf,
    env: NativePayload,
) -> Result<Vec<ContentPart>> {
    let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
    let mut opts = RunOpts::from_config(cfg, workspace);
    opts.print = false;
    opts.persist_session = true;
    opts.session_id = if env.session_id.is_empty() {
        if env.channel.is_empty() {
            "channel".into()
        } else {
            env.channel.clone()
        }
    } else {
        env.session_id.clone()
    };
    opts.session_mode = SessionMode::Agent;
    opts.with_tools = true;
    opts.tool_set = ToolSet::Agent;
    opts.channel = if env.channel.trim().is_empty() {
        "im".into()
    } else {
        env.channel.clone()
    };
    let completer = HttpCompleter::connect(cfg, policy).await?;
    let mut agent = Agent::new(completer, opts)?;
    let out = agent.run_message(env.to_chat_message()).await?;
    if out.text.trim().is_empty() {
        Ok(Vec::new())
    } else {
        Ok(reply_text(out.text))
    }
}
