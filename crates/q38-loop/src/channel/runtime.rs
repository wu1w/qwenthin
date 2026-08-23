//! Start enabled channel adapters (webhook, telegram, QQ, …).

use crate::error::{Error, Result};

use super::manager::{ChannelHandler, ChannelManager};
use super::router::SessionRouter;
use super::ChannelsConfig;

pub async fn run<H>(cfg: ChannelsConfig, handler: H) -> Result<()>
where
    H: ChannelHandler,
{
    let enabled: Vec<_> = cfg
        .endpoints
        .iter()
        .filter(|e| e.enabled)
        .cloned()
        .collect();
    if enabled.is_empty() {
        return Err(Error::msg(
            "no enabled [[channels.endpoints]] — add telegram, webhook, or qq in ~/.q38-agent/config.toml",
        ));
    }
    let router = SessionRouter::in_home()?;
    let mgr = ChannelManager::start(cfg.clone(), router, handler);
    let mut joins = Vec::new();
    for ep in enabled {
        let kind = ep.kind.to_ascii_lowercase();
        let mgr = mgr.clone();
        match kind.as_str() {
            "telegram" => {
                joins.push(tokio::spawn(async move {
                    if let Err(e) = super::telegram::run_long_poll(ep, mgr).await {
                        eprintln!("q38 telegram: {e}");
                    }
                }));
            }
            "webhook" | "http" | "console" => {
                joins.push(tokio::spawn(async move {
                    if let Err(e) = super::webhook::serve(ep, mgr).await {
                        eprintln!("q38 webhook: {e}");
                    }
                }));
            }
            "qq" => {
                joins.push(tokio::spawn(async move {
                    if let Err(e) = super::qq::run_gateway(ep, mgr).await {
                        eprintln!("q38 qq: {e}");
                    }
                }));
            }
            "wechat" => {
                joins.push(tokio::spawn(async move {
                    if let Err(e) = super::wechat::run_poll(ep, mgr).await {
                        eprintln!("q38 wechat: {e}");
                    }
                }));
            }
            "wecom" => {
                joins.push(tokio::spawn(async move {
                    if let Err(e) = super::wecom::run_gateway(ep, mgr).await {
                        eprintln!("q38 wecom: {e}");
                    }
                }));
            }
            "dingtalk" => {
                joins.push(tokio::spawn(async move {
                    if let Err(e) = super::dingtalk::run_gateway(ep, mgr).await {
                        eprintln!("q38 dingtalk: {e}");
                    }
                }));
            }
            "feishu" => {
                joins.push(tokio::spawn(async move {
                    if let Err(e) = super::feishu::run_ws(ep, mgr).await {
                        eprintln!("q38 feishu: {e}");
                    }
                }));
            }
            other => {
                eprintln!(
                    "q38 channel {other}: no in-process client; POST QwenPaw native JSON to a webhook endpoint"
                );
            }
        }
    }
    if joins.is_empty() {
        return Err(Error::msg(
            "no runnable channel (enable telegram, webhook, or qq)",
        ));
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("q38 channels: stopping");
        }
        _ = async {
            for j in joins {
                let _ = j.await;
            }
        } => {}
    }
    Ok(())
}
