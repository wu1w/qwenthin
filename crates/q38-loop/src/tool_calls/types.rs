use crate::media::MediaPart;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolState {
    Success,
    Error,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBlock {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResponse {
    pub id: String,
    pub content: Vec<TextBlock>,
    pub state: ToolState,
    pub offloaded: bool,
    /// SHA-256 of the original output when live text was folded.
    pub blob: Option<String>,
    pub original_chars: usize,
    pub media: Vec<MediaPart>,
}

impl ToolResponse {
    pub fn text(id: impl Into<String>, text: impl Into<String>, state: ToolState) -> Self {
        let text = text.into();
        let original_chars = text.chars().count();
        Self {
            id: id.into(),
            content: vec![TextBlock { text }],
            state,
            offloaded: false,
            blob: None,
            original_chars,
            media: Vec::new(),
        }
    }

    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Offloaded,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    User,
    Timeout,
    Agent,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffloadReason {
    User,
    Timeout,
}

/// Cooperative cancel. `watch` carries the flag so waiters cannot miss a set.
#[derive(Clone, Debug)]
pub struct CancelFlag {
    tx: watch::Sender<bool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self { tx }
    }

    pub fn cancel(&self) {
        // `send` is a no-op (and leaves the value false) when nobody has
        // subscribed yet. Stop during HTTP has no waiter; `send_replace`
        // still sticks so `is_cancelled()` / later `cancelled()` see it.
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|set| *set).await;
    }
}

impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct Deadlines {
    pub started_at: tokio::time::Instant,
    pub offload_at: Option<tokio::time::Instant>,
    pub kill_at: Option<tokio::time::Instant>,
}

impl Deadlines {
    pub fn remaining_kill(&self) -> Option<std::time::Duration> {
        self.kill_at
            .map(|t| t.saturating_duration_since(tokio::time::Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::CancelFlag;

    #[tokio::test]
    async fn cancel_sticks_without_waiters() {
        let f = CancelFlag::new();
        f.cancel();
        assert!(
            f.is_cancelled(),
            "send_replace must stick with no subscribers"
        );
        tokio::time::timeout(std::time::Duration::from_millis(50), f.cancelled())
            .await
            .expect("cancelled() must resolve from the stored true");
    }
}
