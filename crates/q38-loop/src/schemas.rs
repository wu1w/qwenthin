//! Washed from QwenPaw `src/qwenpaw/schemas.py` (commit 434574c).
//! Pixel-close envelope: Message / Content / AgentRequest / AgentResponse.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Enums.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Message,
    Reasoning,
    PluginCall,
    PluginCallOutput,
    FunctionCall,
    FunctionCallOutput,
    McpToolCall,
    McpToolCallOutput,
    Progress,
    Result,
}

impl Default for MessageType {
    fn default() -> Self {
        Self::Message
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Created,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

impl Default for RunStatus {
    fn default() -> Self {
        Self::InProgress
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    Text,
    Image,
    Audio,
    Video,
    Data,
    File,
    Refusal,
}

// ---------------------------------------------------------------------------
// Content blocks.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentBase {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delta: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunStatus>,
    #[serde(default = "content_object")]
    pub object: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn content_object() -> String {
    "content".into()
}

impl Default for ContentBase {
    fn default() -> Self {
        Self {
            delta: false,
            index: None,
            status: None,
            object: content_object(),
            msg_id: None,
            extra: Map::new(),
        }
    }
}

impl ContentBase {
    pub fn in_progress(&mut self) -> &mut Self {
        self.status = Some(RunStatus::InProgress);
        self
    }

    pub fn completed(&mut self) -> &mut Self {
        self.status = Some(RunStatus::Completed);
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text {
        #[serde(default)]
        text: String,
        #[serde(flatten)]
        base: ContentBase,
    },
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(flatten)]
        base: ContentBase,
    },
    Audio {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(flatten)]
        base: ContentBase,
    },
    Video {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        video_url: Option<String>,
        #[serde(flatten)]
        base: ContentBase,
    },
    File {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_url: Option<String>,
        #[serde(flatten)]
        base: ContentBase,
    },
    Data {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(flatten)]
        base: ContentBase,
    },
    Refusal {
        #[serde(default)]
        refusal: String,
        #[serde(flatten)]
        base: ContentBase,
    },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            base: ContentBase::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCallOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn uuid_hex() -> String {
    Uuid::new_v4().simple().to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    #[serde(default = "uuid_hex")]
    pub id: String,
    #[serde(default)]
    pub r#type: MessageType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default)]
    pub content: Vec<Content>,
    #[serde(default)]
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            id: uuid_hex(),
            r#type: MessageType::Message,
            role: None,
            content: Vec::new(),
            status: RunStatus::InProgress,
            metadata: None,
            extra: Map::new(),
        }
    }
}

impl Message {
    /// Append a content block, mirroring the 1.x runtime contract.
    pub fn add_content(&mut self, new_content: Content) -> &mut Self {
        self.content.push(new_content);
        self
    }

    pub fn completed(&mut self) -> &mut Self {
        self.status = RunStatus::Completed;
        self
    }

    pub fn in_progress(&mut self) -> &mut Self {
        self.status = RunStatus::InProgress;
        self
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Event {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentRequest {
    #[serde(default)]
    pub input: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default = "default_true")]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_true() -> bool {
    true
}

impl Default for AgentRequest {
    fn default() -> Self {
        Self {
            input: Vec::new(),
            session_id: None,
            user_id: None,
            stream: true,
            metadata: None,
            extra: Map::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub output: Vec<Message>,
    #[serde(default = "completed_status")]
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn completed_status() -> RunStatus {
    RunStatus::Completed
}

impl Default for AgentResponse {
    fn default() -> Self {
        Self {
            id: None,
            output: Vec::new(),
            status: RunStatus::Completed,
            created_at: None,
            completed_at: None,
            metadata: None,
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_content_and_complete() {
        let mut m = Message::default();
        m.add_content(Content::text("hi")).completed();
        assert_eq!(m.status, RunStatus::Completed);
        assert_eq!(m.content.len(), 1);
        assert_eq!(m.id.len(), 32);
    }

    #[test]
    fn roundtrip_agent_request() {
        let raw = r#"{"input":[],"stream":true}"#;
        let req: AgentRequest = serde_json::from_str(raw).unwrap();
        assert!(req.stream);
        assert!(req.input.is_empty());
    }
}
