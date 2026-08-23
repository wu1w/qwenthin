//! Message channels. Wash of QwenPaw `app/channels`:
//! native payload → `content_parts` → per-session queue → send back.
//!
//! In-process listeners: webhook, telegram, QQ, WeChat, WeCom, Feishu, DingTalk.

mod access;
pub mod catalog;
mod dingtalk;
mod envelope;
mod feishu;
mod inbound;
mod mailbox;
mod manager;
mod outbound;
mod qq;
pub mod qrcode;
mod router;
mod runtime;
mod telegram;
mod webhook;
mod wechat;
mod wecom;

pub use catalog::{catalog_json, endpoint_kind, CATALOG};
pub use envelope::{ChannelAddress, ContentPart, NativePayload};
pub use inbound::{serve_endpoint, serve_qq};
pub use mailbox::{push_steer, take_steer, BusyDecision, BusyPolicy, Mailbox, SteerSlot};
pub use manager::{ChannelHandler, ChannelManager, IngestResult};
pub use outbound::{deliver, outbound_notification, parts_to_text, reply_text};
pub use qrcode::{fetch_qrcode, poll_qrcode};
pub use router::SessionRouter;
pub use runtime::run as run_channels;

/// QwenPaw `BUILTIN_CHANNEL_KEYS` plus q38 local surfaces.
pub const KINDS: &[&str] = &[
    "cli",
    "sidecar",
    "console",
    "webhook",
    "telegram",
    "discord",
    "slack",
    "dingtalk",
    "feishu",
    "qq",
    "wechat",
    "wecom",
    "imessage",
    "matrix",
    "mattermost",
    "mqtt",
    "voice",
    "onebot",
    "sip",
    "xiaoyi",
    "yuanbao",
];

pub fn known_kind(kind: &str) -> bool {
    KINDS.iter().any(|k| k.eq_ignore_ascii_case(kind))
}

/// One configured inbound endpoint. QwenPaw `BaseChannelConfig` shape.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChannelEndpoint {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub deny_from: Vec<String>,
    #[serde(default)]
    pub require_mention: bool,
    /// `open` | `allowlist` | `closed`
    pub dm_policy: String,
    /// `open` | `allowlist` | `mention` | `closed`
    pub group_policy: String,
    /// HTTP POST for outbound `content_parts` (webhook / DingTalk sessionWebhook).
    pub reply_url: String,
    /// Listen address for `kind = "webhook"`.
    pub bind: String,
    pub secret: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, String>,
}

impl Default for ChannelEndpoint {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: String::new(),
            enabled: false,
            allow_from: Vec::new(),
            deny_from: Vec::new(),
            require_mention: false,
            dm_policy: "open".into(),
            group_policy: "open".into(),
            reply_url: String::new(),
            bind: String::new(),
            secret: String::new(),
            extra: std::collections::BTreeMap::new(),
        }
    }
}

fn extra_value_placeholder(v: &str) -> bool {
    v.is_empty() || v == "true" || v == "false" || v.starts_with("****")
}

impl ChannelEndpoint {
    /// Console GET redacts `secret` and token extras. A round-trip save must
    /// not wipe the values still on disk.
    pub fn absorb_secrets_from(&mut self, prev: &ChannelEndpoint) {
        if self.secret.trim().is_empty() {
            self.secret = prev.secret.clone();
        }
        for (k, v) in &prev.extra {
            let incoming = self.extra.get(k).map(|s| s.as_str()).unwrap_or("");
            if extra_value_placeholder(incoming) {
                self.extra.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Keep prior secrets for endpoints the UI re-posts without them.
pub fn merge_channel_endpoints(
    old: &[ChannelEndpoint],
    incoming: Vec<ChannelEndpoint>,
) -> Vec<ChannelEndpoint> {
    incoming
        .into_iter()
        .map(|mut ep| {
            if !ep.id.is_empty() {
                if let Some(prev) = old.iter().find(|p| p.id == ep.id) {
                    ep.absorb_secrets_from(prev);
                }
            }
            ep
        })
        .collect()
}

pub fn upsert_channel_endpoint(list: &mut Vec<ChannelEndpoint>, mut add: ChannelEndpoint) {
    if add.id.trim().is_empty() {
        add.id = add.kind.clone();
    }
    if let Some(prev) = list.iter().find(|p| p.id == add.id) {
        add.absorb_secrets_from(prev);
    }
    if let Some(i) = list.iter().position(|p| p.id == add.id) {
        list[i] = add;
    } else {
        list.push(add);
    }
}

pub fn remove_channel_endpoint(list: &mut Vec<ChannelEndpoint>, id: &str) {
    list.retain(|e| e.id != id);
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChannelsConfig {
    /// Hermes `/busy` default for every session.
    pub busy: String,
    /// Surfaces allowed to enqueue. `cli` and `sidecar` are always implied.
    pub enabled: Vec<String>,
    pub endpoints: Vec<ChannelEndpoint>,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            busy: "interrupt".into(),
            enabled: vec!["cli".into(), "sidecar".into()],
            endpoints: Vec::new(),
        }
    }
}

impl ChannelsConfig {
    pub fn busy_policy(&self) -> BusyPolicy {
        self.busy.parse().unwrap_or(BusyPolicy::Interrupt)
    }

    pub fn list_json(&self) -> serde_json::Value {
        let mut rows = vec![
            serde_json::json!({"id":"cli","kind":"cli","enabled":true,"builtin":true}),
            serde_json::json!({"id":"sidecar","kind":"sidecar","enabled":true,"builtin":true}),
            serde_json::json!({"id":"console","kind":"console","enabled":self.enabled.iter().any(|s| s == "console"),"builtin":true}),
        ];
        for ep in &self.endpoints {
            rows.push(serde_json::json!({
                "id": ep.id,
                "kind": ep.kind,
                "enabled": ep.enabled,
                "bind": ep.bind,
                "require_mention": ep.require_mention,
                "dm_policy": ep.dm_policy,
                "group_policy": ep.group_policy,
                "builtin": false,
            }));
        }
        serde_json::json!(rows)
    }

    pub fn endpoint_for(&self, channel: &str) -> Option<&ChannelEndpoint> {
        if channel.is_empty() {
            return None;
        }
        self.endpoints.iter().find(|e| {
            e.enabled
                && (e.id.eq_ignore_ascii_case(channel) || e.kind.eq_ignore_ascii_case(channel))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_save_keeps_secret_and_bot_token() {
        let mut prev = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            secret: "hook-secret".into(),
            ..ChannelEndpoint::default()
        };
        prev.extra.insert("bot_token".into(), "123:ABC".into());
        prev.extra.insert("bot_username".into(), "q38bot".into());

        let mut incoming = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            ..ChannelEndpoint::default()
        };
        incoming
            .extra
            .insert("bot_username".into(), "q38bot".into());
        incoming.extra.insert("bot_token".into(), "true".into());

        let out = merge_channel_endpoints(&[prev], vec![incoming]);
        assert_eq!(out[0].secret, "hook-secret");
        assert_eq!(
            out[0].extra.get("bot_token").map(String::as_str),
            Some("123:ABC")
        );
        assert_eq!(
            out[0].extra.get("bot_username").map(String::as_str),
            Some("q38bot")
        );
        assert!(out[0].enabled);
    }

    #[test]
    fn typed_secret_replaces() {
        let prev = ChannelEndpoint {
            id: "wh".into(),
            secret: "old".into(),
            ..ChannelEndpoint::default()
        };
        let incoming = ChannelEndpoint {
            id: "wh".into(),
            secret: "new".into(),
            ..ChannelEndpoint::default()
        };
        let out = merge_channel_endpoints(&[prev], vec![incoming]);
        assert_eq!(out[0].secret, "new");
    }

    #[test]
    fn upsert_does_not_drop_siblings() {
        let mut list = vec![ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            ..ChannelEndpoint::default()
        }];
        upsert_channel_endpoint(
            &mut list,
            ChannelEndpoint {
                id: "hook".into(),
                kind: "webhook".into(),
                ..ChannelEndpoint::default()
            },
        );
        assert_eq!(list.len(), 2);
        remove_channel_endpoint(&mut list, "hook");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "tg");
    }

    #[test]
    fn known_kind_includes_scan_platforms() {
        for k in ["feishu", "qq", "wechat", "wecom", "dingtalk"] {
            assert!(known_kind(k), "{k}");
            assert!(endpoint_kind(k), "{k}");
        }
        assert!(!endpoint_kind("cli"));
    }
}
