//! QQ official Bot gateway. Wash of QwenPaw `qq/channel.py` WS + HTTP send.
//!
//! Phone「连接中」clears only after IDENTIFY → READY on this socket.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const API_PROD: &str = "https://api.sgroup.qq.com";
const API_SANDBOX: &str = "https://sandbox.api.sgroup.qq.com";

const OP_DISPATCH: i64 = 0;
const OP_HEARTBEAT: i64 = 1;
const OP_IDENTIFY: i64 = 2;
const OP_RECONNECT: i64 = 7;
const OP_INVALID_SESSION: i64 = 9;
const OP_HELLO: i64 = 10;

const INTENT_GUILD_MEMBERS: u64 = 1 << 1;
const INTENT_DIRECT_MESSAGE: u64 = 1 << 12;
const INTENT_GROUP_AND_C2C: u64 = 1 << 25;
const INTENT_INTERACTION: u64 = 1 << 26;
const INTENT_PUBLIC_GUILD_MESSAGES: u64 = 1 << 30;

static MSG_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn credentials(ep: &ChannelEndpoint) -> Option<(String, String)> {
    let app_id = ep
        .extra
        .get("app_id")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let secret = ep
        .extra
        .get("client_secret")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    if app_id.is_empty() || secret.is_empty() {
        None
    } else {
        Some((app_id, secret))
    }
}

fn api_bases(ep: &ChannelEndpoint) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(b) = ep.extra.get("api_base").map(|s| s.trim().to_string()) {
        if !b.is_empty() {
            out.push(b.trim_end_matches('/').to_string());
        }
    }
    if let Ok(b) = std::env::var("QQ_API_BASE") {
        let b = b.trim().trim_end_matches('/').to_string();
        if !b.is_empty() && !out.iter().any(|x| x == &b) {
            out.push(b);
        }
    }
    for b in [API_PROD, API_SANDBOX] {
        if !out.iter().any(|x| x == b) {
            out.push(b.to_string());
        }
    }
    out
}

async fn access_token(http: &reqwest::Client, app_id: &str, secret: &str) -> Result<String> {
    let url = std::env::var("QQ_TOKEN_URL").unwrap_or_else(|_| TOKEN_URL.into());
    let data: Value = http
        .post(url)
        .json(&json!({"appId": app_id, "clientSecret": secret}))
        .send()
        .await?
        .json()
        .await?;
    data.get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::msg(format!("qq token: {data}")))
}

async fn gateway_url(http: &reqwest::Client, token: &str, bases: &[String]) -> Result<String> {
    let mut last = Error::msg("qq gateway: no api base");
    for base in bases {
        let resp = match http
            .get(format!("{base}/gateway"))
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = e.into();
                continue;
            }
        };
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            if let Some(url) = data.get("url").and_then(Value::as_str) {
                eprintln!("q38 qq gateway {base}");
                return Ok(url.to_string());
            }
        }
        last = Error::msg(format!("qq gateway {base} HTTP {status}: {data}"));
    }
    Err(last)
}

pub async fn run_gateway(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some((app_id, secret)) = credentials(&ep) else {
        return Err(Error::msg(
            "qq: extra.app_id and extra.client_secret required",
        ));
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let bases = api_bases(&ep);
    eprintln!("q38 qq gateway starting app_id={app_id}");
    loop {
        match run_once(&http, &ep, &mgr, &app_id, &secret, &bases).await {
            Ok(()) => eprintln!("q38 qq: socket closed, reconnecting"),
            Err(e) => eprintln!("q38 qq: {e}; retry in 2s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_once(
    http: &reqwest::Client,
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    app_id: &str,
    secret: &str,
    bases: &[String],
) -> Result<()> {
    let token = access_token(http, app_id, secret).await?;
    let url = gateway_url(http, &token, bases).await?;
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| Error::msg(format!("qq ws connect: {e}")))?;
    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));
    let last_seq = Arc::new(Mutex::new(None::<i64>));
    let mut hb: Option<tokio::task::JoinHandle<()>> = None;
    let intents = INTENT_PUBLIC_GUILD_MESSAGES
        | INTENT_GUILD_MEMBERS
        | INTENT_INTERACTION
        | INTENT_DIRECT_MESSAGE
        | INTENT_GROUP_AND_C2C;

    while let Some(frame) = read.next().await {
        let frame = frame.map_err(|e| Error::msg(format!("qq ws: {e}")))?;
        let Message::Text(text) = frame else { continue };
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let op = payload.get("op").and_then(Value::as_i64).unwrap_or(-1);
        if let Some(s) = payload.get("s").and_then(Value::as_i64) {
            *last_seq.lock().await = Some(s);
        }
        match op {
            OP_HELLO => {
                let interval = payload["d"]["heartbeat_interval"]
                    .as_u64()
                    .unwrap_or(45_000)
                    .max(5_000);
                let identify = json!({
                    "op": OP_IDENTIFY,
                    "d": {
                        "token": format!("QQBot {token}"),
                        "intents": intents,
                        "shard": [0, 1],
                    }
                });
                write
                    .lock()
                    .await
                    .send(Message::Text(identify.to_string().into()))
                    .await
                    .map_err(|e| Error::msg(format!("qq identify: {e}")))?;
                if let Some(h) = hb.take() {
                    h.abort();
                }
                let w = write.clone();
                let seq = last_seq.clone();
                hb = Some(tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_millis(interval));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        let d = *seq.lock().await;
                        let body = json!({"op": OP_HEARTBEAT, "d": d});
                        if w.lock()
                            .await
                            .send(Message::Text(body.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            OP_DISPATCH => {
                let t = payload.get("t").and_then(Value::as_str).unwrap_or("");
                if t == "READY" {
                    let sid = payload["d"]["session_id"].as_str().unwrap_or("");
                    eprintln!("q38 qq ready session={sid}");
                } else if let Some(env) = native_from_event(ep, t, &payload["d"]) {
                    if let Err(e) = mgr.ingest(env).await {
                        eprintln!("q38 qq ingest: {e}");
                    }
                }
            }
            OP_RECONNECT | OP_INVALID_SESSION => {
                if let Some(h) = hb.take() {
                    h.abort();
                }
                return Ok(());
            }
            _ => {}
        }
    }
    if let Some(h) = hb.take() {
        h.abort();
    }
    Ok(())
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn first_str(vals: &[&Value]) -> String {
    for v in vals {
        let s = js_str(v);
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

fn native_from_event(ep: &ChannelEndpoint, t: &str, d: &Value) -> Option<NativePayload> {
    let author = &d["author"];
    let (msg_type, sender, group) = match t {
        "C2C_MESSAGE_CREATE" => (
            "c2c",
            first_str(&[&author["user_openid"], &author["id"]]),
            String::new(),
        ),
        "GROUP_AT_MESSAGE_CREATE" => (
            "group",
            first_str(&[&author["member_openid"], &author["id"]]),
            js_str(&d["group_openid"]),
        ),
        "AT_MESSAGE_CREATE" | "DIRECT_MESSAGE_CREATE" => {
            ("guild", first_str(&[&author["id"]]), String::new())
        }
        _ => return None,
    };
    if sender.is_empty() {
        return None;
    }
    let text = d["content"].as_str().unwrap_or("").trim().to_string();
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::text(&text));
    }
    if let Some(atts) = d.get("attachments").and_then(Value::as_array) {
        for a in atts {
            let name = first_str(&[&a["filename"], &a["file_name"], &a["name"]]);
            let url = first_str(&[&a["url"], &a["content"]]);
            let ctype = first_str(&[&a["content_type"], &a["contentType"]]).to_ascii_lowercase();
            if ctype.starts_with("image/")
                || name.to_ascii_lowercase().ends_with(".png")
                || name.to_ascii_lowercase().ends_with(".jpg")
                || name.to_ascii_lowercase().ends_with(".jpeg")
            {
                if url.starts_with("http://") || url.starts_with("https://") {
                    parts.push(ContentPart::Image {
                        image_url: url,
                        url: String::new(),
                        mime: "image/jpeg".into(),
                    });
                } else {
                    parts.push(ContentPart::text("[图片]"));
                }
            } else if !name.is_empty() {
                parts.push(ContentPart::text(format!("[文件] {name}")));
            } else {
                parts.push(ContentPart::text("[文件]"));
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let text = NativePayload {
        content_parts: parts.clone(),
        ..NativePayload::default()
    }
    .query_text();
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "qq".into()
        } else {
            ep.kind.clone()
        },
        sender_id: sender.clone(),
        sender_name: author["username"].as_str().unwrap_or("").to_string(),
        content_parts: parts,
        text,
        ..NativePayload::default()
    };
    env.meta.insert("message_type".into(), json!(msg_type));
    env.meta
        .insert("is_group".into(), json!(msg_type == "group"));
    env.meta
        .insert("is_mentioned".into(), json!(msg_type != "c2c"));
    if let Some(id) = d["id"].as_str() {
        env.meta.insert("msg_id".into(), json!(id));
    }
    if !group.is_empty() {
        env.meta.insert("group_openid".into(), json!(group));
    }
    env.meta.insert("user_openid".into(), json!(sender));
    Some(env)
}

pub async fn send(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some((app_id, secret)) = credentials(ep) else {
        return Err(Error::msg("qq send: missing credentials"));
    };
    let text = super::outbound::parts_to_text(parts);
    if text.trim().is_empty() {
        return Ok(());
    }
    let http = reqwest::Client::new();
    let token = access_token(&http, &app_id, &secret).await?;
    let msg_type = env
        .meta
        .get("message_type")
        .and_then(Value::as_str)
        .unwrap_or("c2c");
    let path = if msg_type == "group" {
        let id = env
            .meta
            .get("group_openid")
            .and_then(Value::as_str)
            .unwrap_or("");
        format!("/v2/groups/{id}/messages")
    } else {
        let id = env
            .meta
            .get("user_openid")
            .and_then(Value::as_str)
            .unwrap_or(&env.sender_id);
        format!("/v2/users/{id}/messages")
    };
    let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut body = json!({
        "content": text.chars().take(2000).collect::<String>(),
        "msg_type": 0,
        "msg_seq": seq,
    });
    if let Some(id) = env.meta.get("msg_id").and_then(Value::as_str) {
        body["msg_id"] = json!(id);
    }
    let mut last = Error::msg("qq send failed");
    for base in api_bases(ep) {
        let resp = match http
            .post(format!("{base}{path}"))
            .header("Authorization", format!("QQBot {token}"))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = e.into();
                continue;
            }
        };
        if resp.status().is_success() {
            return Ok(());
        }
        last = Error::msg(format!(
            "qq send {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    Err(last)
}
