//! WeCom AI Bot WebSocket gateway. Wash of Hermes `wecom.py` (not the Python aibot SDK).
//!
//! After QR bind saves `bot_id` + `secret`, this process holds one WS to
//! `wss://openws.work.weixin.qq.com`. Replies must go on that same socket:
//! `run_gateway` registers an `mpsc` writer keyed by `bot_id`; `send()` looks it up.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const DEFAULT_WS_URL: &str = "wss://openws.work.weixin.qq.com";
const DEVICE_ID: &str = "q38";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HEARTBEAT: Duration = Duration::from_secs(30);
const MAX_MARKDOWN: usize = 4000;

const CMD_SUBSCRIBE: &str = "aibot_subscribe";
const CMD_CALLBACK: &str = "aibot_msg_callback";
const CMD_LEGACY_CALLBACK: &str = "aibot_callback";
const CMD_SEND: &str = "aibot_send_msg";
const CMD_RESPOND: &str = "aibot_respond_msg";
const CMD_PING: &str = "ping";

static SENDERS: OnceLock<StdMutex<HashMap<String, mpsc::UnboundedSender<Value>>>> = OnceLock::new();

fn senders() -> &'static StdMutex<HashMap<String, mpsc::UnboundedSender<Value>>> {
    SENDERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn register_sender(bot_id: &str, tx: mpsc::UnboundedSender<Value>) {
    senders()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(bot_id.to_string(), tx);
}

fn unregister_sender(bot_id: &str, tx: &mpsc::UnboundedSender<Value>) {
    let mut g = senders().lock().unwrap_or_else(|e| e.into_inner());
    if g.get(bot_id).is_some_and(|cur| cur.same_channel(tx)) {
        g.remove(bot_id);
    }
}

fn sender_for(bot_id: &str) -> Option<mpsc::UnboundedSender<Value>> {
    senders()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(bot_id)
        .cloned()
}

struct ConnGuard {
    bot_id: String,
    tx: mpsc::UnboundedSender<Value>,
    hb: Option<tokio::task::JoinHandle<()>>,
    fwd: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        unregister_sender(&self.bot_id, &self.tx);
        if let Some(h) = self.hb.take() {
            h.abort();
        }
        if let Some(f) = self.fwd.take() {
            f.abort();
        }
    }
}

pub fn credentials(ep: &ChannelEndpoint) -> Option<(String, String)> {
    let bot = std::env::var("WECOM_BOT_ID").ok();
    let secret = std::env::var("WECOM_SECRET").ok();
    resolve_credentials(&ep.extra, bot.as_deref(), secret.as_deref())
}

fn resolve_credentials(
    extra: &BTreeMap<String, String>,
    env_bot: Option<&str>,
    env_secret: Option<&str>,
) -> Option<(String, String)> {
    let bot_id = nonempty_extra(extra, "bot_id").or_else(|| nonempty_opt(env_bot))?;
    let secret = nonempty_extra(extra, "secret").or_else(|| nonempty_opt(env_secret))?;
    Some((bot_id, secret))
}

fn websocket_url(ep: &ChannelEndpoint) -> String {
    let env = std::env::var("WECOM_WEBSOCKET_URL").ok();
    resolve_ws_url(&ep.extra, env.as_deref())
}

fn resolve_ws_url(extra: &BTreeMap<String, String>, env: Option<&str>) -> String {
    for key in ["websocket_url", "websocketUrl"] {
        if let Some(u) = nonempty_extra(extra, key) {
            return u;
        }
    }
    nonempty_opt(env).unwrap_or_else(|| DEFAULT_WS_URL.to_string())
}

fn nonempty_extra(extra: &BTreeMap<String, String>, key: &str) -> Option<String> {
    extra
        .get(key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn nonempty_opt(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub async fn run_gateway(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some((bot_id, secret)) = credentials(&ep) else {
        return Err(Error::msg(
            "wecom: extra.bot_id and extra.secret required (or WECOM_BOT_ID / WECOM_SECRET)",
        ));
    };
    let url = websocket_url(&ep);
    eprintln!("q38 wecom gateway starting bot_id={bot_id}");
    loop {
        match run_once(&ep, &mgr, &bot_id, &secret, &url).await {
            Ok(()) => eprintln!("q38 wecom: socket closed, reconnecting"),
            Err(e) => eprintln!("q38 wecom: {e}; retry in 2s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_once(
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    bot_id: &str,
    secret: &str,
    url: &str,
) -> Result<()> {
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| Error::msg("wecom: connect timeout"))?
        .map_err(|e| Error::msg(format!("wecom ws connect: {e}")))?;
    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));

    let req_id = uuid::Uuid::new_v4().to_string();
    let subscribe = json!({
        "cmd": CMD_SUBSCRIBE,
        "headers": {"req_id": req_id},
        "body": {
            "bot_id": bot_id,
            "secret": secret,
            "device_id": DEVICE_ID,
        }
    });
    write
        .lock()
        .await
        .send(Message::Text(subscribe.to_string().into()))
        .await
        .map_err(|e| Error::msg(format!("wecom subscribe: {e}")))?;

    tokio::time::timeout(CONNECT_TIMEOUT, async {
        loop {
            let frame = match read.next().await {
                Some(Ok(f)) => f,
                Some(Err(e)) => return Err(Error::msg(format!("wecom ws: {e}"))),
                None => return Err(Error::msg("wecom: socket closed during subscribe")),
            };
            match frame {
                Message::Ping(d) => {
                    let _ = write.lock().await.send(Message::Pong(d)).await;
                }
                Message::Close(_) => {
                    return Err(Error::msg("wecom: socket closed during subscribe"));
                }
                Message::Text(text) => {
                    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    if payload_req_id(&payload) == req_id {
                        return subscribe_ack_ok(&payload);
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| Error::msg("wecom: timed out waiting for subscribe ack"))??;

    eprintln!("q38 wecom: subscribed bot_id={bot_id}");

    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let write_fwd = write.clone();
    let fwd = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            if write_fwd
                .lock()
                .await
                .send(Message::Text(v.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let tx_hb = tx.clone();
    let hb = tokio::spawn(async move {
        loop {
            tokio::time::sleep(HEARTBEAT).await;
            let ping = json!({
                "cmd": CMD_PING,
                "headers": {"req_id": format!("ping-{}", uuid::Uuid::new_v4())},
                "body": {}
            });
            if tx_hb.send(ping).is_err() {
                break;
            }
        }
    });
    register_sender(bot_id, tx.clone());
    let _guard = ConnGuard {
        bot_id: bot_id.to_string(),
        tx,
        hb: Some(hb),
        fwd: Some(fwd),
    };

    while let Some(frame) = read.next().await {
        let frame = frame.map_err(|e| Error::msg(format!("wecom ws: {e}")))?;
        match frame {
            Message::Ping(d) => {
                let _ = write.lock().await.send(Message::Pong(d)).await;
            }
            Message::Close(_) => return Ok(()),
            Message::Text(text) => {
                let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let cmd = payload.get("cmd").and_then(Value::as_str).unwrap_or("");
                if cmd == CMD_CALLBACK || cmd == CMD_LEGACY_CALLBACK {
                    if let Some(env) = native_from_callback(ep, &payload) {
                        if let Err(e) = mgr.ingest(env).await {
                            eprintln!("q38 wecom ingest: {e}");
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub async fn send(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let Some(ep) = ep else {
        return Ok(());
    };
    let Some((bot_id, _)) = credentials(ep) else {
        return Err(Error::msg("wecom send: missing credentials"));
    };
    let text = super::outbound::parts_to_text(parts);
    if text.trim().is_empty() {
        return Ok(());
    }
    let frame = outbound_frame(env, &text)?;
    let tx =
        sender_for(&bot_id).ok_or_else(|| Error::msg("wecom send: websocket not connected"))?;
    tx.send(frame)
        .map_err(|_| Error::msg("wecom send: websocket not connected"))?;
    Ok(())
}

fn outbound_frame(env: &NativePayload, text: &str) -> Result<Value> {
    let content: String = text.chars().take(MAX_MARKDOWN).collect();
    let reply_req_id = env
        .meta
        .get("reply_req_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !reply_req_id.is_empty() {
        return Ok(json!({
            "cmd": CMD_RESPOND,
            "headers": {"req_id": reply_req_id},
            "body": {
                "msgtype": "markdown",
                "markdown": {"content": content},
            }
        }));
    }
    let chat_id = env.chat_id();
    if chat_id.is_empty() {
        return Err(Error::msg("wecom send: missing chat_id"));
    }
    Ok(json!({
        "cmd": CMD_SEND,
        "headers": {"req_id": uuid::Uuid::new_v4().to_string()},
        "body": {
            "chatid": chat_id,
            "msgtype": "markdown",
            "markdown": {"content": content},
        }
    }))
}

fn native_from_callback(ep: &ChannelEndpoint, payload: &Value) -> Option<NativePayload> {
    let body = payload.get("body")?;
    if !body.is_object() {
        return None;
    }
    let sender_id = js_str(&body["from"]["userid"]).trim().to_string();
    let mut chat_id = js_str(&body["chatid"]).trim().to_string();
    if chat_id.is_empty() {
        chat_id = sender_id.clone();
    }
    if chat_id.is_empty() {
        return None;
    }
    let is_group = body
        .get("chattype")
        .and_then(Value::as_str)
        .unwrap_or("")
        .eq_ignore_ascii_case("group");
    let mut text = extract_text(body);
    if is_group {
        text = strip_leading_mention(&text);
    }
    if text.is_empty() {
        return None;
    }
    let msgid = {
        let m = js_str(&body["msgid"]).trim().to_string();
        if m.is_empty() {
            payload_req_id(payload)
        } else {
            m
        }
    };
    let reply_req_id = payload_req_id(payload);
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "wecom".into()
        } else {
            ep.kind.clone()
        },
        sender_id: sender_id.clone(),
        sender_name: sender_id,
        content_parts: vec![ContentPart::text(&text)],
        text,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    if !reply_req_id.is_empty() {
        env.meta.insert("reply_req_id".into(), json!(reply_req_id));
    }
    if !msgid.is_empty() {
        env.meta.insert("msgid".into(), json!(msgid));
    }
    Some(env)
}

fn extract_text(body: &Value) -> String {
    let msgtype = body
        .get("msgtype")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut parts: Vec<String> = Vec::new();
    if msgtype == "mixed" {
        if let Some(items) = body["mixed"]["msg_item"].as_array() {
            for item in items {
                if !item.is_object() {
                    continue;
                }
                let item_type = item
                    .get("msgtype")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if item_type == "text" {
                    let c = js_str(&item["text"]["content"]);
                    let c = c.trim();
                    if !c.is_empty() {
                        parts.push(c.to_string());
                    }
                } else if item_type == "image" {
                    parts.push("[图片]".into());
                }
            }
        }
    } else {
        let c = js_str(&body["text"]["content"]);
        let c = c.trim();
        if !c.is_empty() {
            parts.push(c.to_string());
        }
    }
    if msgtype == "voice" {
        let c = js_str(&body["voice"]["content"]);
        let c = c.trim();
        if !c.is_empty() {
            parts.push(c.to_string());
        } else {
            parts.push("[语音]".into());
        }
    }
    if msgtype == "image" {
        parts.push("[图片]".into());
    }
    if msgtype == "file" || msgtype == "video" {
        let name = js_str(&body[msgtype.as_str()]["filename"]);
        let name = if name.is_empty() {
            js_str(&body[msgtype.as_str()]["file_name"])
        } else {
            name
        };
        if msgtype == "video" {
            parts.push("[视频]".into());
        } else if name.is_empty() {
            parts.push("[文件]".into());
        } else {
            parts.push(format!("[文件] {name}"));
        }
    }
    parts.join("\n")
}

fn strip_leading_mention(text: &str) -> String {
    let t = text.trim();
    let Some(rest) = t.strip_prefix('@') else {
        return t.to_string();
    };
    match rest.find(char::is_whitespace) {
        Some(i) => rest[i..].trim().to_string(),
        None => String::new(),
    }
}

fn payload_req_id(payload: &Value) -> String {
    js_str(&payload["headers"]["req_id"]).trim().to_string()
}

fn subscribe_ack_ok(payload: &Value) -> Result<()> {
    if errcode_ok(payload) {
        Ok(())
    } else {
        let code = &payload["errcode"];
        let msg = payload
            .get("errmsg")
            .and_then(Value::as_str)
            .unwrap_or("authentication failed");
        Err(Error::msg(format!(
            "wecom subscribe: {msg} (errcode={code})"
        )))
    }
}

fn errcode_ok(payload: &Value) -> bool {
    match payload.get("errcode") {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => n.as_i64() == Some(0) || n.as_u64() == Some(0),
        Some(Value::String(s)) => {
            let t = s.trim();
            t.is_empty() || t == "0"
        }
        _ => false,
    }
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn extra(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn credentials_from_extra() {
        let m = extra(&[("bot_id", "  bid  "), ("secret", " sec ")]);
        assert_eq!(
            resolve_credentials(&m, Some("env-bot"), Some("env-sec")),
            Some(("bid".into(), "sec".into()))
        );
    }

    #[test]
    fn credentials_from_env_when_extra_empty() {
        let m = BTreeMap::new();
        assert_eq!(
            resolve_credentials(&m, Some("env-bot"), Some("env-sec")),
            Some(("env-bot".into(), "env-sec".into()))
        );
        assert_eq!(resolve_credentials(&m, Some("env-bot"), None), None);
        assert_eq!(resolve_credentials(&m, None, Some("env-sec")), None);
    }

    #[test]
    fn websocket_url_prefers_extra() {
        let m = extra(&[("websocket_url", "wss://example.test/ws")]);
        assert_eq!(
            resolve_ws_url(&m, Some("wss://env.example/ws")),
            "wss://example.test/ws"
        );
        assert_eq!(resolve_ws_url(&BTreeMap::new(), None), DEFAULT_WS_URL);
        assert_eq!(
            resolve_ws_url(&BTreeMap::new(), Some("wss://env.example/ws")),
            "wss://env.example/ws"
        );
    }

    #[test]
    fn extract_text_plain() {
        let body = json!({"msgtype":"text","text":{"content":"  hello  "}});
        assert_eq!(extract_text(&body), "hello");
    }

    #[test]
    fn extract_text_mixed_and_voice() {
        let mixed = json!({
            "msgtype": "mixed",
            "mixed": {"msg_item": [
                {"msgtype": "text", "text": {"content": "one"}},
                {"msgtype": "image"},
                {"msgtype": "text", "text": {"content": "two"}}
            ]}
        });
        assert_eq!(extract_text(&mixed), "one\n[图片]\ntwo");
        let voice = json!({"msgtype":"voice","voice":{"content":"said this"}});
        assert_eq!(extract_text(&voice), "said this");
        let img = json!({"msgtype":"image"});
        assert_eq!(extract_text(&img), "[图片]");
        let empty = json!({"msgtype":"text","text":{"content":"  "}});
        assert!(extract_text(&empty).is_empty());
    }

    #[test]
    fn native_skips_empty_text() {
        let ep = ChannelEndpoint {
            kind: "wecom".into(),
            ..ChannelEndpoint::default()
        };
        let payload = json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "r1"},
            "body": {
                "msgid": "m1",
                "from": {"userid": "u1"},
                "chatid": "c1",
                "chattype": "single",
                "msgtype": "text",
                "text": {"content": ""}
            }
        });
        assert!(native_from_callback(&ep, &payload).is_none());
    }

    #[test]
    fn native_text_meta_and_group_mention() {
        let ep = ChannelEndpoint {
            kind: "wecom".into(),
            ..ChannelEndpoint::default()
        };
        let payload = json!({
            "cmd": "aibot_callback",
            "headers": {"req_id": "req-abc"},
            "body": {
                "msgid": "m9",
                "from": {"userid": "user-1"},
                "chatid": "wr-group",
                "chattype": "group",
                "msgtype": "text",
                "text": {"content": "@Bot hello"}
            }
        });
        let env = native_from_callback(&ep, &payload).unwrap();
        assert_eq!(env.channel, "wecom");
        assert_eq!(env.sender_id, "user-1");
        assert_eq!(env.text, "hello");
        assert_eq!(env.meta["chat_id"], json!("wr-group"));
        assert_eq!(env.meta["is_group"], json!(true));
        assert_eq!(env.meta["reply_req_id"], json!("req-abc"));
        assert_eq!(env.meta["msgid"], json!("m9"));
        assert!(env.is_group());
        assert_eq!(env.chat_id(), "wr-group");
    }

    #[test]
    fn dm_chat_id_falls_back_to_sender() {
        let ep = ChannelEndpoint {
            kind: "wecom".into(),
            ..ChannelEndpoint::default()
        };
        let payload = json!({
            "headers": {"req_id": "r2"},
            "body": {
                "msgid": "m2",
                "from": {"userid": "u2"},
                "msgtype": "text",
                "text": {"content": "hi"}
            }
        });
        let env = native_from_callback(&ep, &payload).unwrap();
        assert_eq!(env.meta["chat_id"], json!("u2"));
        assert_eq!(env.meta["is_group"], json!(false));
    }

    #[test]
    fn outbound_respond_uses_same_req_id() {
        let mut env = NativePayload::default();
        env.meta.insert("reply_req_id".into(), json!("req-abc"));
        env.meta.insert("chat_id".into(), json!("wr-group"));
        let frame = outbound_frame(&env, "pong").unwrap();
        assert_eq!(frame["cmd"], CMD_RESPOND);
        assert_eq!(frame["headers"]["req_id"], "req-abc");
        assert_eq!(frame["body"]["msgtype"], "markdown");
        assert_eq!(frame["body"]["markdown"]["content"], "pong");
        assert!(frame["body"].get("chatid").is_none());
    }

    #[test]
    fn outbound_send_uses_chatid() {
        let mut env = NativePayload::default();
        env.meta.insert("chat_id".into(), json!("chat-1"));
        let frame = outbound_frame(&env, "hello").unwrap();
        assert_eq!(frame["cmd"], CMD_SEND);
        assert_eq!(frame["body"]["chatid"], "chat-1");
        assert_eq!(frame["body"]["markdown"]["content"], "hello");
        assert!(!frame["headers"]["req_id"].as_str().unwrap().is_empty());
    }

    #[test]
    fn subscribe_errcode() {
        assert!(errcode_ok(&json!({})));
        assert!(errcode_ok(&json!({"errcode": 0})));
        assert!(errcode_ok(&json!({"errcode": null})));
        assert!(!errcode_ok(&json!({"errcode": 40013, "errmsg": "invalid"})));
        assert!(subscribe_ack_ok(&json!({"errcode": 1, "errmsg": "bad"})).is_err());
    }

    #[tokio::test]
    async fn send_writes_through_bot_id_slot() {
        let bot_id = format!("bot-{}", uuid::Uuid::new_v4());
        let (tx, mut rx) = mpsc::unbounded_channel();
        register_sender(&bot_id, tx.clone());
        let mut ep = ChannelEndpoint {
            kind: "wecom".into(),
            ..ChannelEndpoint::default()
        };
        ep.extra.insert("bot_id".into(), bot_id.clone());
        ep.extra.insert("secret".into(), "unit-test-secret".into());
        let mut env = NativePayload::default();
        env.meta.insert("reply_req_id".into(), json!("rid-1"));
        env.meta.insert("chat_id".into(), json!("c1"));
        send(Some(&ep), &env, &[ContentPart::text("hi")])
            .await
            .unwrap();
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame["cmd"], CMD_RESPOND);
        assert_eq!(frame["headers"]["req_id"], "rid-1");
        assert_eq!(frame["body"]["markdown"]["content"], "hi");
        unregister_sender(&bot_id, &tx);
        send(Some(&ep), &env, &[ContentPart::text("again")])
            .await
            .unwrap_err();
    }
}
