//! Sender / group gate. Wash of QwenPaw `dm_policy` / `group_policy` /
//! `allow_from` / `require_mention` (pairing codes are not copied).

use super::envelope::NativePayload;
use super::ChannelEndpoint;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Deny(&'static str),
}

pub fn admit(ep: &ChannelEndpoint, env: &NativePayload) -> GateDecision {
    let sender = env.sender_id.trim();
    if !sender.is_empty() && ep.deny_from.iter().any(|d| d == sender) {
        return GateDecision::Deny("deny_from");
    }
    let group = env.is_group();
    let mentioned = env.is_mentioned();
    let policy = if group {
        ep.group_policy.as_str()
    } else {
        ep.dm_policy.as_str()
    };
    match policy {
        "closed" | "disabled" => GateDecision::Deny("closed"),
        "allowlist" => allowlist(ep, sender),
        "mention" => {
            if group && !mentioned {
                GateDecision::Deny("mention")
            } else {
                allowlist_if_set(ep, sender)
            }
        }
        _ => {
            // open
            if ep.require_mention && group && !mentioned {
                return GateDecision::Deny("mention");
            }
            allowlist_if_set(ep, sender)
        }
    }
}

fn allowlist(ep: &ChannelEndpoint, sender: &str) -> GateDecision {
    if ep.allow_from.is_empty() {
        return GateDecision::Deny("allowlist");
    }
    if sender.is_empty() || !ep.allow_from.iter().any(|a| a == sender) {
        GateDecision::Deny("allowlist")
    } else {
        GateDecision::Allow
    }
}

fn allowlist_if_set(ep: &ChannelEndpoint, sender: &str) -> GateDecision {
    if ep.allow_from.is_empty() {
        return GateDecision::Allow;
    }
    allowlist(ep, sender)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    fn env(group: bool, mentioned: bool, sender: &str) -> NativePayload {
        let mut p = NativePayload::text_only("telegram", "hi");
        p.sender_id = sender.into();
        let mut meta = Map::new();
        meta.insert("is_group".into(), json!(group));
        meta.insert("is_mentioned".into(), json!(mentioned));
        p.meta = meta;
        p
    }

    #[test]
    fn open_dm_allows() {
        let ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            ..ChannelEndpoint::default()
        };
        assert_eq!(admit(&ep, &env(false, false, "1")), GateDecision::Allow);
    }

    #[test]
    fn group_require_mention() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            require_mention: true,
            ..ChannelEndpoint::default()
        };
        ep.group_policy = "open".into();
        assert_eq!(
            admit(&ep, &env(true, false, "1")),
            GateDecision::Deny("mention")
        );
        assert_eq!(admit(&ep, &env(true, true, "1")), GateDecision::Allow);
    }

    #[test]
    fn allow_from_filters() {
        let mut ep = ChannelEndpoint {
            id: "tg".into(),
            kind: "telegram".into(),
            enabled: true,
            allow_from: vec!["42".into()],
            ..ChannelEndpoint::default()
        };
        ep.dm_policy = "open".into();
        assert_eq!(
            admit(&ep, &env(false, false, "1")),
            GateDecision::Deny("allowlist")
        );
        assert_eq!(admit(&ep, &env(false, false, "42")), GateDecision::Allow);
    }
}
