//! Run one in-process endpoint (used by `q38 --channels` and `q38 web`).

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;

use crate::agent::{apply_unattended_policy, Agent, HttpCompleter, RunOpts, ToolSet};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::session::SessionMode;

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::outbound::reply_text;
use super::router::SessionRouter;
use super::ChannelEndpoint;

const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 120;

/// Watcher / console status while [`keep_client_watched`] is between attempts.
#[derive(Clone, Debug)]
pub enum ClientWatch {
    Running,
    Retry { detail: String, wait_secs: u64 },
    Fatal { detail: String },
}

pub fn supervise_backoff_secs(fail: u32) -> u64 {
    let exp = fail.saturating_sub(1).min(8);
    BACKOFF_MIN_SECS
        .saturating_mul(1u64 << exp)
        .min(BACKOFF_MAX_SECS)
}

pub fn is_fatal_serve_error(err: &str) -> bool {
    err.contains("no in-process client")
}

pub async fn catch_client<F>(fut: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(_) => Err(Error::msg("channel client panicked")),
    }
}

/// Restart a live client after unexpected return / error / panic.
/// Fingerprint changes abort the task; this loop then stops.
pub async fn keep_client_watched<F, Fut, W, WFut>(kind: &str, id: &str, mut once: F, mut watch: W)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
    W: FnMut(ClientWatch) -> WFut,
    WFut: Future<Output = ()>,
{
    let mut fail = 0u32;
    loop {
        match super::poll_lock::acquire(kind, id) {
            Ok(_lock) => {
                watch(ClientWatch::Running).await;
                match catch_client(once()).await {
                    Ok(()) => {
                        fail = fail.saturating_add(1);
                        let wait = supervise_backoff_secs(fail);
                        let detail = "client exited".to_string();
                        eprintln!("q38 {kind} ({id}): {detail}; retry in {wait}s");
                        watch(ClientWatch::Retry {
                            detail,
                            wait_secs: wait,
                        })
                        .await;
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                    }
                    Err(e) => {
                        let s = e.to_string();
                        if is_fatal_serve_error(&s) {
                            eprintln!("q38 {kind}: {s}");
                            watch(ClientWatch::Fatal { detail: s }).await;
                            return;
                        }
                        fail = fail.saturating_add(1);
                        let wait = supervise_backoff_secs(fail);
                        eprintln!("q38 {kind} ({id}): {s}; retry in {wait}s");
                        watch(ClientWatch::Retry {
                            detail: s,
                            wait_secs: wait,
                        })
                        .await;
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                    }
                }
            }
            Err(e) => {
                fail = fail.saturating_add(1);
                let wait = supervise_backoff_secs(fail);
                let s = e.to_string();
                eprintln!("q38 {kind} ({id}): {s}; retry in {wait}s");
                watch(ClientWatch::Retry {
                    detail: s,
                    wait_secs: wait,
                })
                .await;
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }
    }
}

/// Start the live client for one enabled endpoint (returns if the adapter exits).
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
    apply_unattended_policy(&mut opts, cfg);
    let completer = HttpCompleter::connect(cfg, policy).await?;
    let mut agent = tokio::task::spawn_blocking(move || Agent::new(completer, opts))
        .await
        .map_err(|e| Error::msg(format!("agent setup: {e}")))??;
    let out = agent.run_message(env.to_chat_message()).await?;
    if out.text.trim().is_empty() {
        Ok(Vec::new())
    } else {
        Ok(reply_text(out.text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps() {
        assert_eq!(supervise_backoff_secs(1), 5);
        assert_eq!(supervise_backoff_secs(2), 10);
        assert_eq!(supervise_backoff_secs(3), 20);
        assert_eq!(supervise_backoff_secs(20), BACKOFF_MAX_SECS);
    }

    #[test]
    fn fatal_unknown_kind() {
        assert!(is_fatal_serve_error(
            "q38 channel discord: no in-process client"
        ));
        assert!(!is_fatal_serve_error("wechat HTTP 502"));
    }
}
