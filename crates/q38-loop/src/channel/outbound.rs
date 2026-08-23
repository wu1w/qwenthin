//! Send QwenPaw `content_parts` back to the originating chat.

use serde_json::{json, Value};

use crate::error::Result;

use super::envelope::{ContentPart, NativePayload};
use super::ChannelEndpoint;

pub fn reply_text(body: impl Into<String>) -> Vec<ContentPart> {
    let body = body.into();
    if body.trim().is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(body)]
    }
}

pub fn parts_to_text(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for p in parts {
        if let Some(t) = p.as_text() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        } else if let Some(line) = p.fallback_line() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&line);
        }
    }
    text
}

pub async fn deliver(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    if parts.is_empty() {
        return Ok(());
    }
    let kind = ep.map(|e| e.kind.as_str()).unwrap_or(env.channel.as_str());
    match kind {
        "telegram" => super::telegram::send(ep, env, parts).await,
        "qq" => super::qq::send(ep, env, parts).await,
        "wechat" => super::wechat::send(ep, env, parts).await,
        "wecom" => super::wecom::send(ep, env, parts).await,
        "dingtalk" => super::dingtalk::send(ep, env, parts).await,
        "feishu" => super::feishu::send(ep, env, parts).await,
        _ => post_webhook(ep, env, parts).await,
    }
}

async fn post_webhook(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let url = env
        .reply_url()
        .or_else(|| ep.and_then(|e| nonempty(&e.reply_url).map(|s| s.to_string())))
        .or_else(|| ep.and_then(|e| e.extra.get("reply_url").cloned()));
    let Some(url) = url else {
        return Ok(());
    };
    let body = json!({
        "channel": env.channel,
        "sender_id": env.sender_id,
        "session_id": env.session_id,
        "to_handle": env.chat_id(),
        "content_parts": parts,
        "text": parts_to_text(parts),
        "meta": env.meta,
    });
    let client = reqwest::Client::new();
    let mut req = client.post(&url).json(&body);
    if let Some(secret) = ep.and_then(|e| nonempty(&e.secret).map(|s| s.to_string())) {
        req = req.header("X-Q38-Token", secret);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(crate::error::Error::msg(format!(
            "channel outbound {} {}",
            resp.status(),
            url
        )));
    }
    let _ = resp;
    Ok(())
}

pub fn outbound_notification(env: &NativePayload, parts: &[ContentPart]) -> Value {
    json!({
        "channel": env.channel,
        "sender_id": env.sender_id,
        "session_id": env.session_id,
        "to_handle": env.chat_id(),
        "content_parts": parts,
        "text": parts_to_text(parts),
        "meta": env.meta,
    })
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}
