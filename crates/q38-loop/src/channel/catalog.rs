//! QwenPaw-shaped channel catalog for the console (kinds, copy, fields, QR).

use serde::Serialize;
use serde_json::Value;

use super::KINDS;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub secret: bool,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub hint: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct KindSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    pub mark: &'static str,
    /// Official-ish tile color (icon only).
    pub color: &'static str,
    pub qr: bool,
    /// One endpoint per kind (webhook may repeat).
    pub once: bool,
    /// In-process listener (`q38 web` and `q38 --channels`).
    pub in_process: bool,
    pub fields: &'static [FieldSpec],
}

pub const CATALOG: &[KindSpec] = &[
    KindSpec {
        id: "dingtalk",
        name: "钉钉",
        blurb: "扫码创应用后进程内连钉钉 Stream",
        mark: "钉",
        color: "#0089FF",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "client_id",
                label: "Client ID",
                secret: false,
                hint: "即 AppKey",
            },
            FieldSpec {
                key: "client_secret",
                label: "Client Secret",
                secret: true,
                hint: "即 AppSecret",
            },
        ],
    },
    KindSpec {
        id: "feishu",
        name: "飞书",
        blurb: "扫码创应用后进程内连飞书长连接",
        mark: "飞",
        color: "#3370FF",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "app_id",
                label: "App ID",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "app_secret",
                label: "App Secret",
                secret: true,
                hint: "",
            },
            FieldSpec {
                key: "domain",
                label: "域名",
                secret: false,
                hint: "feishu 或 lark",
            },
        ],
    },
    KindSpec {
        id: "qq",
        name: "QQ",
        blurb: "扫码绑定后进程内连 QQ 官方网关",
        mark: "Q",
        color: "#12B7F5",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "app_id",
                label: "App ID",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "client_secret",
                label: "Client Secret",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "wechat",
        name: "微信",
        blurb: "iLink 扫码后进程内长轮询 getupdates",
        mark: "微",
        color: "#07C160",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "bot_token",
                label: "Bot token",
                secret: true,
                hint: "扫码后自动填入",
            },
            FieldSpec {
                key: "base_url",
                label: "iLink base_url",
                secret: false,
                hint: "默认 https://ilinkai.weixin.qq.com",
            },
        ],
    },
    KindSpec {
        id: "wecom",
        name: "企业微信",
        blurb: "扫码后进程内连企微 AI Bot 长连接",
        mark: "企",
        color: "#2B7BD6",
        qr: true,
        once: true,
        in_process: true,
        fields: &[
            FieldSpec {
                key: "bot_id",
                label: "Bot ID",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "secret",
                label: "Secret",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "telegram",
        name: "Telegram",
        blurb: "Bot API 长轮询，控制台进程内接听",
        mark: "TG",
        color: "#229ED9",
        qr: false,
        once: true,
        in_process: true,
        fields: &[FieldSpec {
            key: "bot_token",
            label: "Bot token",
            secret: true,
            hint: "BotFather 发给你的 token",
        }],
    },
    KindSpec {
        id: "discord",
        name: "Discord",
        blurb: "填 Bot token",
        mark: "DC",
        color: "#5865F2",
        qr: false,
        once: true,
        in_process: false,
        fields: &[FieldSpec {
            key: "bot_token",
            label: "Bot token",
            secret: true,
            hint: "",
        }],
    },
    KindSpec {
        id: "slack",
        name: "Slack",
        blurb: "填 Bot token",
        mark: "S",
        color: "#611F69",
        qr: false,
        once: true,
        in_process: false,
        fields: &[FieldSpec {
            key: "bot_token",
            label: "Bot token",
            secret: true,
            hint: "",
        }],
    },
    KindSpec {
        id: "webhook",
        name: "Webhook",
        blurb: "POST /inbound，控制台进程内接听",
        mark: "WH",
        color: "#615CED",
        qr: false,
        once: false,
        in_process: true,
        fields: &[],
    },
    KindSpec {
        id: "onebot",
        name: "OneBot",
        blurb: "OneBot 协议接入",
        mark: "OB",
        color: "#111827",
        qr: false,
        once: true,
        in_process: false,
        fields: &[
            FieldSpec {
                key: "ws_url",
                label: "WebSocket URL",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "access_token",
                label: "Access token",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "imessage",
        name: "iMessage",
        blurb: "本机 iMessage 桥",
        mark: "iM",
        color: "#34C759",
        qr: false,
        once: true,
        in_process: false,
        fields: &[],
    },
    KindSpec {
        id: "matrix",
        name: "Matrix",
        blurb: "Homeserver + access token",
        mark: "MX",
        color: "#0DBD8B",
        qr: false,
        once: true,
        in_process: false,
        fields: &[
            FieldSpec {
                key: "homeserver",
                label: "Homeserver",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "access_token",
                label: "Access token",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "mattermost",
        name: "Mattermost",
        blurb: "Bot token + 站点 URL",
        mark: "MM",
        color: "#0058CC",
        qr: false,
        once: true,
        in_process: false,
        fields: &[
            FieldSpec {
                key: "url",
                label: "站点 URL",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "bot_token",
                label: "Bot token",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "mqtt",
        name: "MQTT",
        blurb: "Broker 主题订阅",
        mark: "MQ",
        color: "#660066",
        qr: false,
        once: true,
        in_process: false,
        fields: &[
            FieldSpec {
                key: "broker",
                label: "Broker",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "username",
                label: "用户名",
                secret: false,
                hint: "",
            },
            FieldSpec {
                key: "password",
                label: "密码",
                secret: true,
                hint: "",
            },
        ],
    },
    KindSpec {
        id: "voice",
        name: "Voice",
        blurb: "语音通道",
        mark: "V",
        color: "#F59E0B",
        qr: false,
        once: true,
        in_process: false,
        fields: &[],
    },
    KindSpec {
        id: "sip",
        name: "SIP",
        blurb: "SIP 语音",
        mark: "SIP",
        color: "#64748B",
        qr: false,
        once: true,
        in_process: false,
        fields: &[],
    },
    KindSpec {
        id: "xiaoyi",
        name: "小艺",
        blurb: "华为小艺",
        mark: "艺",
        color: "#CF0A2C",
        qr: false,
        once: true,
        in_process: false,
        fields: &[],
    },
    KindSpec {
        id: "yuanbao",
        name: "元宝",
        blurb: "腾讯元宝",
        mark: "宝",
        color: "#0052D9",
        qr: false,
        once: true,
        in_process: false,
        fields: &[],
    },
];

pub fn spec(kind: &str) -> Option<&'static KindSpec> {
    let k = kind.to_ascii_lowercase();
    CATALOG.iter().find(|s| s.id == k)
}

pub fn supports_qr(kind: &str) -> bool {
    spec(kind).is_some_and(|s| s.qr)
}

/// Configured `[[channels.endpoints]]` kinds (not cli / sidecar / console).
pub fn endpoint_kind(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    if matches!(k.as_str(), "cli" | "sidecar" | "console" | "") {
        return false;
    }
    KINDS.iter().any(|x| x.eq_ignore_ascii_case(&k))
}

pub fn catalog_json() -> Value {
    serde_json::to_value(CATALOG).unwrap_or(Value::Array(vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_kinds_are_in_catalog() {
        for k in ["dingtalk", "feishu", "qq", "wechat", "wecom"] {
            assert!(supports_qr(k), "{k}");
            assert!(endpoint_kind(k), "{k}");
        }
        assert!(!supports_qr("telegram"));
        assert!(endpoint_kind("telegram"));
        assert!(endpoint_kind("webhook"));
        assert!(!endpoint_kind("cli"));
        assert!(!endpoint_kind("console"));
    }
}
