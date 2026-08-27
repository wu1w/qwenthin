//! WeChat iLink Bot long-poll. Thin wash of QwenPaw `wechat/client.py` +
//! Hermes `weixin.py`: HTTP `getupdates` inbound, `sendmessage` outbound.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const DEFAULT_BASE: &str = "https://ilinkai.weixin.qq.com";
const CHANNEL_VERSION: &str = "2.2.0";
const POLL_TIMEOUT_SECS: u64 = 45;
const DEDUP_CAP: usize = 512;
const TEXT_CLIP: usize = 4000;
const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 120;

pub fn credentials(ep: &ChannelEndpoint) -> Option<(String, String)> {
    let token = ep
        .extra
        .get("bot_token")
        .cloned()
        .or_else(|| ep.extra.get("token").cloned())
        .or_else(|| std::env::var("WECHAT_BOT_TOKEN").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let base = ep
        .extra
        .get("base_url")
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string());
    Some((token, base))
}

pub async fn run_poll(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some((token, base)) = credentials(&ep) else {
        return Err(Error::msg(
            "wechat: extra.bot_token or WECHAT_BOT_TOKEN required",
        ));
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(POLL_TIMEOUT_SECS))
        .build()?;
    let mut cursor = load_cursor(&ep.id);
    let mut seen = TokenDedup::new();
    let mut fail = 0u32;
    eprintln!("q38 wechat long-poll ({})", ep.id);
    loop {
        match poll_once(&http, &ep, &mgr, &token, &base, &mut cursor, &mut seen).await {
            Ok(()) => fail = 0,
            Err(e) => {
                fail = fail.saturating_add(1);
                let wait = backoff_secs(fail);
                eprintln!("q38 wechat: {e}; retry in {wait}s");
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }
    }
}

pub async fn send(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some((token, base)) = credentials(ep) else {
        return Err(Error::msg("wechat send: missing bot token"));
    };
    let to = env.chat_id();
    if to.is_empty() {
        return Err(Error::msg("wechat send: missing to_user_id"));
    }
    let text = clip(&super::outbound::parts_to_text(parts), TEXT_CLIP);
    if text.trim().is_empty() {
        return Ok(());
    }
    let context_token = meta_str(env, "context_token");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let url = format!("{base}/ilink/bot/sendmessage");
    let mut data = post_json(&http, &url, &token, &send_body(&to, &text, &context_token)).await?;
    if !api_ok(&data) && !context_token.is_empty() {
        // Hermes: session expired — retry once without context_token.
        data = post_json(&http, &url, &token, &send_body(&to, &text, "")).await?;
    }
    if !api_ok(&data) {
        return Err(Error::msg(format!(
            "wechat sendmessage ret={} errcode={}",
            json_i64(&data["ret"]),
            json_i64(&data["errcode"])
        )));
    }
    Ok(())
}

async fn poll_once(
    http: &reqwest::Client,
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    token: &str,
    base: &str,
    cursor: &mut String,
    seen: &mut TokenDedup,
) -> Result<()> {
    let url = format!("{base}/ilink/bot/getupdates");
    let body = json!({
        "get_updates_buf": cursor.as_str(),
        "base_info": {"channel_version": CHANNEL_VERSION},
    });
    let data = post_json(http, &url, token, &body).await?;
    if let Some(buf) = data.get("get_updates_buf") {
        if !buf.is_null() {
            *cursor = js_str(buf);
            save_cursor(&ep.id, cursor);
        }
    }
    let empty = Vec::new();
    let msgs = data.get("msgs").and_then(Value::as_array).unwrap_or(&empty);
    let ret = data
        .get("ret")
        .map(json_i64)
        .unwrap_or(if msgs.is_empty() { -1 } else { 0 });
    for msg in msgs {
        let ctx = js_str(&msg["context_token"]);
        if seen.insert(&ctx) {
            continue;
        }
        let Some(env) = native_from_msg(ep, msg) else {
            continue;
        };
        if let Err(e) = mgr.ingest(env).await {
            eprintln!("q38 wechat ingest: {e}");
        }
    }
    if ret == -1 && msgs.is_empty() {
        return Ok(());
    }
    if ret != 0 && msgs.is_empty() {
        return Err(Error::msg(format!("wechat getupdates ret={ret}")));
    }
    Ok(())
}

async fn post_json(http: &reqwest::Client, url: &str, token: &str, body: &Value) -> Result<Value> {
    let resp = http
        .post(url)
        .headers(headers(token)?)
        .json(body)
        .send()
        .await?;
    let status = resp.status();
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(Error::msg(format!("wechat HTTP {status}")));
    }
    Ok(data)
}

fn headers(bot_token: &str) -> Result<HeaderMap> {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(
        "AuthorizationType",
        HeaderValue::from_static("ilink_bot_token"),
    );
    let uin = (uuid::Uuid::new_v4().as_u128() & 0xffff_ffff) as u32;
    let uin_b64 = STANDARD.encode(uin.to_string());
    if let Ok(v) = HeaderValue::from_str(&uin_b64) {
        h.insert("X-WECHAT-UIN", v);
    }
    let auth = HeaderValue::from_str(&format!("Bearer {bot_token}"))
        .map_err(|_| Error::msg("wechat: bot token is not a valid HTTP header"))?;
    h.insert(AUTHORIZATION, auth);
    Ok(h)
}

fn send_body(to: &str, text: &str, context_token: &str) -> Value {
    let mut msg = json!({
        "from_user_id": "",
        "to_user_id": to,
        "client_id": uuid::Uuid::new_v4().to_string(),
        "message_type": 2,
        "message_state": 2,
        "item_list": [{"type": 1, "text_item": {"text": text}}],
    });
    if !context_token.is_empty() {
        msg["context_token"] = json!(context_token);
    }
    json!({
        "msg": msg,
        "base_info": {"channel_version": CHANNEL_VERSION},
    })
}

fn native_from_msg(ep: &ChannelEndpoint, msg: &Value) -> Option<NativePayload> {
    if json_i64(&msg["message_type"]) != 1 {
        return None;
    }
    let from = js_str(&msg["from_user_id"]);
    if from.is_empty() {
        return None;
    }
    let content_parts = parts_from_items(&msg["item_list"]);
    if content_parts.is_empty() {
        return None;
    }
    let text = NativePayload {
        content_parts: content_parts.clone(),
        ..NativePayload::default()
    }
    .query_text();
    let group_id = js_str(&msg["group_id"]);
    let is_group = !group_id.is_empty();
    let chat_id = if is_group { group_id } else { from.clone() };
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "wechat".into()
        } else {
            ep.kind.clone()
        },
        sender_id: from,
        content_parts,
        text,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta
        .insert("context_token".into(), json!(js_str(&msg["context_token"])));
    env.meta
        .insert("to_user_id".into(), json!(js_str(&msg["to_user_id"])));
    Some(env)
}

#[cfg(test)]
fn text_from_items(item_list: &Value) -> String {
    parts_from_items(item_list)
        .iter()
        .filter_map(ContentPart::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// iLink item types: 1 TEXT, 2 IMAGE, 3 VOICE, 4 FILE, 5 VIDEO.
/// Encrypted CDN blobs have no public URL; those become a caption so the turn
/// is not dropped. Voice often carries an ASR `text` field.
fn parts_from_items(item_list: &Value) -> Vec<ContentPart> {
    let Some(arr) = item_list.as_array() else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    for item in arr {
        match json_i64(&item["type"]) {
            1 => {
                let t = js_str(&item["text_item"]["text"]);
                let t = t.trim();
                if !t.is_empty() {
                    parts.push(ContentPart::text(t));
                }
            }
            2 => {
                if let Some(url) = first_http_url(&[
                    js_str(&item["image_item"]["url"]),
                    js_str(&item["image_item"]["image_url"]),
                    js_str(&item["image_item"]["media"]["url"]),
                ]) {
                    parts.push(ContentPart::Image {
                        image_url: url,
                        url: String::new(),
                        mime: "image/jpeg".into(),
                    });
                } else {
                    parts.push(ContentPart::text("[图片]"));
                }
            }
            3 => {
                let t = js_str(&item["voice_item"]["text"]);
                let t = t.trim();
                if !t.is_empty() {
                    parts.push(ContentPart::text(t));
                } else {
                    parts.push(ContentPart::text("[语音]"));
                }
            }
            4 => {
                let name = js_str(&item["file_item"]["file_name"]);
                let name = if name.is_empty() {
                    js_str(&item["file_item"]["media"]["file_name"])
                } else {
                    name
                };
                if name.is_empty() {
                    parts.push(ContentPart::text("[文件]"));
                } else {
                    parts.push(ContentPart::text(format!("[文件] {name}")));
                }
            }
            5 => parts.push(ContentPart::text("[视频]")),
            _ => {}
        }
    }
    parts
}

fn first_http_url(cands: &[String]) -> Option<String> {
    cands.iter().find_map(|s| {
        let t = s.trim();
        if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("data:") {
            Some(t.to_string())
        } else {
            None
        }
    })
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn json_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        Value::Bool(true) => 1,
        _ => 0,
    }
}

fn api_ok(data: &Value) -> bool {
    json_i64(&data["ret"]) == 0 && json_i64(&data["errcode"]) == 0
}

fn meta_str(env: &NativePayload, key: &str) -> String {
    match env.meta.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn backoff_secs(fail: u32) -> u64 {
    let exp = fail.saturating_sub(1).min(8);
    BACKOFF_MIN_SECS
        .saturating_mul(1u64 << exp)
        .min(BACKOFF_MAX_SECS)
}

fn cursor_path(id: &str) -> PathBuf {
    crate::config::Config::home_dir()
        .map(|h| h.join("channels").join(format!("{id}.wechat.cursor")))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/q38-{id}.wechat.cursor")))
}

fn load_cursor(id: &str) -> String {
    std::fs::read_to_string(cursor_path(id))
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn save_cursor(id: &str, cursor: &str) {
    let path = cursor_path(id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, cursor);
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!(
            "{}…",
            s.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    }
}

struct TokenDedup {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl TokenDedup {
    fn new() -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Returns `true` if `token` was already seen.
    fn insert(&mut self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        if !self.set.insert(token.to_string()) {
            return true;
        }
        self.order.push_back(token.to_string());
        while self.set.len() > DEDUP_CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            } else {
                break;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credentials_empty() {
        let ep = ChannelEndpoint::default();
        let from_env = std::env::var("WECHAT_BOT_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if from_env.is_none() {
            assert!(credentials(&ep).is_none());
        }
        let mut ep = ChannelEndpoint::default();
        ep.extra.insert("bot_token".into(), "   ".into());
        ep.extra.insert("token".into(), String::new());
        if from_env.is_none() {
            assert!(credentials(&ep).is_none());
        }
    }

    #[test]
    fn credentials_from_extra() {
        let mut ep = ChannelEndpoint::default();
        ep.extra.insert("bot_token".into(), " secret ".into());
        let (tok, base) = credentials(&ep).expect("token");
        assert_eq!(tok, "secret");
        assert_eq!(base, DEFAULT_BASE);
        ep.extra
            .insert("base_url".into(), "https://example.test/".into());
        let (_, base) = credentials(&ep).expect("token");
        assert_eq!(base, "https://example.test");
    }

    #[test]
    fn text_from_item_list() {
        let items = json!([
            {"type": 1, "text_item": {"text": " hi "}},
            {"type": 2, "image_item": {}},
            {"type": 1, "text_item": {"text": ""}},
            {"type": "1", "text_item": {"text": "there"}},
        ]);
        assert_eq!(text_from_items(&items), "hi\n[图片]\nthere");
        assert!(text_from_items(&json!([])).is_empty());
    }

    #[test]
    fn native_ingests_image_voice_file() {
        let ep = ChannelEndpoint::default();
        let img = json!({
            "message_type": 1,
            "from_user_id": "u",
            "item_list": [{"type": 2, "image_item": {}}],
        });
        let env = native_from_msg(&ep, &img).expect("image-only");
        assert_eq!(env.text, "[图片]");
        let with_url = json!({
            "message_type": 1,
            "from_user_id": "u",
            "item_list": [{
                "type": 2,
                "image_item": {"url": "https://cdn.example/p.jpg"}
            }],
        });
        let env = native_from_msg(&ep, &with_url).expect("image url");
        assert!(env
            .media_parts()
            .iter()
            .any(|m| m.url.contains("cdn.example")));
        let voice = json!({
            "message_type": 1,
            "from_user_id": "u",
            "item_list": [{"type": 3, "voice_item": {"text": "打开灯"}}],
        });
        let env = native_from_msg(&ep, &voice).expect("voice asr");
        assert_eq!(env.text, "打开灯");
        let file = json!({
            "message_type": 1,
            "from_user_id": "u",
            "item_list": [{"type": 4, "file_item": {"file_name": "a.pdf"}}],
        });
        let env = native_from_msg(&ep, &file).expect("file");
        assert_eq!(env.text, "[文件] a.pdf");
    }

    #[test]
    fn native_skips_non_user_and_empty() {
        let ep = ChannelEndpoint::default();
        let bot = json!({
            "message_type": 2,
            "from_user_id": "u",
            "item_list": [{"type": 1, "text_item": {"text": "x"}}],
        });
        assert!(native_from_msg(&ep, &bot).is_none());
        let empty = json!({
            "message_type": 1,
            "from_user_id": "u",
            "item_list": [],
        });
        assert!(native_from_msg(&ep, &empty).is_none());
        let ok = json!({
            "message_type": 1,
            "from_user_id": "u1",
            "to_user_id": "bot",
            "context_token": "ct",
            "item_list": [{"type": 1, "text_item": {"text": "hello"}}],
        });
        let env = native_from_msg(&ep, &ok).expect("user text");
        assert_eq!(env.channel, "wechat");
        assert_eq!(env.sender_id, "u1");
        assert_eq!(env.text, "hello");
        assert_eq!(env.meta["chat_id"], json!("u1"));
        assert_eq!(env.meta["is_group"], json!(false));
        assert_eq!(env.meta["context_token"], json!("ct"));
        assert_eq!(env.meta["to_user_id"], json!("bot"));
        let group = json!({
            "message_type": 1,
            "from_user_id": "u1",
            "group_id": "g9",
            "item_list": [{"type": 1, "text_item": {"text": "hi"}}],
        });
        let env = native_from_msg(&ep, &group).expect("group");
        assert_eq!(env.meta["chat_id"], json!("g9"));
        assert_eq!(env.meta["is_group"], json!(true));
    }

    #[test]
    fn backoff_caps() {
        assert_eq!(backoff_secs(1), 5);
        assert_eq!(backoff_secs(2), 10);
        assert_eq!(backoff_secs(3), 20);
        assert_eq!(backoff_secs(20), BACKOFF_MAX_SECS);
    }
}
