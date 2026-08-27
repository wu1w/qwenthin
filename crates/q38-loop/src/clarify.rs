//! Blocking multiple-choice ask. Append-only `ask` tool — not in the frozen four.
//!
//! Armed by `/plan` or `/clarify`. YOLO / `--print` (no hub) skip to the first
//! option. Does not replace permit.

use std::fmt;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::tool_calls::CancelFlag;

pub const CLARIFY_CARD: &str = "\
[clarify] You may call ask with a title, prompt, and 2-4 short options when a \
choice would change the work. First option is recommended. The user picks, \
skips (recommended), or types Other. Do not ask in prose.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClarifyOption {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClarifyAsk {
    pub title: String,
    pub prompt: String,
    pub options: Vec<ClarifyOption>,
}

impl ClarifyAsk {
    pub fn recommended(&self) -> Option<&ClarifyOption> {
        self.options.first()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClarifyDecision {
    Pick { id: String, label: String },
    Other { text: String },
    Skip,
}

pub struct ClarifyRequest {
    pub ask: ClarifyAsk,
    /// Sidecar / console session id. Empty when the hub was not tagged.
    pub session: String,
    pub reply: oneshot::Sender<ClarifyDecision>,
}

#[derive(Clone)]
pub struct ClarifyHub {
    tx: mpsc::UnboundedSender<ClarifyRequest>,
    session: String,
}

impl fmt::Debug for ClarifyHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClarifyHub").finish_non_exhaustive()
    }
}

impl ClarifyHub {
    pub fn pair() -> (Self, mpsc::UnboundedReceiver<ClarifyRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                session: String::new(),
            },
            rx,
        )
    }

    pub fn with_session(&self, id: impl Into<String>) -> Self {
        let mut c = self.clone();
        c.session = id.into();
        c
    }

    pub async fn ask(&self, ask: ClarifyAsk, cancel: &CancelFlag) -> ClarifyDecision {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(ClarifyRequest {
                ask,
                session: self.session.clone(),
                reply,
            })
            .is_err()
        {
            return ClarifyDecision::Skip;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => ClarifyDecision::Skip,
            dec = rx => dec.unwrap_or(ClarifyDecision::Skip),
        }
    }
}

pub fn parse_ask(args: &Value) -> Result<ClarifyAsk, String> {
    let prompt = args
        .get("prompt")
        .or_else(|| args.get("question"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "ask needs a prompt".to_string())?
        .to_string();
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("选择")
        .to_string();
    let raw = args
        .get("options")
        .ok_or_else(|| "ask needs 2-4 options".to_string())?;
    let mut options = Vec::new();
    match raw {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if options.len() == 4 {
                    break;
                }
                match item {
                    Value::String(s) => {
                        let label = s.trim();
                        if !label.is_empty() {
                            options.push(ClarifyOption {
                                id: format!("{}", i + 1),
                                label: label.to_string(),
                            });
                        }
                    }
                    Value::Object(map) => {
                        let label = map
                            .get("label")
                            .or_else(|| map.get("text"))
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let Some(label) = label else { continue };
                        let id = map
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("{}", i + 1));
                        options.push(ClarifyOption {
                            id,
                            label: label.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => return Err("options must be an array".into()),
    }
    if options.len() < 2 {
        return Err("ask needs 2-4 options".into());
    }
    Ok(ClarifyAsk {
        title,
        prompt,
        options,
    })
}

pub fn format_decision(ask: &ClarifyAsk, decision: ClarifyDecision) -> String {
    match decision {
        ClarifyDecision::Pick { id, label } => format!("picked: {label} (id={id})"),
        ClarifyDecision::Other { text } => {
            let t = text.trim();
            if t.is_empty() {
                format_decision(ask, ClarifyDecision::Skip)
            } else {
                format!("other: {t}")
            }
        }
        ClarifyDecision::Skip => match ask.recommended() {
            Some(o) => format!("skipped: using {} (recommended)", o.label),
            None => "skipped".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_strings_and_objects() {
        let ask = parse_ask(&json!({
            "prompt": "Which scope?",
            "options": ["All routes", {"id": "auth", "label": "Auth only"}, "Other-ish"]
        }))
        .unwrap();
        assert_eq!(ask.title, "选择");
        assert_eq!(ask.options.len(), 3);
        assert_eq!(ask.options[0].id, "1");
        assert_eq!(ask.options[1].id, "auth");
        assert_eq!(ask.recommended().unwrap().label, "All routes");
    }

    #[test]
    fn parse_rejects_one_option() {
        assert!(parse_ask(&json!({"prompt": "x", "options": ["only"]})).is_err());
    }

    #[test]
    fn skip_uses_first_option() {
        let ask = parse_ask(&json!({"prompt": "x", "options": ["A", "B"]})).unwrap();
        assert_eq!(
            format_decision(&ask, ClarifyDecision::Skip),
            "skipped: using A (recommended)"
        );
        assert_eq!(
            format_decision(
                &ask,
                ClarifyDecision::Pick {
                    id: "2".into(),
                    label: "B".into()
                }
            ),
            "picked: B (id=2)"
        );
    }

    #[tokio::test]
    async fn hub_delivers_pick() {
        let (hub, mut rx) = ClarifyHub::pair();
        let ask = parse_ask(&json!({"prompt": "x", "options": ["A", "B"]})).unwrap();
        let h = hub.clone();
        let ask2 = ask.clone();
        let join = tokio::spawn(async move { h.ask(ask2, &CancelFlag::new()).await });
        let req = rx.recv().await.expect("queued");
        let _ = req.reply.send(ClarifyDecision::Pick {
            id: "1".into(),
            label: "A".into(),
        });
        assert!(matches!(
            join.await.unwrap(),
            ClarifyDecision::Pick { id, .. } if id == "1"
        ));
    }
}
