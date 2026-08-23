//! QwenPaw native inbound payload: platform message → `content_parts`.
//!
//! Adapters (Telegram, webhook, …) convert their native JSON into this shape.
//! The agent never sees DingTalk/Telegram wire types.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::media::{MediaKind, MediaPart};
use crate::template::ChatMessage;

/// QwenPaw `ChannelAddress` — where the reply goes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAddress {
    /// `dm` | `channel` | `webhook` | `console`
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl ChannelAddress {
    pub fn handle(&self) -> String {
        if let Some(Value::String(h)) = self.extra.get("to_handle") {
            return h.clone();
        }
        if self.kind.is_empty() {
            return self.id.clone();
        }
        format!("{}:{}", self.kind, self.id)
    }
}

/// One content block. Wire names match QwenPaw `TextContent` / `ImageContent`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentPart {
    Text {
        #[serde(default)]
        text: String,
    },
    Image {
        #[serde(default)]
        image_url: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        mime: String,
    },
    Video {
        #[serde(default)]
        video_url: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        mime: String,
    },
    Audio {
        #[serde(default)]
        audio_url: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        mime: String,
    },
    File {
        #[serde(default)]
        file_url: String,
        #[serde(default)]
        file_id: String,
        #[serde(default)]
        name: String,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }

    fn image_src(&self) -> Option<&str> {
        match self {
            Self::Image { image_url, url, .. } => nonempty(image_url).or_else(|| nonempty(url)),
            _ => None,
        }
    }

    pub fn media_part(&self) -> Option<MediaPart> {
        match self {
            Self::Image { mime, .. } => {
                let url = self.image_src()?.to_string();
                let mut p = MediaPart::image_url(url);
                if !mime.is_empty() {
                    p.mime = mime.clone();
                }
                Some(p)
            }
            Self::Video {
                video_url,
                url,
                mime,
                ..
            } => {
                let src = nonempty(video_url).or_else(|| nonempty(url))?;
                let mut p = MediaPart::video_url(src);
                if !mime.is_empty() {
                    p.mime = mime.clone();
                }
                Some(p)
            }
            Self::Audio {
                audio_url,
                url,
                mime,
                ..
            } => {
                let src = nonempty(audio_url).or_else(|| nonempty(url))?;
                let m = if mime.is_empty() {
                    "audio/wav"
                } else {
                    mime.as_str()
                };
                Some(MediaPart::audio_url(src, m))
            }
            Self::File { file_url, name, .. } => {
                let src = nonempty(file_url)?;
                let lower = name.to_ascii_lowercase();
                if lower.ends_with(".mp4") || lower.ends_with(".webm") {
                    Some(MediaPart::video_url(src))
                } else if MediaKind::parse(
                    std::path::Path::new(name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or(""),
                ) == Some(MediaKind::Image)
                    || lower.ends_with(".png")
                    || lower.ends_with(".jpg")
                    || lower.ends_with(".jpeg")
                    || lower.ends_with(".webp")
                    || lower.ends_with(".gif")
                {
                    Some(MediaPart::image_url(src))
                } else {
                    None
                }
            }
            Self::Text { .. } => None,
        }
    }

    pub fn fallback_line(&self) -> Option<String> {
        match self {
            Self::Text { .. } => None,
            Self::Image { .. } => self.image_src().map(|u| format!("[Image: {u}]")),
            Self::Video { video_url, url, .. } => nonempty(video_url)
                .or_else(|| nonempty(url))
                .map(|u| format!("[Video: {u}]")),
            Self::Audio { .. } => Some("[Audio]".into()),
            Self::File {
                file_url,
                file_id,
                name,
                ..
            } => {
                let label = if !name.is_empty() {
                    name.as_str()
                } else if !file_id.is_empty() {
                    file_id.as_str()
                } else {
                    nonempty(file_url).unwrap_or("file")
                };
                Some(format!("[File: {label}]"))
            }
        }
    }
}

/// QwenPaw native dict that hits `BaseChannel.consume_one`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NativePayload {
    #[serde(default, alias = "channel_id")]
    pub channel: String,
    #[serde(default)]
    pub sender_id: String,
    #[serde(default)]
    pub sender_name: String,
    #[serde(default, alias = "session")]
    pub session_id: String,
    #[serde(default)]
    pub content_parts: Vec<ContentPart>,
    /// Convenience when the adapter only has a string (old `channel.inbound`).
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub meta: Map<String, Value>,
}

impl NativePayload {
    pub fn text_only(channel: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            channel: channel.into(),
            content_parts: vec![ContentPart::text(&text)],
            text: text.clone(),
            ..Self::default()
        }
    }

    pub fn parts(&self) -> Vec<ContentPart> {
        if !self.content_parts.is_empty() {
            return self.content_parts.clone();
        }
        let t = if !self.text.is_empty() {
            &self.text
        } else {
            &self.prompt
        };
        if t.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(t)]
        }
    }

    pub fn query_text(&self) -> String {
        let mut out = String::new();
        for p in self.parts() {
            if let Some(t) = p.as_text() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        if out.is_empty() {
            for p in self.parts() {
                if let Some(line) = p.fallback_line() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&line);
                }
            }
        }
        out
    }

    pub fn media_parts(&self) -> Vec<MediaPart> {
        self.parts()
            .iter()
            .filter_map(ContentPart::media_part)
            .collect()
    }

    pub fn to_chat_message(&self) -> ChatMessage {
        let mut text = self.query_text();
        if text.trim().is_empty() {
            text = " ".into();
        }
        let mut msg = ChatMessage::user(text);
        msg.parts = self.media_parts();
        msg
    }

    pub fn is_group(&self) -> bool {
        bool_meta(&self.meta, "is_group")
    }

    pub fn is_mentioned(&self) -> bool {
        bool_meta(&self.meta, "is_mentioned") || bool_meta(&self.meta, "is_reply_to_bot")
    }

    pub fn chat_id(&self) -> String {
        string_meta(&self.meta, "chat_id")
            .or_else(|| string_meta(&self.meta, "conversation_id"))
            .unwrap_or_else(|| self.sender_id.clone())
    }

    pub fn reply_url(&self) -> Option<String> {
        string_meta(&self.meta, "reply_url").or_else(|| string_meta(&self.meta, "session_webhook"))
    }

    /// QwenPaw `resolve_session_id` default, with a group split.
    pub fn route_key(&self) -> String {
        let ch = if self.channel.is_empty() {
            "webhook"
        } else {
            self.channel.as_str()
        };
        if self.is_group() {
            format!("{ch}:g:{}", self.chat_id())
        } else {
            let who = if self.sender_id.is_empty() {
                self.chat_id()
            } else {
                self.sender_id.clone()
            };
            format!("{ch}:dm:{who}")
        }
    }

    pub fn merge(items: Vec<Self>) -> Option<Self> {
        let mut iter = items.into_iter();
        let mut first = iter.next()?;
        for next in iter {
            first.content_parts.extend(next.parts());
            if first.sender_id.is_empty() {
                first.sender_id = next.sender_id;
            }
            if first.sender_name.is_empty() {
                first.sender_name = next.sender_name;
            }
            for (k, v) in next.meta {
                first.meta.insert(k, v);
            }
            if first.session_id.is_empty() {
                first.session_id = next.session_id;
            }
        }
        first.text.clear();
        first.prompt.clear();
        Some(first)
    }
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn bool_meta(meta: &Map<String, Value>, key: &str) -> bool {
    match meta.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn string_meta(meta: &Map<String, Value>, key: &str) -> Option<String> {
    match meta.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_alias_becomes_parts() {
        let p = NativePayload::text_only("telegram", "hello");
        assert_eq!(p.query_text(), "hello");
        assert!(p.media_parts().is_empty());
    }

    #[test]
    fn qwenpaw_image_part() {
        let raw = serde_json::json!({
            "channel_id": "telegram",
            "sender_id": "9",
            "content_parts": [
                {"type": "text", "text": "what color"},
                {"type": "image", "image_url": "https://x/a.png"}
            ],
            "meta": {"is_group": false, "chat_id": "9"}
        });
        let p: NativePayload = serde_json::from_value(raw).unwrap();
        assert_eq!(p.channel, "telegram");
        assert_eq!(p.query_text(), "what color");
        assert_eq!(p.media_parts().len(), 1);
        assert_eq!(p.route_key(), "telegram:dm:9");
    }

    #[test]
    fn merge_concat_parts() {
        let a = NativePayload::text_only("hook", "a");
        let mut b = NativePayload::text_only("hook", "b");
        b.content_parts.push(ContentPart::Image {
            image_url: "https://x/i.png".into(),
            url: String::new(),
            mime: String::new(),
        });
        let m = NativePayload::merge(vec![a, b]).unwrap();
        assert!(m.query_text().contains('a'));
        assert_eq!(m.media_parts().len(), 1);
    }
}
