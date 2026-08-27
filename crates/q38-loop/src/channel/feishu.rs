//! Feishu / Lark bot adapter. Receive via long-connection WS, send via Open API HTTP.
//!
//! No `lark-oapi` crate: tenant token + HTTP send + WS. Official long-connection
//! frames are pbbp2 protobuf wrapping a JSON `im.message.receive_v1` payload.
//! Text JSON frames are also accepted. Never log `app_secret` or WS tickets.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::error::{Error, Result};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::outbound::parts_to_text;
use super::ChannelEndpoint;

const FEISHU_BASE: &str = "https://open.feishu.cn";
const LARK_BASE: &str = "https://open.larksuite.com";
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);
const RECONNECT_WAIT: Duration = Duration::from_secs(2);
const PING_INTERVAL: Duration = Duration::from_secs(30);

const PB_CONTROL: i32 = 0;
const PB_DATA: i32 = 1;

type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

static TOKEN: StdMutex<Option<CachedToken>> = StdMutex::new(None);

struct CachedToken {
    app_id: String,
    base: String,
    token: String,
    until: Instant,
}

#[derive(Clone, Default)]
struct PbHeader {
    key: String,
    value: String,
}

#[derive(Clone, Default)]
struct PbFrame {
    seq_id: u64,
    log_id: u64,
    service: i32,
    method: i32,
    headers: Vec<PbHeader>,
    payload_encoding: String,
    payload_type: String,
    payload: Vec<u8>,
    log_id_new: String,
}

impl PbFrame {
    fn header(&self, key: &str) -> &str {
        self.headers
            .iter()
            .find(|h| h.key.eq_ignore_ascii_case(key))
            .map(|h| h.value.as_str())
            .unwrap_or("")
    }

    fn ping(service: i32) -> Self {
        Self {
            service,
            method: PB_CONTROL,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            ..Self::default()
        }
    }

    fn to_pong(&self) -> Self {
        let mut f = self.clone();
        f.method = PB_CONTROL;
        if let Some(h) = f
            .headers
            .iter_mut()
            .find(|h| h.key.eq_ignore_ascii_case("type"))
        {
            h.value = "pong".into();
        } else {
            f.headers.push(PbHeader {
                key: "type".into(),
                value: "pong".into(),
            });
        }
        f
    }

    fn to_ack(&self) -> Self {
        let mut f = self.clone();
        f.payload = br#"{"code":200}"#.to_vec();
        if !f
            .headers
            .iter()
            .any(|h| h.key.eq_ignore_ascii_case("biz_rt"))
        {
            f.headers.push(PbHeader {
                key: "biz_rt".into(),
                value: "0".into(),
            });
        }
        f
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        pb_u64(1, self.seq_id, &mut out);
        pb_u64(2, self.log_id, &mut out);
        pb_i32(3, self.service, &mut out);
        pb_i32(4, self.method, &mut out);
        for h in &self.headers {
            let mut inner = Vec::new();
            pb_bytes(1, h.key.as_bytes(), &mut inner);
            pb_bytes(2, h.value.as_bytes(), &mut inner);
            pb_bytes(5, &inner, &mut out);
        }
        if !self.payload_encoding.is_empty() {
            pb_bytes(6, self.payload_encoding.as_bytes(), &mut out);
        }
        if !self.payload_type.is_empty() {
            pb_bytes(7, self.payload_type.as_bytes(), &mut out);
        }
        if !self.payload.is_empty() {
            pb_bytes(8, &self.payload, &mut out);
        }
        if !self.log_id_new.is_empty() {
            pb_bytes(9, self.log_id_new.as_bytes(), &mut out);
        }
        out
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        let mut f = Self::default();
        let mut i = 0usize;
        while i < buf.len() {
            let key = pb_varint(buf, &mut i)?;
            let field = (key >> 3) as u32;
            let wire = (key & 7) as u32;
            match (field, wire) {
                (1, 0) => f.seq_id = pb_varint(buf, &mut i)?,
                (2, 0) => f.log_id = pb_varint(buf, &mut i)?,
                (3, 0) => f.service = pb_varint(buf, &mut i)? as i32,
                (4, 0) => f.method = pb_varint(buf, &mut i)? as i32,
                (5, 2) => {
                    let b = pb_len(buf, &mut i)?;
                    f.headers.push(decode_header(b)?);
                }
                (6, 2) => f.payload_encoding = pb_string(buf, &mut i)?,
                (7, 2) => f.payload_type = pb_string(buf, &mut i)?,
                (8, 2) => f.payload = pb_len(buf, &mut i)?.to_vec(),
                (9, 2) => f.log_id_new = pb_string(buf, &mut i)?,
                (_, w) => pb_skip(buf, &mut i, w)?,
            }
        }
        Some(f)
    }
}

pub fn credentials(ep: &ChannelEndpoint) -> Option<(String, String)> {
    let app_id = extra(ep, "app_id");
    let app_id = if app_id.is_empty() {
        extra(ep, "client_id")
    } else {
        app_id
    };
    let secret = extra(ep, "app_secret");
    let secret = if secret.is_empty() {
        extra(ep, "client_secret")
    } else {
        secret
    };
    if app_id.is_empty() || secret.is_empty() {
        None
    } else {
        Some((app_id, secret))
    }
}

fn open_base(ep: &ChannelEndpoint) -> &'static str {
    let domain = extra(ep, "domain").to_ascii_lowercase();
    let brand = extra(ep, "tenant_brand").to_ascii_lowercase();
    if domain.contains("lark") || brand == "lark" {
        LARK_BASE
    } else {
        FEISHU_BASE
    }
}

pub async fn run_ws(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some((app_id, secret)) = credentials(&ep) else {
        return Err(Error::msg(
            "feishu: extra.app_id and extra.app_secret required",
        ));
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let base = open_base(&ep);
    eprintln!("q38 feishu gateway starting app_id={app_id} base={base}");
    loop {
        match run_once(&http, &ep, &mgr, &app_id, &secret, base).await {
            Ok(()) => eprintln!("q38 feishu: socket closed, reconnecting"),
            Err(e) => eprintln!("q38 feishu: {e}; retry in 2s"),
        }
        tokio::time::sleep(RECONNECT_WAIT).await;
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
    let Some((app_id, secret)) = credentials(ep) else {
        return Err(Error::msg("feishu send: missing credentials"));
    };
    let text = parts_to_text(parts);
    if text.trim().is_empty() {
        return Ok(());
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let base = open_base(ep);
    send_text(&http, env, base, &app_id, &secret, &text).await
}

async fn run_once(
    http: &reqwest::Client,
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    app_id: &str,
    secret: &str,
    base: &str,
) -> Result<()> {
    let url = ws_endpoint(http, base, app_id, secret).await?;
    let service_id = query_i32(&url, "service_id");
    let host = url.split('?').next().unwrap_or("wss");
    eprintln!("q38 feishu connecting {host}");
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), connect_async(&url))
        .await
        .map_err(|_| Error::msg("feishu ws connect timeout"))?
        .map_err(|e| {
            let msg = e.to_string();
            let safe = msg.split('?').next().unwrap_or("ws error");
            Error::msg(format!("feishu ws connect: {safe}"))
        })?;
    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));
    let ping_w = write.clone();
    let ping = tokio::spawn(async move {
        let mut tick = tokio::time::interval(PING_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut w = ping_w.lock().await;
            let pb = PbFrame::ping(service_id).encode();
            if w.send(Message::Ping(Vec::new().into())).await.is_err() {
                break;
            }
            if w.send(Message::Binary(pb.into())).await.is_err() {
                break;
            }
        }
    });
    let mut frags: HashMap<String, Vec<Option<Vec<u8>>>> = HashMap::new();
    while let Some(frame) = read.next().await {
        let frame = frame.map_err(|e| Error::msg(format!("feishu ws: {e}")))?;
        match frame {
            Message::Ping(p) => {
                let _ = write.lock().await.send(Message::Pong(p)).await;
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Text(text) => {
                handle_text(ep, mgr, &write, &text).await;
            }
            Message::Binary(bin) => {
                handle_binary(ep, mgr, &write, &bin, &mut frags).await;
            }
            _ => {}
        }
    }
    ping.abort();
    Ok(())
}

async fn handle_text(
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    write: &Arc<Mutex<WsWrite>>,
    text: &str,
) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        eprintln!("q38 feishu: non-json frame, skipping");
        return;
    };
    if is_ping(&v) {
        let _ = write
            .lock()
            .await
            .send(Message::Text(json!({"type": "pong"}).to_string().into()))
            .await;
        return;
    }
    if let Some(ack) = json_ack(&v) {
        let _ = write.lock().await.send(Message::Text(ack.into())).await;
    }
    ingest_json(ep, mgr, &v).await;
}

async fn handle_binary(
    ep: &ChannelEndpoint,
    mgr: &ChannelManager,
    write: &Arc<Mutex<WsWrite>>,
    bin: &[u8],
    frags: &mut HashMap<String, Vec<Option<Vec<u8>>>>,
) {
    if bin.first().copied() == Some(b'{') {
        if let Ok(text) = std::str::from_utf8(bin) {
            handle_text(ep, mgr, write, text).await;
            return;
        }
    }
    let Some(frame) = PbFrame::decode(bin) else {
        eprintln!("q38 feishu: non-json frame, skipping");
        return;
    };
    let kind = frame.header("type");
    if frame.method == PB_CONTROL || kind.eq_ignore_ascii_case("ping") {
        if kind.eq_ignore_ascii_case("ping") {
            let _ = write
                .lock()
                .await
                .send(Message::Binary(frame.to_pong().encode().into()))
                .await;
        }
        return;
    }
    if frame.method == PB_DATA || kind.eq_ignore_ascii_case("event") {
        let _ = write
            .lock()
            .await
            .send(Message::Binary(frame.to_ack().encode().into()))
            .await;
        let Some(payload) = assemble(&frame, frags) else {
            return;
        };
        match serde_json::from_slice::<Value>(&payload) {
            Ok(v) => ingest_json(ep, mgr, &v).await,
            Err(_) => eprintln!("q38 feishu: non-json frame, skipping"),
        }
    }
}

async fn ingest_json(ep: &ChannelEndpoint, mgr: &ChannelManager, v: &Value) {
    if let Some(env) = native_from_envelope(ep, v) {
        if let Err(e) = mgr.ingest(env).await {
            eprintln!("q38 feishu ingest: {e}");
        }
    }
}

fn json_ack(v: &Value) -> Option<String> {
    let mid = header_str(v, "message_id");
    let need = boolish(&v["need_ack"]) || boolish(&v["headers"]["need_ack"]) || !mid.is_empty();
    if !need {
        return None;
    }
    Some(json!({"type": "ack", "code": 200, "message_id": mid}).to_string())
}

async fn ws_endpoint(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
) -> Result<String> {
    let token = tenant_token(http, base, app_id, secret).await?;
    let open_api = format!("{base}/open-apis/callback/ws/endpoint");
    let bare = format!("{base}/callback/ws/endpoint");
    match post_ws_url(http, &open_api, Some(&token), json!({})).await {
        Ok(url) => return Ok(url),
        Err(e) => eprintln!("q38 feishu: open-apis ws endpoint: {e}"),
    }
    match post_ws_url(http, &bare, Some(&token), json!({})).await {
        Ok(url) => return Ok(url),
        Err(e) => eprintln!("q38 feishu: callback ws endpoint: {e}"),
    }
    // Official SDK path (AppID/AppSecret). This is what actually returns the
    // pbbp2 long-connection URL used by lark-oapi / QwenPaw / Hermes.
    post_ws_url(
        http,
        &bare,
        None,
        json!({"AppID": app_id, "AppSecret": secret}),
    )
    .await
}

async fn post_ws_url(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    body: Value,
) -> Result<String> {
    let mut req = http.post(url).json(&body);
    if let Some(tok) = bearer {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    let resp = req.send().await?;
    let status = resp.status();
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = data.get("code").cloned().unwrap_or(Value::Null);
        let msg = js_str(&data["msg"]);
        return Err(Error::msg(format!(
            "feishu ws endpoint HTTP {status} code={code} msg={msg}"
        )));
    }
    if let Some(n) = data.get("code").and_then(Value::as_i64) {
        if n != 0 {
            return Err(Error::msg(format!(
                "feishu ws endpoint code={n} msg={}",
                js_str(&data["msg"])
            )));
        }
    }
    pick_ws_url(&data).ok_or_else(|| Error::msg("feishu ws endpoint: no url"))
}

fn pick_ws_url(data: &Value) -> Option<String> {
    for v in [
        &data["data"]["url"],
        &data["data"]["URL"],
        &data["url"],
        &data["URL"],
        &data["data"]["ws_url"],
    ] {
        let s = js_str(v);
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

async fn tenant_token(
    http: &reqwest::Client,
    base: &str,
    app_id: &str,
    secret: &str,
) -> Result<String> {
    if let Ok(g) = TOKEN.lock() {
        if let Some(c) = g.as_ref() {
            if c.app_id == app_id && c.base == base && Instant::now() < c.until {
                return Ok(c.token.clone());
            }
        }
    }
    let url = format!("{base}/open-apis/auth/v3/tenant_access_token/internal");
    let data: Value = http
        .post(url)
        .json(&json!({"app_id": app_id, "app_secret": secret}))
        .send()
        .await?
        .json()
        .await?;
    let token = js_str(&data["tenant_access_token"]);
    if token.is_empty() {
        return Err(Error::msg(format!(
            "feishu token code={} msg={}",
            data.get("code").unwrap_or(&Value::Null),
            js_str(&data["msg"])
        )));
    }
    if let Ok(mut g) = TOKEN.lock() {
        *g = Some(CachedToken {
            app_id: app_id.to_string(),
            base: base.to_string(),
            token: token.clone(),
            until: Instant::now() + TOKEN_TTL,
        });
    }
    Ok(token)
}

fn clear_token(app_id: &str) {
    if let Ok(mut g) = TOKEN.lock() {
        if g.as_ref().is_some_and(|c| c.app_id == app_id) {
            *g = None;
        }
    }
}

async fn send_text(
    http: &reqwest::Client,
    env: &NativePayload,
    base: &str,
    app_id: &str,
    secret: &str,
    text: &str,
) -> Result<()> {
    let id_type = receive_id_type(env);
    let receive_id = receive_id(env, &id_type);
    if receive_id.is_empty() {
        return Err(Error::msg("feishu send: missing chat_id / open_id"));
    }
    let content = serde_json::to_string(&json!({"text": text})).unwrap_or_else(|_| "{}".into());
    let body = json!({
        "receive_id": receive_id,
        "msg_type": "text",
        "content": content,
        "uuid": uuid::Uuid::new_v4().to_string(),
    });
    let mut last = Error::msg("feishu send failed");
    for _ in 0..2 {
        let token = tenant_token(http, base, app_id, secret).await?;
        let url = format!("{base}/open-apis/im/v1/messages?receive_id_type={id_type}");
        let resp = match http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
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
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        let code = data.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if status.is_success() && code == 0 {
            return Ok(());
        }
        if status.as_u16() == 401 || code == 99991663 {
            clear_token(app_id);
            last = Error::msg(format!("feishu send unauthorized code={code}"));
            continue;
        }
        last = Error::msg(format!(
            "feishu send HTTP {status} code={code} msg={}",
            js_str(&data["msg"])
        ));
        break;
    }
    Err(last)
}

fn receive_id_type(env: &NativePayload) -> String {
    let t = js_str(env.meta.get("receive_id_type").unwrap_or(&Value::Null));
    if !t.is_empty() {
        return t;
    }
    if env.is_group() {
        "chat_id".into()
    } else {
        "open_id".into()
    }
}

fn receive_id(env: &NativePayload, id_type: &str) -> String {
    if id_type == "open_id" {
        let id = js_str(env.meta.get("receive_id").unwrap_or(&Value::Null));
        if !id.is_empty() {
            return id;
        }
        return env.sender_id.clone();
    }
    env.chat_id()
}

fn native_from_envelope(ep: &ChannelEndpoint, v: &Value) -> Option<NativePayload> {
    if is_ping(v) {
        return None;
    }
    let et = envelope_event_type(v);
    if et.to_ascii_lowercase().contains("card") {
        return None;
    }
    let event = extract_event(v)?;
    native_from_event(ep, &event)
}

fn native_from_event(ep: &ChannelEndpoint, event: &Value) -> Option<NativePayload> {
    let message = &event["message"];
    let sender = &event["sender"];
    if message.is_null() {
        return None;
    }
    let sender_type = js_str(&sender["sender_type"]);
    if sender_type == "app" || sender_type == "bot" {
        return None;
    }
    let open_id = first_str(&[
        &sender["sender_id"]["open_id"],
        &sender["sender_id"]["user_id"],
        &sender["open_id"],
    ]);
    if open_id.is_empty() {
        return None;
    }
    let chat_id = js_str(&message["chat_id"]);
    let chat_type = js_str(&message["chat_type"]);
    let is_group = chat_type.eq_ignore_ascii_case("group");
    let msg_type = js_str(&message["message_type"]);
    let mut text = parse_text_content(&message["content"]);
    if text.is_empty() {
        text = feishu_nontext_caption(&msg_type, &message["content"]);
    }
    if text.is_empty() {
        return None;
    }
    let message_id = js_str(&message["message_id"]);
    let mentioned = !is_group || mentions_present(&message["mentions"]) || text.contains("@_all");
    let receive_id_type = if is_group { "chat_id" } else { "open_id" };
    let receive_id = if is_group {
        chat_id.clone()
    } else {
        open_id.clone()
    };
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "feishu".into()
        } else {
            ep.kind.clone()
        },
        sender_id: open_id,
        sender_name: js_str(&sender["name"]),
        content_parts: vec![ContentPart::text(&text)],
        text,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("message_id".into(), json!(message_id));
    env.meta
        .insert("receive_id_type".into(), json!(receive_id_type));
    env.meta.insert("receive_id".into(), json!(receive_id));
    env.meta.insert("is_mentioned".into(), json!(mentioned));
    Some(env)
}

fn feishu_nontext_caption(msg_type: &str, content: &Value) -> String {
    let kind = msg_type.trim().to_ascii_lowercase();
    match kind.as_str() {
        "image" | "sticker" => "[图片]".into(),
        "audio" => "[语音]".into(),
        "media" | "video" => "[视频]".into(),
        "file" => {
            let name = parse_file_name(content);
            if name.is_empty() {
                "[文件]".into()
            } else {
                format!("[文件] {name}")
            }
        }
        _ => String::new(),
    }
}

fn parse_file_name(content: &Value) -> String {
    let from_obj = |v: &Value| {
        let n = js_str(&v["file_name"]);
        if n.is_empty() {
            js_str(&v["name"])
        } else {
            n
        }
    };
    match content {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .map(|v| from_obj(&v))
            .unwrap_or_default(),
        Value::Object(_) => from_obj(content),
        _ => String::new(),
    }
}

#[cfg(test)]
fn parse_text_content_str(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return parse_text_content(&v);
    }
    raw.trim().to_string()
}

fn parse_text_content(content: &Value) -> String {
    match content {
        Value::String(s) => {
            if let Ok(v) = serde_json::from_str::<Value>(s) {
                let t = js_str(&v["text"]);
                if !t.is_empty() {
                    return t;
                }
                if v.is_object() {
                    return String::new();
                }
            }
            s.trim().to_string()
        }
        Value::Object(_) => js_str(&content["text"]),
        _ => String::new(),
    }
}

fn extract_event(v: &Value) -> Option<Value> {
    if looks_like_im_event(v) {
        return Some(v.clone());
    }
    for key in ["event", "data", "payload"] {
        match v.get(key) {
            Some(Value::String(s)) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    if let Some(ev) = extract_event(&parsed) {
                        return Some(ev);
                    }
                }
            }
            Some(inner) => {
                if let Some(ev) = extract_event(inner) {
                    return Some(ev);
                }
            }
            None => {}
        }
    }
    None
}

fn looks_like_im_event(v: &Value) -> bool {
    v.get("message").is_some() && (v.get("sender").is_some() || v.get("sender_id").is_some())
}

fn envelope_event_type(v: &Value) -> String {
    first_str(&[
        &v["header"]["event_type"],
        &v["event"]["header"]["event_type"],
        &v["event_type"],
        &v["type"],
    ])
}

fn is_ping(v: &Value) -> bool {
    let t = js_str(&v["type"]);
    t.eq_ignore_ascii_case("ping") || v.get("ping").is_some()
}

fn mentions_present(mentions: &Value) -> bool {
    mentions.as_array().is_some_and(|a| !a.is_empty())
}

fn extra(ep: &ChannelEndpoint, key: &str) -> String {
    ep.extra
        .get(key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn js_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
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

fn boolish(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Value::Number(n) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn header_str(v: &Value, key: &str) -> String {
    let h = &v["headers"];
    if let Some(x) = h.get(key) {
        let s = js_str(x);
        if !s.is_empty() {
            return s;
        }
    }
    if let Some(arr) = h.as_array() {
        for item in arr {
            let k = first_str(&[&item["key"], &item["Key"]]);
            if k.eq_ignore_ascii_case(key) {
                return first_str(&[&item["value"], &item["Value"]]);
            }
        }
    }
    String::new()
}

fn query_i32(url: &str, key: &str) -> i32 {
    let Some(q) = url.split_once('?').map(|(_, q)| q) else {
        return 0;
    };
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return v.parse().unwrap_or(0);
            }
        }
    }
    0
}

fn assemble(frame: &PbFrame, frags: &mut HashMap<String, Vec<Option<Vec<u8>>>>) -> Option<Vec<u8>> {
    let sum: usize = frame.header("sum").parse().unwrap_or(1);
    let seq: usize = frame.header("seq").parse().unwrap_or(0);
    let msg_id = frame.header("message_id").to_string();
    if sum <= 1 || msg_id.is_empty() {
        return Some(frame.payload.clone());
    }
    let entry = frags
        .entry(msg_id.clone())
        .or_insert_with(|| vec![None; sum]);
    if entry.len() != sum {
        *entry = vec![None; sum];
    }
    if seq >= sum {
        return Some(frame.payload.clone());
    }
    entry[seq] = Some(frame.payload.clone());
    if entry.iter().all(|s| s.is_some()) {
        let full: Vec<u8> = entry
            .iter()
            .flat_map(|s| s.as_deref().unwrap_or(&[]))
            .copied()
            .collect();
        frags.remove(&msg_id);
        Some(full)
    } else {
        None
    }
}

fn decode_header(buf: &[u8]) -> Option<PbHeader> {
    let mut h = PbHeader::default();
    let mut i = 0usize;
    while i < buf.len() {
        let key = pb_varint(buf, &mut i)?;
        let field = (key >> 3) as u32;
        let wire = (key & 7) as u32;
        match (field, wire) {
            (1, 2) => h.key = pb_string(buf, &mut i)?,
            (2, 2) => h.value = pb_string(buf, &mut i)?,
            (_, w) => pb_skip(buf, &mut i, w)?,
        }
    }
    Some(h)
}

fn pb_u64(field: u32, v: u64, out: &mut Vec<u8>) {
    pb_put_varint(u64::from(field) << 3, out);
    pb_put_varint(v, out);
}

fn pb_i32(field: u32, v: i32, out: &mut Vec<u8>) {
    pb_u64(field, v as u64, out);
}

fn pb_bytes(field: u32, b: &[u8], out: &mut Vec<u8>) {
    pb_put_varint((u64::from(field) << 3) | 2, out);
    pb_put_varint(b.len() as u64, out);
    out.extend_from_slice(b);
}

fn pb_put_varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
            out.push(b);
        } else {
            out.push(b);
            break;
        }
    }
}

fn pb_varint(buf: &[u8], i: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    while *i < buf.len() {
        let b = buf[*i];
        *i += 1;
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn pb_len<'a>(buf: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    let n = pb_varint(buf, i)? as usize;
    if *i + n > buf.len() {
        return None;
    }
    let s = &buf[*i..*i + n];
    *i += n;
    Some(s)
}

fn pb_string(buf: &[u8], i: &mut usize) -> Option<String> {
    let b = pb_len(buf, i)?;
    Some(String::from_utf8_lossy(b).into_owned())
}

fn pb_skip(buf: &[u8], i: &mut usize, wire: u32) -> Option<()> {
    match wire {
        0 => {
            pb_varint(buf, i)?;
            Some(())
        }
        1 => {
            *i += 8;
            (*i <= buf.len()).then_some(())
        }
        2 => {
            pb_len(buf, i)?;
            Some(())
        }
        5 => {
            *i += 4;
            (*i <= buf.len()).then_some(())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep_with(pairs: &[(&str, &str)]) -> ChannelEndpoint {
        let mut ep = ChannelEndpoint::default();
        ep.kind = "feishu".into();
        for (k, v) in pairs {
            ep.extra.insert((*k).into(), (*v).into());
        }
        ep
    }

    #[test]
    fn credentials_app_id_secret() {
        let ep = ep_with(&[("app_id", "cli_a"), ("app_secret", "s3cret")]);
        assert_eq!(credentials(&ep), Some(("cli_a".into(), "s3cret".into())));
    }

    #[test]
    fn credentials_qr_client_id_maps_to_app_id() {
        let ep = ep_with(&[("client_id", "cli_qr"), ("app_secret", "s")]);
        assert_eq!(credentials(&ep).unwrap().0, "cli_qr");
    }

    #[test]
    fn credentials_missing_secret() {
        let ep = ep_with(&[("app_id", "cli_a")]);
        assert!(credentials(&ep).is_none());
    }

    #[test]
    fn domain_defaults_feishu() {
        let ep = ep_with(&[]);
        assert_eq!(open_base(&ep), FEISHU_BASE);
    }

    #[test]
    fn domain_lark_from_domain() {
        let ep = ep_with(&[("domain", "lark")]);
        assert_eq!(open_base(&ep), LARK_BASE);
        let ep = ep_with(&[("domain", "larksuite")]);
        assert_eq!(open_base(&ep), LARK_BASE);
    }

    #[test]
    fn domain_lark_from_tenant_brand() {
        let ep = ep_with(&[("tenant_brand", "lark")]);
        assert_eq!(open_base(&ep), LARK_BASE);
        let ep = ep_with(&[("tenant_brand", "feishu")]);
        assert_eq!(open_base(&ep), FEISHU_BASE);
    }

    #[test]
    fn parse_text_content_json() {
        assert_eq!(parse_text_content_str(r#"{"text":"hello"}"#), "hello");
        assert_eq!(parse_text_content_str(""), "");
        assert_eq!(parse_text_content_str("plain"), "plain");
    }

    #[test]
    fn native_p2p_text_event() {
        let ep = ep_with(&[]);
        let v = json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {
                    "sender_id": {"open_id": "ou_user"},
                    "sender_type": "user"
                },
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_dm",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"hi bot\"}"
                }
            }
        });
        let env = native_from_envelope(&ep, &v).unwrap();
        assert_eq!(env.channel, "feishu");
        assert_eq!(env.sender_id, "ou_user");
        assert_eq!(env.text, "hi bot");
        assert_eq!(env.meta["chat_id"], json!("oc_dm"));
        assert_eq!(env.meta["is_group"], json!(false));
        assert_eq!(env.meta["message_id"], json!("om_1"));
        assert_eq!(env.meta["receive_id_type"], json!("open_id"));
        assert_eq!(env.meta["receive_id"], json!("ou_user"));
        assert_eq!(receive_id_type(&env), "open_id");
        assert_eq!(receive_id(&env, "open_id"), "ou_user");
    }

    #[test]
    fn native_group_uses_chat_id() {
        let ep = ep_with(&[]);
        let v = json!({
            "event": {
                "header": {"event_type": "im.message.receive_v1"},
                "sender": {"sender_id": {"open_id": "ou_2"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_g",
                    "chat_id": "oc_group",
                    "chat_type": "group",
                    "message_type": "text",
                    "content": {"text": "hey"},
                    "mentions": [{"id": {"open_id": "ou_bot"}}]
                }
            }
        });
        let env = native_from_envelope(&ep, &v).unwrap();
        assert_eq!(env.meta["is_group"], json!(true));
        assert_eq!(env.meta["receive_id_type"], json!("chat_id"));
        assert_eq!(env.meta["receive_id"], json!("oc_group"));
        assert_eq!(env.meta["is_mentioned"], json!(true));
        assert_eq!(receive_id(&env, "chat_id"), "oc_group");
    }

    #[test]
    fn native_ingests_image() {
        let ep = ep_with(&[]);
        let v = json!({
            "event": {
                "sender": {"sender_id": {"open_id": "ou_u"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_i",
                    "chat_id": "oc_dm",
                    "chat_type": "p2p",
                    "message_type": "image",
                    "content": "{\"image_key\":\"img_x\"}"
                }
            }
        });
        let env = native_from_envelope(&ep, &v).expect("image");
        assert_eq!(env.text, "[图片]");
    }

    #[test]
    fn skips_bot_sender() {
        let ep = ep_with(&[]);
        let v = json!({
            "event": {
                "sender": {"sender_id": {"open_id": "ou_bot"}, "sender_type": "app"},
                "message": {
                    "chat_id": "oc_x",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"nope\"}"
                }
            }
        });
        assert!(native_from_envelope(&ep, &v).is_none());
    }

    #[test]
    fn protobuf_ping_roundtrip() {
        let f = PbFrame::ping(42);
        let bytes = f.encode();
        let d = PbFrame::decode(&bytes).unwrap();
        assert_eq!(d.service, 42);
        assert_eq!(d.method, PB_CONTROL);
        assert_eq!(d.header("type"), "ping");
        assert_eq!(d.seq_id, 0);
    }

    #[test]
    fn protobuf_event_payload_is_json() {
        let payload = serde_json::to_vec(&json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_z"}, "sender_type": "user"},
                "message": {
                    "message_id": "om_z",
                    "chat_id": "oc_z",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"from pb\"}"
                }
            }
        }))
        .unwrap();
        let frame = PbFrame {
            method: PB_DATA,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "event".into(),
            }],
            payload,
            ..PbFrame::default()
        };
        let d = PbFrame::decode(&frame.encode()).unwrap();
        assert_eq!(d.method, PB_DATA);
        let v: Value = serde_json::from_slice(&d.payload).unwrap();
        let ep = ep_with(&[]);
        let env = native_from_envelope(&ep, &v).unwrap();
        assert_eq!(env.text, "from pb");
        assert_eq!(env.sender_id, "ou_z");
    }

    #[test]
    fn json_ack_when_need_ack() {
        let v = json!({"type": "event", "headers": {"need_ack": true, "message_id": "m1"}});
        let ack = json_ack(&v).unwrap();
        assert!(ack.contains("m1"));
        assert!(ack.contains("ack"));
    }
}
