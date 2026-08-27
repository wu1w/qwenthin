//! DingTalk Stream Mode. Open-connection JSON + WebSocket; replies via sessionWebhook.
//!
//! No `dingtalk-stream` Python SDK. Chatbot fields match QwenPaw; markdown
//! send matches Hermes.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const OPEN_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const BOT_TOPIC: &str = "/v1.0/im/bot/messages/get";
const ACK_SUCCESS: &str = r#"{"status":"SUCCESS"}"#;
const MAX_MARKDOWN: usize = 20_000;

pub fn credentials(ep: &ChannelEndpoint) -> Option<(String, String)> {
    let env_id = std::env::var("DINGTALK_CLIENT_ID").ok();
    let env_secret = std::env::var("DINGTALK_CLIENT_SECRET").ok();
    pick_credentials(
        ep.extra.get("client_id").map(String::as_str),
        ep.extra.get("client_secret").map(String::as_str),
        env_id.as_deref(),
        env_secret.as_deref(),
    )
}

fn pick_credentials(
    extra_id: Option<&str>,
    extra_secret: Option<&str>,
    env_id: Option<&str>,
    env_secret: Option<&str>,
) -> Option<(String, String)> {
    let id = nonempty(extra_id).or_else(|| nonempty(env_id))?;
    let secret = nonempty(extra_secret).or_else(|| nonempty(env_secret))?;
    Some((id.to_string(), secret.to_string()))
}

fn nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

pub async fn run_gateway(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some((client_id, client_secret)) = credentials(&ep) else {
        return Err(Error::msg(
            "dingtalk: extra.client_id/client_secret or DINGTALK_CLIENT_ID/DINGTALK_CLIENT_SECRET required",
        ));
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    eprintln!("q38 dingtalk gateway starting client_id={client_id}");
    loop {
        match run_once(&http, &ep, &mgr, &client_id, &client_secret).await {
            Ok(()) => eprintln!("q38 dingtalk: socket closed, reconnecting"),
            Err(e) => eprintln!("q38 dingtalk: {e}; retry in 2s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_once(
    http: &reqwest::Client,
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    client_id: &str,
    client_secret: &str,
) -> Result<()> {
    let (endpoint, ticket) = open_connection(http, client_id, client_secret).await?;
    let url = ws_url(&endpoint, &ticket);
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| Error::msg(format!("dingtalk ws connect: {e}")))?;
    let (mut write, mut read) = ws.split();
    eprintln!("q38 dingtalk connected");

    while let Some(frame) = read.next().await {
        let frame = frame.map_err(|e| Error::msg(format!("dingtalk ws: {e}")))?;
        match frame {
            Message::Close(_) => break,
            Message::Text(text) => {
                let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let ty = payload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                let message_id = header_str(&payload, "messageId");
                match ty.as_str() {
                    "SYSTEM" => {
                        let topic = header_str(&payload, "topic");
                        if topic.eq_ignore_ascii_case("disconnect") {
                            eprintln!("q38 dingtalk: disconnect");
                            return Ok(());
                        }
                        write
                            .send(Message::Text(
                                ack_json(message_id, &frame_data_raw(&payload)).into(),
                            ))
                            .await
                            .map_err(|e| Error::msg(format!("dingtalk ack: {e}")))?;
                    }
                    "CALLBACK" | "EVENT" => {
                        write
                            .send(Message::Text(ack_json(message_id, ACK_SUCCESS).into()))
                            .await
                            .map_err(|e| Error::msg(format!("dingtalk ack: {e}")))?;
                        if ty == "CALLBACK" {
                            if let Some(data) = parse_chatbot_data(&payload["data"]) {
                                if let Some(env) = native_from_chatbot(ep, &data) {
                                    let mgr = mgr.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = mgr.ingest(env).await {
                                            eprintln!("q38 dingtalk ingest: {e}");
                                        }
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn open_connection(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
) -> Result<(String, String)> {
    let body = json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "subscriptions": [
            {"type": "CALLBACK", "topic": BOT_TOPIC},
            {"type": "EVENT", "topic": "*"}
        ],
        "ua": "q38",
        "localIp": "127.0.0.1"
    });
    let resp = http.post(OPEN_URL).json(&body).send().await?;
    let status = resp.status();
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(Error::msg(format!("dingtalk open {status}: {data}")));
    }
    let endpoint = data
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let ticket = data
        .get("ticket")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if endpoint.is_empty() || ticket.is_empty() {
        return Err(Error::msg(format!(
            "dingtalk open: missing endpoint/ticket {data}"
        )));
    }
    Ok((endpoint.to_string(), ticket.to_string()))
}

fn ws_url(endpoint: &str, ticket: &str) -> String {
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    format!("{endpoint}{sep}ticket={ticket}")
}

fn header_str<'a>(frame: &'a Value, key: &str) -> &'a str {
    frame
        .get("headers")
        .and_then(|h| h.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn ack_json(message_id: &str, data: &str) -> String {
    json!({
        "code": 200,
        "headers": {
            "messageId": message_id,
            "contentType": "application/json"
        },
        "message": "OK",
        "data": data
    })
    .to_string()
}

fn frame_data_raw(frame: &Value) -> String {
    match frame.get("data") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "{}".into(),
    }
}

fn parse_chatbot_data(data: &Value) -> Option<Value> {
    match data {
        Value::String(raw) => serde_json::from_str(raw).ok(),
        Value::Object(_) => Some(data.clone()),
        _ => None,
    }
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn first_str(obj: &Value, keys: &[&str]) -> String {
    for k in keys {
        let s = js_str(&obj[*k]);
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    String::new()
}

fn json_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Value::Number(n) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn chatbot_text(data: &Value) -> String {
    let text = &data["text"];
    let content = match text {
        Value::String(s) => s.clone(),
        Value::Object(_) => js_str(&text["content"]),
        _ => String::new(),
    };
    let content = content.trim().to_string();
    if !content.is_empty() {
        return content;
    }
    let msgtype = first_str(data, &["msgtype", "msgType"]).to_ascii_lowercase();
    match msgtype.as_str() {
        "picture" | "image" | "photo" => "[图片]".into(),
        "video" => "[视频]".into(),
        "audio" | "voice" => "[语音]".into(),
        "file" => {
            let n = first_str(data, &["fileName", "filename", "file_name"]);
            if n.is_empty() {
                "[文件]".into()
            } else {
                format!("[文件] {n}")
            }
        }
        _ => String::new(),
    }
}

fn session_webhook(data: &Value) -> String {
    first_str(data, &["sessionWebhook", "session_webhook"])
}

fn native_from_chatbot(ep: &ChannelEndpoint, data: &Value) -> Option<NativePayload> {
    let sender = first_str(
        data,
        &["senderId", "sender_id", "senderStaffId", "sender_staff_id"],
    );
    if sender.is_empty() {
        return None;
    }
    let text = chatbot_text(data);
    if text.is_empty() {
        return None;
    }
    let conversation_id = first_str(data, &["conversationId", "conversation_id"]);
    let conv_type = first_str(data, &["conversationType", "conversation_type"]);
    let is_group = conv_type == "2";
    let mentioned = json_bool(&data["isInAtList"]) || json_bool(&data["is_in_at_list"]);
    let webhook = session_webhook(data);
    let msg_id = first_str(data, &["msgId", "msg_id"]);
    let chat_id = if conversation_id.is_empty() {
        sender.clone()
    } else {
        conversation_id.clone()
    };
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "dingtalk".into()
        } else {
            ep.kind.clone()
        },
        sender_id: sender,
        sender_name: first_str(data, &["senderNick", "sender_nick"]),
        content_parts: vec![ContentPart::text(&text)],
        text,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("is_mentioned".into(), json!(mentioned));
    if !webhook.is_empty() {
        env.meta.insert("session_webhook".into(), json!(webhook));
    }
    if !msg_id.is_empty() {
        env.meta.insert("msg_id".into(), json!(msg_id));
    }
    if !conversation_id.is_empty() {
        env.meta
            .insert("conversation_id".into(), json!(conversation_id));
    }
    Some(env)
}

pub async fn send(
    ep: Option<&ChannelEndpoint>,
    env: &NativePayload,
    parts: &[ContentPart],
) -> Result<()> {
    let _ = ep;
    let text = super::outbound::parts_to_text(parts);
    if text.trim().is_empty() {
        return Ok(());
    }
    let Some(url) = webhook_from_env(env) else {
        return Err(Error::msg("dingtalk send: missing session_webhook"));
    };
    let clipped: String = text.chars().take(MAX_MARKDOWN).collect();
    let body = json!({
        "msgtype": "markdown",
        "markdown": {"title": "q38", "text": clipped},
    });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let resp = http.post(&url).json(&body).send().await?;
    let status = resp.status();
    let t = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::msg(format!("dingtalk send {status} {t}")));
    }
    if let Ok(v) = serde_json::from_str::<Value>(&t) {
        let errcode = v.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        if errcode != 0 {
            let errmsg = v.get("errmsg").and_then(Value::as_str).unwrap_or("");
            return Err(Error::msg(format!(
                "dingtalk send errcode={errcode} {errmsg}"
            )));
        }
    }
    Ok(())
}

fn webhook_from_env(env: &NativePayload) -> Option<String> {
    env.reply_url()
        .or_else(|| match env.meta.get("sessionWebhook") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep() -> ChannelEndpoint {
        ChannelEndpoint {
            kind: "dingtalk".into(),
            ..ChannelEndpoint::default()
        }
    }

    fn chatbot_object() -> Value {
        json!({
            "text": {"content": "  hello  "},
            "senderId": "$:LWCP_v1:$user",
            "senderStaffId": "manager1",
            "senderNick": "Will",
            "conversationId": "cidABC==",
            "conversationType": "2",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/sendBySession?session=tok",
            "msgId": "msgLICYe==",
            "isInAtList": true,
            "atUsers": [{"dingtalkId": "bot"}]
        })
    }

    #[test]
    fn credentials_from_extra() {
        let mut e = ep();
        e.extra.insert("client_id".into(), " id-1 ".into());
        e.extra.insert("client_secret".into(), " sec-1 ".into());
        assert_eq!(credentials(&e), Some(("id-1".into(), "sec-1".into())));
    }

    #[test]
    fn credentials_extra_beats_env() {
        assert_eq!(
            pick_credentials(
                Some("qr-id"),
                Some("qr-sec"),
                Some("env-id"),
                Some("env-sec")
            ),
            Some(("qr-id".into(), "qr-sec".into()))
        );
    }

    #[test]
    fn credentials_env_fallback() {
        assert_eq!(
            pick_credentials(None, None, Some("env-id"), Some("env-sec")),
            Some(("env-id".into(), "env-sec".into()))
        );
        assert_eq!(
            pick_credentials(Some("id"), None, None, Some("env-sec")),
            Some(("id".into(), "env-sec".into()))
        );
        assert!(pick_credentials(Some("id"), Some("  "), None, None).is_none());
        assert!(pick_credentials(None, None, None, None).is_none());
    }

    #[test]
    fn parse_chatbot_data_string_vs_object() {
        let obj = chatbot_object();
        let as_string = Value::String(obj.to_string());
        let from_str = parse_chatbot_data(&as_string).unwrap();
        let from_obj = parse_chatbot_data(&obj).unwrap();
        assert_eq!(from_str["conversationId"], "cidABC==");
        assert_eq!(from_obj["conversationId"], "cidABC==");
        assert_eq!(
            session_webhook(&from_str),
            "https://oapi.dingtalk.com/robot/sendBySession?session=tok"
        );
        assert_eq!(session_webhook(&from_obj), session_webhook(&from_str));
    }

    #[test]
    fn parse_chatbot_text_plain_string() {
        let data = json!({
            "text": "plain",
            "senderId": "u1",
            "conversationId": "c1",
            "conversationType": 1,
            "session_webhook": "https://oapi.dingtalk.com/robot/sendBySession?session=x"
        });
        let env = native_from_chatbot(&ep(), &data).unwrap();
        assert_eq!(env.text, "plain");
        assert_eq!(
            env.meta.get("session_webhook").and_then(Value::as_str),
            Some("https://oapi.dingtalk.com/robot/sendBySession?session=x")
        );
        assert!(!env.is_group());
    }

    #[test]
    fn native_payload_extracts_session_webhook() {
        let env = native_from_chatbot(&ep(), &chatbot_object()).unwrap();
        assert_eq!(env.channel, "dingtalk");
        assert_eq!(env.sender_id, "$:LWCP_v1:$user");
        assert_eq!(env.sender_name, "Will");
        assert_eq!(env.chat_id(), "cidABC==");
        assert!(env.is_group());
        assert!(env.is_mentioned());
        assert_eq!(
            env.reply_url().as_deref(),
            Some("https://oapi.dingtalk.com/robot/sendBySession?session=tok")
        );
        assert_eq!(
            env.meta.get("msg_id").and_then(Value::as_str),
            Some("msgLICYe==")
        );
        assert_eq!(
            webhook_from_env(&env).as_deref(),
            env.reply_url().as_deref()
        );
    }

    #[test]
    fn missing_session_webhook_is_none() {
        let data = json!({
            "text": {"content": "hi"},
            "senderId": "u1",
            "conversationId": "c1",
            "conversationType": "1"
        });
        let env = native_from_chatbot(&ep(), &data).unwrap();
        assert!(webhook_from_env(&env).is_none());
        assert!(env.reply_url().is_none());
    }

    #[test]
    fn ws_url_appends_ticket() {
        assert_eq!(
            ws_url("wss://wss-open-connection.dingtalk.com:443/connect", "tick"),
            "wss://wss-open-connection.dingtalk.com:443/connect?ticket=tick"
        );
        assert_eq!(
            ws_url("wss://host/connect?foo=1", "tick"),
            "wss://host/connect?foo=1&ticket=tick"
        );
    }

    #[test]
    fn callback_ack_shape() {
        let raw = ack_json("mid-1", ACK_SUCCESS);
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["code"], 200);
        assert_eq!(v["message"], "OK");
        assert_eq!(v["headers"]["messageId"], "mid-1");
        assert_eq!(v["headers"]["contentType"], "application/json");
        assert_eq!(v["data"], ACK_SUCCESS);
    }

    #[test]
    fn open_connection_url() {
        assert_eq!(
            OPEN_URL,
            "https://api.dingtalk.com/v1.0/gateway/connections/open"
        );
        assert_eq!(BOT_TOPIC, "/v1.0/im/bot/messages/get");
    }
}
