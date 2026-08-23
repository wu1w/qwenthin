//! QwenPaw-compatible QR bind for dingtalk / feishu / qq / wechat / wecom.
//!
//! Official device-flow portals whitelist `source=QwenPaw` / `QWENPAW`.
//! Scan writes credentials into `ChannelEndpoint.extra`; message adapters
//! for those kinds are separate from this handshake.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde_json::{json, Value};

use super::catalog::supports_qr;

const PROJECT_NAME: &str = "QwenPaw";
const WECHAT_BASE: &str = "https://ilinkai.weixin.qq.com";
const WECOM_ORIGIN: &str = "https://work.weixin.qq.com";
const DINGTALK_API: &str = "https://oapi.dingtalk.com";
const FEISHU_ACCOUNTS: &str = "https://accounts.feishu.cn";
const LARK_ACCOUNTS: &str = "https://accounts.larksuite.com";
const FEISHU_REGISTER: &str = "/oauth/v1/app/registration";
const QQ_PORTAL: &str = "q.qq.com";

const DINGTALK_PENDING: &[&str] = &["WAITING", "CREATING", "PUBLISHING", "APPROVING"];
const DINGTALK_FAILED: &[&str] = &["FAIL", "EXPIRED"];

#[derive(Debug)]
pub enum QrError {
    UnknownKind(String),
    BadToken(String),
    Upstream(String),
    Internal(String),
}

impl std::fmt::Display for QrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKind(k) => write!(f, "{k} 不支持扫码绑定"),
            Self::BadToken(m) | Self::Upstream(m) | Self::Internal(m) => write!(f, "{m}"),
        }
    }
}

pub type QrResult<T> = std::result::Result<T, QrError>;

pub struct QrFetch {
    pub image: String,
    pub poll_token: String,
    pub scan_url: String,
}

pub struct QrPoll {
    pub status: String,
    pub credentials: BTreeMap<String, String>,
}

impl QrPoll {
    fn waiting() -> Self {
        Self {
            status: "waiting".into(),
            credentials: BTreeMap::new(),
        }
    }

    fn fail(reason: impl Into<String>) -> Self {
        let mut credentials = BTreeMap::new();
        credentials.insert("fail_reason".into(), reason.into());
        Self {
            status: "fail".into(),
            credentials,
        }
    }
}

pub async fn fetch_qrcode(kind: &str, domain: Option<&str>) -> QrResult<QrFetch> {
    let k = kind.to_ascii_lowercase();
    if !supports_qr(&k) {
        return Err(QrError::UnknownKind(k));
    }
    match k.as_str() {
        "wechat" => wechat_fetch().await,
        "wecom" => wecom_fetch().await,
        "dingtalk" => dingtalk_fetch().await,
        "feishu" => feishu_fetch(domain).await,
        "qq" => qq_fetch().await,
        other => Err(QrError::UnknownKind(other.into())),
    }
}

pub async fn poll_qrcode(kind: &str, token: &str, domain: Option<&str>) -> QrResult<QrPoll> {
    let k = kind.to_ascii_lowercase();
    if !supports_qr(&k) {
        return Err(QrError::UnknownKind(k));
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(QrError::BadToken("缺少 poll token".into()));
    }
    match k.as_str() {
        "wechat" => wechat_poll(token).await,
        "wecom" => wecom_poll(token).await,
        "dingtalk" => dingtalk_poll(token).await,
        "feishu" => feishu_poll(token, domain).await,
        "qq" => qq_poll(token).await,
        other => Err(QrError::UnknownKind(other.into())),
    }
}

fn http(timeout: Duration) -> QrResult<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()
        .map_err(|e| QrError::Internal(e.to_string()))
}

fn clean_str(v: &Value) -> String {
    v.as_str().map(str::trim).unwrap_or("").to_string()
}

fn qr_image_from_url(scan_url: &str) -> QrResult<String> {
    let code = qrcode::QrCode::new(scan_url.as_bytes())
        .map_err(|e| QrError::Internal(format!("二维码生成失败: {e}")))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(qrcode::render::svg::Color("#111827"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        STANDARD.encode(svg.as_bytes())
    ))
}

fn image_from_wechat(qrcode: &str, img: &str) -> QrResult<(String, String)> {
    let scan_url = if img.starts_with("http://") || img.starts_with("https://") {
        img.to_string()
    } else {
        format!("https://liteapp.weixin.qq.com/q/7GiQu1?qrcode={qrcode}&bot_type=3")
    };
    if img.starts_with("data:") {
        return Ok((img.to_string(), scan_url));
    }
    if !img.is_empty() && !img.starts_with("http") {
        return Ok((format!("data:image/png;base64,{img}"), scan_url));
    }
    Ok((qr_image_from_url(&scan_url)?, scan_url))
}

fn wechat_headers(bot_token: &str) -> reqwest::header::HeaderMap {
    let uin = (uuid::Uuid::new_v4().as_u128() & 0xffff_ffff) as u32;
    let uin_b64 = STANDARD.encode(uin.to_string());
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    h.insert(
        "AuthorizationType",
        reqwest::header::HeaderValue::from_static("ilink_bot_token"),
    );
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&uin_b64) {
        h.insert("X-WECHAT-UIN", v);
    }
    if !bot_token.is_empty() {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {bot_token}")) {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    h
}

async fn wechat_fetch() -> QrResult<QrFetch> {
    let client = http(Duration::from_secs(15))?;
    let resp = client
        .get(format!("{WECHAT_BASE}/ilink/bot/get_bot_qrcode"))
        .query(&[("bot_type", "3")])
        .headers(wechat_headers(""))
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("微信二维码请求失败: {e}")))?;
    let status = resp.status();
    let data: Value = resp
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("微信二维码响应无效: {e}")))?;
    if !status.is_success() {
        return Err(QrError::Upstream(format!(
            "微信二维码 HTTP {status}: {data}"
        )));
    }
    let qrcode = clean_str(&data["qrcode"]);
    let img = clean_str(&data["qrcode_img_content"]);
    if qrcode.is_empty() && img.is_empty() {
        return Err(QrError::Upstream("微信返回空二维码".into()));
    }
    let (image, scan_url) = image_from_wechat(&qrcode, &img)?;
    Ok(QrFetch {
        image,
        poll_token: qrcode,
        scan_url,
    })
}

async fn wechat_poll(token: &str) -> QrResult<QrPoll> {
    let client = http(Duration::from_secs(45))?;
    let resp = client
        .get(format!("{WECHAT_BASE}/ilink/bot/get_qrcode_status"))
        .query(&[("qrcode", token)])
        .headers(wechat_headers(""))
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("微信扫码状态失败: {e}")))?;
    let data: Value = resp
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("微信扫码状态无效: {e}")))?;
    let raw = clean_str(&data["status"]).to_ascii_lowercase();
    let bot_token = clean_str(&data["bot_token"]);
    let base_url = clean_str(&data["baseurl"]);
    if raw == "confirmed" && !bot_token.is_empty() {
        let mut credentials = BTreeMap::new();
        credentials.insert("bot_token".into(), bot_token);
        if !base_url.is_empty() {
            credentials.insert("base_url".into(), base_url);
        }
        return Ok(QrPoll {
            status: "success".into(),
            credentials,
        });
    }
    if raw == "expired" {
        return Ok(QrPoll {
            status: "expired".into(),
            credentials: BTreeMap::new(),
        });
    }
    if raw == "scanned" {
        return Ok(QrPoll {
            status: "scanned".into(),
            credentials: BTreeMap::new(),
        });
    }
    Ok(QrPoll::waiting())
}

fn millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn wecom_settings_json(html: &str) -> QrResult<Value> {
    let Some(i) = html.find("window.settings") else {
        return Err(QrError::Upstream("无法解析企微授权页 settings".into()));
    };
    let rest = &html[i..];
    let Some(brace) = rest.find('{') else {
        return Err(QrError::Upstream("企微授权页缺少 settings 对象".into()));
    };
    let start = i + brace;
    let slice = &html[start..];
    let end = slice
        .find("</")
        .or_else(|| slice.find(";\n"))
        .unwrap_or(slice.len().min(12_000));
    let mut raw = slice[..end].trim();
    if let Some(s) = raw.strip_suffix(';') {
        raw = s.trim();
    }
    serde_json::from_str(raw).map_err(|e| QrError::Upstream(format!("企微 settings JSON: {e}")))
}

async fn wecom_fetch() -> QrResult<QrFetch> {
    let state = uuid::Uuid::new_v4().simple().to_string();
    let url = format!(
        "{WECOM_ORIGIN}/ai/qc/gen?source={}&state={state}&timestamp={}",
        PROJECT_NAME.to_ascii_lowercase(),
        millis()
    );
    let client = http(Duration::from_secs(15))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("企微授权页失败: {e}")))?;
    let html = resp
        .text()
        .await
        .map_err(|e| QrError::Upstream(format!("企微授权页读取失败: {e}")))?;
    let settings = wecom_settings_json(&html)?;
    let scode = clean_str(&settings["scode"]);
    let auth_url = clean_str(&settings["auth_url"]);
    if scode.is_empty() || auth_url.is_empty() {
        return Err(QrError::Upstream("企微返回空 scode / auth_url".into()));
    }
    Ok(QrFetch {
        image: qr_image_from_url(&auth_url)?,
        poll_token: scode,
        scan_url: auth_url,
    })
}

async fn wecom_poll(token: &str) -> QrResult<QrPoll> {
    let client = http(Duration::from_secs(10))?;
    let resp = client
        .get(format!("{WECOM_ORIGIN}/ai/qc/query_result"))
        .query(&[("scode", token)])
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("企微扫码状态失败: {e}")))?;
    let result: Value = resp
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("企微扫码状态无效: {e}")))?;
    let data = &result["data"];
    let bot = &data["bot_info"];
    let bot_id = clean_str(&bot["botid"]);
    let secret = clean_str(&bot["secret"]);
    if !bot_id.is_empty() && !secret.is_empty() {
        let mut credentials = BTreeMap::new();
        credentials.insert("bot_id".into(), bot_id);
        credentials.insert("secret".into(), secret);
        return Ok(QrPoll {
            status: "success".into(),
            credentials,
        });
    }
    let status = clean_str(&data["status"]);
    if status.eq_ignore_ascii_case("fail") || status.eq_ignore_ascii_case("expired") {
        return Ok(QrPoll {
            status: status.to_ascii_lowercase(),
            credentials: BTreeMap::new(),
        });
    }
    Ok(QrPoll::waiting())
}

async fn dingtalk_fetch() -> QrResult<QrFetch> {
    let client = http(Duration::from_secs(15))?;
    let init: Value = client
        .post(format!("{DINGTALK_API}/app/registration/init"))
        .json(&json!({"source": "QWENPAW"}))
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("钉钉 init 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("钉钉 init 响应无效: {e}")))?;
    if init.get("errcode").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(QrError::Upstream(format!(
            "钉钉 init: {}",
            clean_str(&init["errmsg"])
        )));
    }
    let nonce = clean_str(&init["nonce"]);
    if nonce.is_empty() {
        return Err(QrError::Upstream("钉钉返回空 nonce".into()));
    }
    let begin: Value = client
        .post(format!("{DINGTALK_API}/app/registration/begin"))
        .json(&json!({"nonce": nonce}))
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("钉钉 begin 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("钉钉 begin 响应无效: {e}")))?;
    if begin.get("errcode").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(QrError::Upstream(format!(
            "钉钉 begin: {}",
            clean_str(&begin["errmsg"])
        )));
    }
    let device_code = clean_str(&begin["device_code"]);
    let scan_url = clean_str(&begin["verification_uri_complete"]);
    if device_code.is_empty() || scan_url.is_empty() {
        return Err(QrError::Upstream("钉钉返回空 device_code / URI".into()));
    }
    Ok(QrFetch {
        image: qr_image_from_url(&scan_url)?,
        poll_token: device_code,
        scan_url,
    })
}

pub fn dingtalk_map_status(data: &Value) -> QrPoll {
    let status = clean_str(&data["status"]).to_ascii_uppercase();
    let client_id = clean_str(&data["client_id"]);
    let client_secret = clean_str(&data["client_secret"]);
    if !client_id.is_empty() && !client_secret.is_empty() {
        let mut credentials = BTreeMap::new();
        credentials.insert("client_id".into(), client_id);
        credentials.insert("client_secret".into(), client_secret);
        return QrPoll {
            status: "success".into(),
            credentials,
        };
    }
    if DINGTALK_FAILED.iter().any(|s| *s == status) {
        let mut credentials = BTreeMap::new();
        let reason = clean_str(&data["fail_reason"]);
        if !reason.is_empty() {
            credentials.insert("fail_reason".into(), reason);
        }
        return QrPoll {
            status: if status == "EXPIRED" {
                "expired".into()
            } else {
                "fail".into()
            },
            credentials,
        };
    }
    let _ = DINGTALK_PENDING;
    QrPoll::waiting()
}

async fn dingtalk_poll(token: &str) -> QrResult<QrPoll> {
    let client = http(Duration::from_secs(10))?;
    let data: Value = client
        .post(format!("{DINGTALK_API}/app/registration/poll"))
        .json(&json!({"device_code": token}))
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("钉钉 poll 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("钉钉 poll 响应无效: {e}")))?;
    Ok(dingtalk_map_status(&data))
}

fn feishu_accounts(domain: Option<&str>) -> &'static str {
    if domain
        .map(str::trim)
        .unwrap_or("")
        .eq_ignore_ascii_case("lark")
    {
        LARK_ACCOUNTS
    } else {
        FEISHU_ACCOUNTS
    }
}

async fn feishu_fetch(domain: Option<&str>) -> QrResult<QrFetch> {
    let endpoint = format!("{}{FEISHU_REGISTER}", feishu_accounts(domain));
    let client = http(Duration::from_secs(15))?;
    let init: Value = client
        .post(&endpoint)
        .form(&[("action", "init")])
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("飞书 init 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("飞书 init 响应无效: {e}")))?;
    let methods = init["supported_auth_methods"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let ok = methods.iter().any(|m| m.as_str() == Some("client_secret"));
    if !ok {
        return Err(QrError::Upstream("飞书不支持 client_secret 授权".into()));
    }
    let begin: Value = client
        .post(&endpoint)
        .form(&[
            ("action", "begin"),
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id"),
        ])
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("飞书 begin 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("飞书 begin 响应无效: {e}")))?;
    let device_code = clean_str(&begin["device_code"]);
    let mut scan_url = clean_str(&begin["verification_uri_complete"]);
    if device_code.is_empty() || scan_url.is_empty() {
        return Err(QrError::Upstream(
            "飞书缺少 device_code 或二维码 URL".into(),
        ));
    }
    if scan_url.contains('?') {
        scan_url.push_str(&format!("&source={PROJECT_NAME}"));
    } else {
        scan_url.push_str(&format!("?source={PROJECT_NAME}"));
    }
    Ok(QrFetch {
        image: qr_image_from_url(&scan_url)?,
        poll_token: device_code,
        scan_url,
    })
}

async fn feishu_poll(token: &str, domain: Option<&str>) -> QrResult<QrPoll> {
    let endpoint = format!("{}{FEISHU_REGISTER}", feishu_accounts(domain));
    let client = http(Duration::from_secs(10))?;
    let data: Value = client
        .post(&endpoint)
        .form(&[("action", "poll"), ("device_code", token)])
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("飞书 poll 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("飞书 poll 响应无效: {e}")))?;
    let client_id = clean_str(&data["client_id"]);
    let client_secret = clean_str(&data["client_secret"]);
    if !client_id.is_empty() && !client_secret.is_empty() {
        let user = &data["user_info"];
        let mut credentials = BTreeMap::new();
        credentials.insert("app_id".into(), client_id);
        credentials.insert("app_secret".into(), client_secret);
        let open_id = clean_str(&user["open_id"]);
        if !open_id.is_empty() {
            credentials.insert("open_id".into(), open_id);
        }
        let brand = clean_str(&user["tenant_brand"]);
        credentials.insert(
            "tenant_brand".into(),
            if brand.is_empty() {
                "feishu".into()
            } else {
                brand
            },
        );
        return Ok(QrPoll {
            status: "success".into(),
            credentials,
        });
    }
    let error = clean_str(&data["error"]);
    if error == "expired_token" || error == "invalid_grant" {
        return Ok(QrPoll {
            status: "expired".into(),
            credentials: BTreeMap::new(),
        });
    }
    if error == "access_denied" {
        return Ok(QrPoll::fail("用户拒绝授权"));
    }
    if !error.is_empty() && error != "authorization_pending" && error != "slow_down" {
        return Ok(QrPoll::fail(error));
    }
    Ok(QrPoll::waiting())
}

pub fn generate_bind_key() -> String {
    let mut buf = [0u8; 32];
    buf[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    buf[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    STANDARD.encode(buf)
}

pub fn encode_poll_token(task_id: &str, aes_key: &str) -> String {
    let payload = json!({"task_id": task_id, "key": aes_key});
    URL_SAFE.encode(payload.to_string().as_bytes())
}

pub fn decode_poll_token(token: &str) -> QrResult<(String, String)> {
    let raw = URL_SAFE
        .decode(token.as_bytes())
        .or_else(|_| URL_SAFE_NO_PAD.decode(token.as_bytes()))
        .map_err(|_| QrError::BadToken("无效的 poll token".into()))?;
    let data: Value =
        serde_json::from_slice(&raw).map_err(|_| QrError::BadToken("无效的 poll token".into()))?;
    let task_id = clean_str(&data["task_id"]);
    let key = clean_str(&data["key"]);
    if task_id.is_empty() || key.is_empty() {
        return Err(QrError::BadToken("无效的 poll token".into()));
    }
    Ok((task_id, key))
}

pub fn decrypt_secret(encrypted_b64: &str, key_b64: &str) -> QrResult<String> {
    let key = STANDARD
        .decode(key_b64.trim())
        .map_err(|_| QrError::Internal("QQ AES key 无效".into()))?;
    if key.len() != 32 {
        return Err(QrError::Internal("QQ AES key 长度不是 256 位".into()));
    }
    let raw = STANDARD
        .decode(encrypted_b64.trim())
        .map_err(|_| QrError::Internal("QQ 密文无效".into()))?;
    if raw.len() < 28 {
        return Err(QrError::Internal("QQ 密文过短".into()));
    }
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| QrError::Internal(format!("AES: {e}")))?;
    let nonce = Nonce::from_slice(&raw[..12]);
    let pt = cipher
        .decrypt(nonce, raw[12..].as_ref())
        .map_err(|_| QrError::Internal("QQ 密钥解密失败".into()))?;
    String::from_utf8(pt).map_err(|_| QrError::Internal("QQ 密钥不是 UTF-8".into()))
}

async fn qq_fetch() -> QrResult<QrFetch> {
    let host = std::env::var("QQ_PORTAL_HOST").unwrap_or_else(|_| QQ_PORTAL.into());
    let aes_key = generate_bind_key();
    let client = http(Duration::from_secs(15))?;
    let t0 = std::time::Instant::now();
    let data: Value = client
        .post(format!("https://{host}/lite/create_bind_task"))
        .json(&json!({"key": aes_key}))
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("QQ create_bind_task 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("QQ create_bind_task 响应无效: {e}")))?;
    eprintln!(
        "q38 qr qq fetch {}ms retcode={}",
        t0.elapsed().as_millis(),
        data.get("retcode").and_then(Value::as_i64).unwrap_or(-1)
    );
    if data.get("retcode").and_then(Value::as_i64) != Some(0) {
        return Err(QrError::Upstream(format!(
            "QQ create_bind_task: {}",
            clean_str(&data["msg"])
        )));
    }
    let task_id = clean_str(&data["data"]["task_id"]);
    if task_id.is_empty() {
        return Err(QrError::Upstream("QQ 返回空 task_id".into()));
    }
    let scan_url = format!(
        "https://{host}/qqbot/openclaw/connect.html?task_id={task_id}&_wv=2&source={PROJECT_NAME}"
    );
    Ok(QrFetch {
        image: qr_image_from_url(&scan_url)?,
        poll_token: encode_poll_token(&task_id, &aes_key),
        scan_url,
    })
}

fn qq_rate_limited(data: &Value) -> bool {
    let retcode = data.get("retcode").and_then(Value::as_i64);
    if retcode == Some(30012) {
        return true;
    }
    clean_str(&data["msg"]).contains("频率过高")
}

pub fn qq_map_result(data: &Value, aes_key: &str) -> QrPoll {
    if qq_rate_limited(data) {
        return QrPoll::waiting();
    }
    if data.get("retcode").and_then(Value::as_i64) != Some(0) {
        let msg = clean_str(&data["msg"]);
        return QrPoll::fail(if msg.is_empty() {
            "QQ poll_bind_result 失败".into()
        } else {
            msg
        });
    }
    let result = &data["data"];
    let status = result.get("status").and_then(Value::as_i64).unwrap_or(-1);
    if status == 2 {
        let app_id = result
            .get("bot_appid")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string().trim_matches('"').to_string(),
            })
            .unwrap_or_default();
        let encrypted = clean_str(&result["bot_encrypt_secret"]);
        if app_id.is_empty() || encrypted.is_empty() || app_id == "0" {
            return QrPoll::fail("缺少 app_id 或密文");
        }
        let client_secret = match decrypt_secret(&encrypted, aes_key) {
            Ok(s) => s,
            Err(_) => return QrPoll::fail("密钥解密失败"),
        };
        let mut credentials = BTreeMap::new();
        credentials.insert("app_id".into(), app_id);
        credentials.insert("client_secret".into(), client_secret);
        let openid = result.get("user_openid").map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string().trim_matches('"').to_string(),
        });
        if let Some(id) = openid.filter(|s| !s.is_empty() && s != "null") {
            credentials.insert("user_openid".into(), id);
        }
        return QrPoll {
            status: "success".into(),
            credentials,
        };
    }
    if status == 3 {
        return QrPoll {
            status: "expired".into(),
            credentials: BTreeMap::new(),
        };
    }
    QrPoll::waiting()
}

async fn qq_poll(token: &str) -> QrResult<QrPoll> {
    let (task_id, aes_key) = decode_poll_token(token)?;
    let host = std::env::var("QQ_PORTAL_HOST").unwrap_or_else(|_| QQ_PORTAL.into());
    let t0 = std::time::Instant::now();
    let client = http(Duration::from_secs(10))?;
    let data: Value = client
        .post(format!("https://{host}/lite/poll_bind_result"))
        .json(&json!({"task_id": task_id}))
        .send()
        .await
        .map_err(|e| QrError::Upstream(format!("QQ poll_bind_result 失败: {e}")))?
        .json()
        .await
        .map_err(|e| QrError::Upstream(format!("QQ poll_bind_result 响应无效: {e}")))?;
    let mapped = qq_map_result(&data, &aes_key);
    eprintln!(
        "q38 qr qq poll {}ms retcode={} raw_status={} -> {}",
        t0.elapsed().as_millis(),
        data.get("retcode").and_then(Value::as_i64).unwrap_or(-1),
        data["data"]["status"],
        mapped.status
    );
    Ok(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;
    use serde_json::json;

    #[test]
    fn qq_rate_limit_keeps_waiting() {
        let p = qq_map_result(
            &json!({"msg":"轮询频率过高，请稍后再试","retcode":30012}),
            "unused",
        );
        assert_eq!(p.status, "waiting");
        assert!(p.credentials.is_empty());
    }

    #[test]
    fn qq_status_one_is_still_pending() {
        let p = qq_map_result(
            &json!({"retcode":0,"msg":"success","data":{"status":1,"bot_appid":"0","bot_encrypt_secret":"","user_openid":""}}),
            "unused",
        );
        assert_eq!(p.status, "waiting");
    }

    #[test]
    fn qq_token_roundtrip() {
        let tok = encode_poll_token("task-1", "aes-key");
        let (id, key) = decode_poll_token(&tok).unwrap();
        assert_eq!(id, "task-1");
        assert_eq!(key, "aes-key");
    }

    #[test]
    fn qq_decrypt_aes256_gcm() {
        let key = STANDARD.decode(generate_bind_key()).unwrap();
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let iv = [9u8; 12];
        let nonce = Nonce::from_slice(&iv);
        let ct = cipher.encrypt(nonce, b"qq-secret".as_ref()).unwrap();
        let mut raw = iv.to_vec();
        raw.extend_from_slice(&ct);
        let got = decrypt_secret(&STANDARD.encode(raw), &STANDARD.encode(&key)).unwrap();
        assert_eq!(got, "qq-secret");
    }

    #[test]
    fn dingtalk_waiting_until_both_creds() {
        let waiting = dingtalk_map_status(&json!({"status": "WAITING"}));
        assert_eq!(waiting.status, "waiting");
        let approving = dingtalk_map_status(&json!({
            "status": "APPROVING",
            "client_id": "id-1",
            "client_secret": "sec-1"
        }));
        assert_eq!(approving.status, "success");
        assert_eq!(approving.credentials.get("client_id").unwrap(), "id-1");
        let expired = dingtalk_map_status(&json!({"status": "EXPIRED"}));
        assert_eq!(expired.status, "expired");
        let null_id = dingtalk_map_status(&json!({
            "status": "SUCCESS",
            "client_id": null,
            "client_secret": "x"
        }));
        assert_eq!(null_id.status, "waiting");
    }

    #[test]
    fn wecom_settings_from_html() {
        let html = r#"<script>window.settings = {"scode":"abc","auth_url":"https://work.weixin.qq.com/qr"};</script>"#;
        let v = wecom_settings_json(html).unwrap();
        assert_eq!(v["scode"], "abc");
    }

    #[test]
    fn qr_svg_data_uri() {
        let img = qr_image_from_url("https://example.com/scan").unwrap();
        assert!(img.starts_with("data:image/svg+xml;base64,"));
    }
}
