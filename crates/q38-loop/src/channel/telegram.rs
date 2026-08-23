//! Telegram Bot API long-poll. Wash of QwenPaw `telegram/channel.py` inbound
//! shape: native dict with `content_parts`, session key `telegram:dm:{id}`.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::media::{data_uri, MAX_INLINE_MEDIA_BYTES};

use super::envelope::{ContentPart, NativePayload};
use super::manager::ChannelManager;
use super::ChannelEndpoint;

const API: &str = "https://api.telegram.org";

pub fn token(ep: &ChannelEndpoint) -> Option<String> {
    ep.extra
        .get("bot_token")
        .cloned()
        .or_else(|| ep.extra.get("token").cloned())
        .or_else(|| std::env::var("TELEGRAM_BOT_TOKEN").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn run_long_poll(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let Some(token) = token(&ep) else {
        return Err(Error::msg(
            "telegram: set extra.bot_token or TELEGRAM_BOT_TOKEN",
        ));
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()?;
    let mut offset = load_offset(&ep.id);
    eprintln!("q38 channel telegram long-poll ({})", ep.id);
    let mut conflict_n = 0u32;
    loop {
        let url = format!(
            "{API}/bot{token}/getUpdates?timeout=25&offset={offset}&allowed_updates=%5B%22message%22%5D"
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("q38 telegram poll: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("q38 telegram json: {e}");
                continue;
            }
        };
        if body["ok"] != true {
            let desc = body["description"].as_str().unwrap_or("");
            if let Some(secs) = retry_after_secs(&body, desc, status.as_u16()) {
                eprintln!("q38 telegram: rate-limited, retry in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
                continue;
            }
            if is_getupdates_conflict(desc, status.as_u16()) {
                conflict_n = conflict_n.saturating_add(1);
                let delay = conflict_backoff_s(conflict_n);
                eprintln!("q38 telegram: getUpdates conflict, retry in {delay}s");
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }
            eprintln!("q38 telegram: {desc}");
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }
        conflict_n = 0;
        let Some(arr) = body["result"].as_array() else {
            continue;
        };
        for upd in arr {
            let id = upd["update_id"].as_i64().unwrap_or(0);
            offset = id + 1;
            if let Some(msg) = upd.get("message").or_else(|| upd.get("edited_message")) {
                match native_from_message(&client, &token, &ep, msg).await {
                    Ok(Some(env)) => {
                        if let Err(e) = mgr.ingest(env).await {
                            eprintln!("q38 telegram ingest: {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("q38 telegram msg: {e}"),
                }
            }
        }
        save_offset(&ep.id, offset);
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
    let Some(token) = token(ep) else {
        return Err(Error::msg("telegram send: missing bot token"));
    };
    let chat_id = env.chat_id();
    if chat_id.is_empty() {
        return Err(Error::msg("telegram send: missing chat_id"));
    }
    let client = reqwest::Client::new();
    let text = super::outbound::parts_to_text(parts);
    if !text.trim().is_empty() {
        let url = format!("{API}/bot{token}/sendMessage");
        let resp = client
            .post(&url)
            .json(&json!({
                "chat_id": chat_id,
                "text": clip(&text, 3900),
            }))
            .send()
            .await?;
        let status = resp.status();
        let t = resp.text().await.unwrap_or_default();
        if status.as_u16() == 429 {
            let secs = serde_json::from_str::<Value>(&t)
                .ok()
                .and_then(|v| retry_after_secs(&v, &t, 429))
                .unwrap_or(3);
            tokio::time::sleep(Duration::from_secs(secs)).await;
            let retry = client
                .post(&url)
                .json(&json!({
                    "chat_id": chat_id,
                    "text": clip(&text, 3900),
                }))
                .send()
                .await?;
            if !retry.status().is_success() {
                let t = retry.text().await.unwrap_or_default();
                return Err(Error::msg(format!("telegram sendMessage: {t}")));
            }
            return Ok(());
        }
        if !status.is_success() {
            return Err(Error::msg(format!("telegram sendMessage: {t}")));
        }
    }
    Ok(())
}

async fn native_from_message(
    client: &reqwest::Client,
    token: &str,
    ep: &ChannelEndpoint,
    msg: &Value,
) -> Result<Option<NativePayload>> {
    let chat = &msg["chat"];
    let from = &msg["from"];
    let chat_id = chat["id"].to_string();
    let sender_id = from["id"].to_string();
    if sender_id == "null" || chat_id == "null" {
        return Ok(None);
    }
    let chat_type = chat["type"].as_str().unwrap_or("private");
    let is_group = matches!(chat_type, "group" | "supergroup");
    let bot_username = ep.extra.get("bot_username").cloned().unwrap_or_default();
    let text = msg["text"]
        .as_str()
        .or_else(|| msg["caption"].as_str())
        .unwrap_or("")
        .to_string();
    let mentioned = is_mentioned(&text, &bot_username)
        || msg["reply_to_message"]["from"]["is_bot"].as_bool() == Some(true);
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::text(strip_mention(&text, &bot_username)));
    }
    if let Some(photos) = msg["photo"].as_array() {
        if let Some(best) = photos.last() {
            if let Some(file_id) = best["file_id"].as_str() {
                if let Ok(url) = download_file(client, token, file_id).await {
                    parts.push(ContentPart::Image {
                        image_url: url,
                        url: String::new(),
                        mime: "image/jpeg".into(),
                    });
                }
            }
        }
    }
    if let Some(doc) = msg.get("document") {
        if let Some(file_id) = doc["file_id"].as_str() {
            let name = doc["file_name"].as_str().unwrap_or("file").to_string();
            if let Ok(url) = download_file(client, token, file_id).await {
                parts.push(ContentPart::File {
                    file_url: url,
                    file_id: file_id.into(),
                    name,
                });
            }
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let mut env = NativePayload {
        channel: if ep.kind.is_empty() {
            "telegram".into()
        } else {
            ep.kind.clone()
        },
        sender_id,
        sender_name: from["first_name"].as_str().unwrap_or("").to_string(),
        content_parts: parts,
        ..NativePayload::default()
    };
    env.meta.insert("chat_id".into(), json!(chat_id));
    env.meta.insert("is_group".into(), json!(is_group));
    env.meta.insert("is_mentioned".into(), json!(mentioned));
    env.meta.insert(
        "is_reply_to_bot".into(),
        json!(msg["reply_to_message"]["from"]["is_bot"].as_bool() == Some(true)),
    );
    Ok(Some(env))
}

async fn download_file(client: &reqwest::Client, token: &str, file_id: &str) -> Result<String> {
    let meta: Value = client
        .get(format!("{API}/bot{token}/getFile?file_id={file_id}"))
        .send()
        .await?
        .json()
        .await?;
    let path = meta["result"]["file_path"]
        .as_str()
        .ok_or_else(|| Error::msg("telegram getFile: no file_path"))?;
    let bytes = client
        .get(format!("{API}/file/bot{token}/{path}"))
        .send()
        .await?
        .bytes()
        .await?;
    if bytes.len() > MAX_INLINE_MEDIA_BYTES {
        return Err(Error::msg("telegram file over 2MB inline cap"));
    }
    let mime = if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".mp4") {
        "video/mp4"
    } else {
        "image/jpeg"
    };
    Ok(data_uri(mime, &bytes))
}

fn is_mentioned(text: &str, bot_username: &str) -> bool {
    if bot_username.is_empty() {
        return false;
    }
    let tag = format!("@{}", bot_username.trim_start_matches('@'));
    text.split_whitespace()
        .any(|w| w.eq_ignore_ascii_case(&tag))
}

fn strip_mention(text: &str, bot_username: &str) -> String {
    if bot_username.is_empty() {
        return text.to_string();
    }
    let tag = format!("@{}", bot_username.trim_start_matches('@'));
    text.split_whitespace()
        .filter(|w| !w.eq_ignore_ascii_case(&tag))
        .collect::<Vec<_>>()
        .join(" ")
}

fn offset_path(id: &str) -> PathBuf {
    crate::config::Config::home_dir()
        .map(|h| h.join("channels").join(format!("{id}.offset")))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/q38-{id}.offset")))
}

fn load_offset(id: &str) -> i64 {
    std::fs::read_to_string(offset_path(id))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn save_offset(id: &str, offset: i64) {
    let path = offset_path(id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, offset.to_string());
}

fn retry_after_secs(body: &Value, desc: &str, http_status: u16) -> Option<u64> {
    if let Some(n) = body["parameters"]["retry_after"].as_u64() {
        return Some(n.clamp(1, 120));
    }
    if http_status != 429 && !desc.to_ascii_lowercase().contains("retry after") {
        return None;
    }
    let lower = desc.to_ascii_lowercase();
    let Some(idx) = lower.rfind("retry after") else {
        return (http_status == 429).then_some(3);
    };
    let rest = desc[idx + "retry after".len()..].trim();
    let n: u64 = rest
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())?;
    Some(n.clamp(1, 120))
}

fn is_getupdates_conflict(desc: &str, http_status: u16) -> bool {
    http_status == 409
        || desc
            .to_ascii_lowercase()
            .contains("terminated by other getupdates")
        || desc.to_ascii_lowercase().contains("conflict")
}

fn conflict_backoff_s(attempt: u32) -> u64 {
    let base = 5.0_f64;
    let cap = 21.0_f64;
    let n = (base * 1.8_f64.powi(attempt.saturating_sub(1) as i32)).min(cap);
    n.ceil() as u64
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mention_strip() {
        assert!(is_mentioned("hey @q38bot do it", "q38bot"));
        assert_eq!(strip_mention("@q38bot do it", "q38bot"), "do it");
    }

    #[test]
    fn retry_after_from_parameters() {
        let body = json!({"ok":false,"parameters":{"retry_after":12}});
        assert_eq!(retry_after_secs(&body, "Too Many Requests", 429), Some(12));
    }

    #[test]
    fn retry_after_from_description() {
        let body = json!({"ok": false});
        assert_eq!(
            retry_after_secs(&body, "Too Many Requests: retry after 8", 429),
            Some(8)
        );
    }

    #[test]
    fn conflict_detected() {
        assert!(is_getupdates_conflict(
            "Conflict: terminated by other getUpdates request",
            409
        ));
        assert_eq!(conflict_backoff_s(1), 5);
        assert!(conflict_backoff_s(8) <= 21);
    }
}
