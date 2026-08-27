//! Start enabled channel adapters (webhook, telegram, QQ, …).

use crate::error::{Error, Result};

use super::inbound::keep_client_watched;
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
                let id = ep.id.clone();
                joins.push(tokio::spawn(async move {
                    keep_client_watched(
                        "telegram",
                        &id,
                        || super::telegram::run_long_poll(ep.clone(), mgr.clone()),
                        |_| async {},
                    )
                    .await;
                }));
            }
            "webhook" | "http" | "console" => {
                let id = ep.id.clone();
                let label = kind.clone();
                joins.push(tokio::spawn(async move {
                    keep_client_watched(
                        &label,
                        &id,
                        || super::webhook::serve(ep.clone(), mgr.clone()),
                        |_| async {},
                    )
                    .await;
                }));
            }
            "qq" => {
                let id = ep.id.clone();
                joins.push(tokio::spawn(async move {
                    keep_client_watched(
                        "qq",
                        &id,
                        || super::qq::run_gateway(ep.clone(), mgr.clone()),
                        |_| async {},
                    )
                    .await;
                }));
            }
            "wechat" => {
                let id = ep.id.clone();
                joins.push(tokio::spawn(async move {
                    keep_client_watched(
                        "wechat",
                        &id,
                        || super::wechat::run_poll(ep.clone(), mgr.clone()),
                        |_| async {},
                    )
                    .await;
                }));
            }
            "wecom" => {
                let id = ep.id.clone();
                joins.push(tokio::spawn(async move {
                    keep_client_watched(
                        "wecom",
                        &id,
                        || super::wecom::run_gateway(ep.clone(), mgr.clone()),
                        |_| async {},
                    )
                    .await;
                }));
            }
            "dingtalk" => {
                let id = ep.id.clone();
                joins.push(tokio::spawn(async move {
                    keep_client_watched(
                        "dingtalk",
                        &id,
                        || super::dingtalk::run_gateway(ep.clone(), mgr.clone()),
                        |_| async {},
                    )
                    .await;
                }));
            }
            "feishu" => {
                let id = ep.id.clone();
                joins.push(tokio::spawn(async move {
                    keep_client_watched(
                        "feishu",
                        &id,
                        || super::feishu::run_ws(ep.clone(), mgr.clone()),
                        |_| async {},
                    )
                    .await;
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
