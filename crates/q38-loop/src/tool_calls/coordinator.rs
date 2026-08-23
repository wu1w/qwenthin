use std::collections::HashMap;
use std::future::{pending, Future};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::lock_unpoison;

use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};

use super::timeout::{secs_to_dur, MIN_BACKGROUND_WINDOW_SECS, OFFLOAD_TIMEOUT_RATIO};
use super::types::{
    CancelFlag, CancelReason, Deadlines, TextBlock, ToolCall, ToolResponse, ToolState,
};

struct HookTimeouts {
    default_timeout_secs: Option<f64>,
    max_internal_timeout_secs: Option<f64>,
}

struct LiveCall {
    cancel: CancelFlag,
    name: String,
}

struct FinishedBg {
    name: String,
    response: ToolResponse,
}

pub struct ToolCoordinator {
    default_timeout: Option<f64>,
    cancel_grace: Duration,
    offload_on_deadline: AtomicBool,
    live: Arc<Mutex<HashMap<String, LiveCall>>>,
    finished: Arc<Mutex<Vec<FinishedBg>>>,
    per_agent: Mutex<HashMap<(String, String), f64>>,
    hooks: Mutex<HashMap<String, HookTimeouts>>,
}

impl ToolCoordinator {
    pub fn new(default_timeout_secs: Option<f64>) -> Self {
        Self {
            default_timeout: default_timeout_secs,
            cancel_grace: Duration::from_secs(5),
            offload_on_deadline: AtomicBool::new(false),
            live: Arc::new(Mutex::new(HashMap::new())),
            finished: Arc::new(Mutex::new(Vec::new())),
            per_agent: Mutex::new(HashMap::new()),
            hooks: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_offload_on_deadline(&self, value: bool) {
        self.offload_on_deadline.store(value, Ordering::Relaxed);
    }

    pub fn register_hook(
        &self,
        tool_name: impl Into<String>,
        default_timeout_secs: Option<f64>,
        max_internal_timeout_secs: Option<f64>,
    ) {
        lock_unpoison(&self.hooks).insert(
            tool_name.into(),
            HookTimeouts {
                default_timeout_secs,
                max_internal_timeout_secs,
            },
        );
    }

    pub fn set_agent_tool_timeout(
        &self,
        agent_id: &str,
        tool_name: &str,
        timeout_secs: Option<f64>,
    ) -> bool {
        let mut map = lock_unpoison(&self.per_agent);
        match timeout_secs {
            None => {
                map.remove(&(agent_id.to_string(), tool_name.to_string()));
                true
            }
            Some(secs) if secs <= 0.0 => false,
            Some(secs) => {
                if let Some(hook) = lock_unpoison(&self.hooks).get(tool_name) {
                    if hook.max_internal_timeout_secs.is_some_and(|cap| secs > cap) {
                        return false;
                    }
                }
                map.insert((agent_id.to_string(), tool_name.to_string()), secs);
                true
            }
        }
    }

    /// Completed background tools. Each id already had a foreground
    /// "running in background" tool result; these are follow-up notes.
    pub fn take_finished(&self) -> Vec<(String, ToolResponse)> {
        lock_unpoison(&self.finished)
            .drain(..)
            .map(|f| (f.name, f.response))
            .collect()
    }

    /// Cooperative-cancel every in-flight call (foreground and offloaded).
    pub fn cancel_background(&self) {
        for entry in lock_unpoison(&self.live).values() {
            entry.cancel.cancel();
        }
    }

    /// Run `work` until it finishes, is cancelled, hits kill, or is offloaded.
    pub async fn execute<F, Fut>(
        &self,
        call: ToolCall,
        agent_id: &str,
        timeout_override: Option<f64>,
        work: F,
    ) -> ToolResponse
    where
        F: FnOnce(CancelFlag) -> Fut + Send + 'static,
        Fut: Future<Output = ToolResponse> + Send + 'static,
    {
        let timeout = self.resolve_timeout(agent_id, &call.name, timeout_override);
        let now = Instant::now();
        let mut deadlines = Deadlines {
            started_at: now,
            offload_at: timeout.map(|s| now + secs_to_dur(s * OFFLOAD_TIMEOUT_RATIO)),
            kill_at: None,
        };
        if let Some(secs) = timeout {
            super::timeout::arm_kill_deadline(&mut deadlines, secs, false);
        }

        let cancel = CancelFlag::new();
        {
            let mut live = lock_unpoison(&self.live);
            live.insert(
                call.id.clone(),
                LiveCall {
                    cancel: cancel.clone(),
                    name: call.name.clone(),
                },
            );
        }

        let cancel_for_task = cancel.clone();
        let join = tokio::spawn(work(cancel_for_task));
        let outcome = self.drive(call.id.clone(), join, cancel, deadlines).await;
        if !outcome.offloaded {
            lock_unpoison(&self.live).remove(&call.id);
        }
        outcome
    }

    pub fn cancel(&self, tool_call_id: &str, _reason: CancelReason) -> bool {
        let live = lock_unpoison(&self.live);
        let Some(entry) = live.get(tool_call_id) else {
            return false;
        };
        entry.cancel.cancel();
        true
    }

    fn resolve_timeout(
        &self,
        agent_id: &str,
        tool_name: &str,
        override_secs: Option<f64>,
    ) -> Option<f64> {
        if override_secs.is_some() {
            return override_secs;
        }
        if let Some(v) =
            lock_unpoison(&self.per_agent).get(&(agent_id.to_string(), tool_name.to_string()))
        {
            return Some(*v);
        }
        if let Some(hook) = lock_unpoison(&self.hooks).get(tool_name) {
            if hook.default_timeout_secs.is_some() {
                return hook.default_timeout_secs;
            }
        }
        self.default_timeout
    }

    async fn drive(
        &self,
        id: String,
        mut join: JoinHandle<ToolResponse>,
        cancel: CancelFlag,
        mut deadlines: Deadlines,
    ) -> ToolResponse {
        loop {
            tokio::select! {
                biased;
                res = &mut join => {
                    return res.unwrap_or_else(|_| {
                        ToolResponse::text(&id, "Error: tool task aborted", ToolState::Interrupted)
                    });
                }
                _ = cancel.cancelled() => {
                    return force_stop(&mut join, self.cancel_grace, &id, "cancelled").await;
                }
                _ = sleep_opt(deadlines.kill_at) => {
                    cancel.cancel();
                    return force_stop(&mut join, self.cancel_grace, &id, "timeout").await;
                }
                _ = sleep_opt(deadlines.offload_at) => {
                    if self.offload_on_deadline.load(Ordering::Relaxed)
                        && Self::has_kill_budget(&deadlines)
                    {
                        let name = lock_unpoison(&self.live)
                            .get(&id)
                            .map(|l| l.name.clone())
                            .unwrap_or_default();
                        self.spawn_watch(id.clone(), name, join, cancel, deadlines.kill_at);
                        let text = format!(
                            "running in background (id={id}). Keep going; the result will be posted as a follow-up note when it finishes."
                        );
                        let original_chars = text.chars().count();
                        return ToolResponse {
                            id: id.clone(),
                            content: vec![TextBlock { text }],
                            state: ToolState::Success,
                            offloaded: true,
                            blob: None,
                            original_chars,
                            media: Vec::new(),
                        };
                    }
                    if deadlines.kill_at.is_some() {
                        deadlines.offload_at = None;
                        continue;
                    }
                    cancel.cancel();
                    return force_stop(&mut join, self.cancel_grace, &id, "timeout").await;
                }
            }
        }
    }

    fn spawn_watch(
        &self,
        id: String,
        name: String,
        mut join: JoinHandle<ToolResponse>,
        cancel: CancelFlag,
        kill_at: Option<Instant>,
    ) {
        let live = Arc::clone(&self.live);
        let finished = Arc::clone(&self.finished);
        let grace = self.cancel_grace;
        tokio::spawn(async move {
            let response = tokio::select! {
                biased;
                res = &mut join => res.unwrap_or_else(|_| {
                    ToolResponse::text(&id, "Error: tool task aborted", ToolState::Interrupted)
                }),
                _ = cancel.cancelled() => {
                    force_stop(&mut join, grace, &id, "cancelled").await
                }
                _ = sleep_opt(kill_at) => {
                    cancel.cancel();
                    force_stop(&mut join, grace, &id, "timeout").await
                }
            };
            lock_unpoison(&live).remove(&id);
            lock_unpoison(&finished).push(FinishedBg { name, response });
        });
    }

    fn has_kill_budget(deadlines: &Deadlines) -> bool {
        deadlines
            .remaining_kill()
            .is_some_and(|d| d.as_secs_f64() >= MIN_BACKGROUND_WINDOW_SECS)
    }
}

async fn force_stop(
    join: &mut JoinHandle<ToolResponse>,
    grace: Duration,
    id: &str,
    msg: &str,
) -> ToolResponse {
    let _ = tokio::time::timeout(grace, &mut *join).await;
    join.abort();
    ToolResponse::text(id, msg, ToolState::Interrupted)
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn completes() {
        let coord = ToolCoordinator::new(None);
        let out = coord
            .execute(call("c1"), "agent", None, |_cancel| async {
                ToolResponse::text("c1", "ok", ToolState::Success)
            })
            .await;
        assert_eq!(out.state, ToolState::Success);
        assert_eq!(out.joined_text(), "ok");
    }

    #[tokio::test]
    async fn poisoned_locks_recover_and_loop_continues() {
        let coord = std::sync::Arc::new(ToolCoordinator::new(Some(30.0)));
        let poisoner = coord.clone();
        std::thread::spawn(move || {
            let _guard = poisoner.live.lock().unwrap();
            panic!("poison the live map");
        })
        .join()
        .unwrap_err();
        assert!(coord.live.is_poisoned());

        coord.register_hook("bash", Some(1.0), None);
        assert!(coord.set_agent_tool_timeout("agent", "bash", Some(2.0)));
        assert_eq!(coord.resolve_timeout("agent", "bash", None), Some(2.0));

        let out = coord
            .execute(call("c1"), "agent", None, |_cancel| async {
                ToolResponse::text("c1", "ok-after-poison", ToolState::Success)
            })
            .await;
        assert_eq!(out.state, ToolState::Success);
        assert_eq!(out.joined_text(), "ok-after-poison");
        assert!(!coord.cancel("c1", CancelReason::User));
    }

    #[tokio::test]
    async fn cancel_interrupts() {
        let coord = ToolCoordinator::new(None);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let exec = coord.execute(call("c2"), "agent", None, move |cancel| async move {
            let _ = tx.send(());
            cancel.cancelled().await;
            ToolResponse::text("c2", "cancelled-inside", ToolState::Interrupted)
        });
        tokio::pin!(exec);
        tokio::select! {
            biased;
            _ = &mut exec => panic!("tool finished before cancel"),
            _ = rx => {}
        }
        assert!(coord.cancel("c2", CancelReason::User));
        let out = exec.await;
        assert_eq!(out.state, ToolState::Interrupted);
        assert!(out.joined_text().contains("cancelled"));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_kills_when_offload_disabled() {
        let coord = ToolCoordinator::new(Some(1.0));
        let exec = coord.execute(call("c3"), "agent", None, |cancel| async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    ToolResponse::text("c3", "saw-cancel", ToolState::Interrupted)
                }
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    ToolResponse::text("c3", "finished", ToolState::Success)
                }
            }
        });
        tokio::pin!(exec);
        tokio::time::advance(Duration::from_millis(1_100)).await;
        let out = exec.await;
        assert_eq!(out.state, ToolState::Interrupted);
        assert!(out.joined_text().contains("timeout"));
    }

    #[tokio::test(start_paused = true)]
    async fn offload_keeps_task_running_and_delivers() {
        let coord = ToolCoordinator::new(Some(80.0));
        coord.set_offload_on_deadline(true);
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let exec = coord.execute(call("c4"), "agent", None, move |_cancel| async move {
            let _ = go_rx.await;
            ToolResponse::text("c4", "late", ToolState::Success)
        });
        tokio::pin!(exec);
        tokio::select! {
            biased;
            _ = &mut exec => panic!("tool finished before offload"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::advance(Duration::from_secs(41)).await;
        let out = exec.await;
        assert!(out.offloaded, "{out:?}");
        assert_eq!(out.state, ToolState::Success);
        assert!(out.joined_text().contains("running in background"));
        assert!(
            lock_unpoison(&coord.live).contains_key("c4"),
            "offloaded call must stay registered"
        );
        let _ = go_tx.send(());
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let done = coord.take_finished();
        assert_eq!(done.len(), 1, "{done:?}");
        assert_eq!(done[0].0, "bash");
        assert_eq!(done[0].1.joined_text(), "late");
        assert_eq!(done[0].1.state, ToolState::Success);
        assert!(!coord.cancel("c4", CancelReason::User));
    }

    #[tokio::test(start_paused = true)]
    async fn offload_then_cancel_posts_interrupted() {
        let coord = ToolCoordinator::new(Some(80.0));
        coord.set_offload_on_deadline(true);
        let exec = coord.execute(call("c6"), "agent", None, |cancel| async move {
            cancel.cancelled().await;
            ToolResponse::text("c6", "saw-cancel", ToolState::Interrupted)
        });
        tokio::pin!(exec);
        tokio::select! {
            biased;
            _ = &mut exec => panic!("tool finished before offload"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::advance(Duration::from_secs(41)).await;
        let out = exec.await;
        assert!(out.offloaded);
        assert!(coord.cancel("c6", CancelReason::User));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let done = coord.take_finished();
        assert_eq!(done.len(), 1, "{done:?}");
        assert_eq!(done[0].1.state, ToolState::Interrupted);
    }

    #[tokio::test(start_paused = true)]
    async fn offload_then_kill_posts_timeout() {
        let coord = ToolCoordinator::new(Some(80.0));
        coord.set_offload_on_deadline(true);
        let exec = coord.execute(call("c5"), "agent", None, |cancel| async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    ToolResponse::text("c5", "saw-cancel", ToolState::Interrupted)
                }
                _ = tokio::time::sleep(Duration::from_secs(10_000)) => {
                    ToolResponse::text("c5", "late", ToolState::Success)
                }
            }
        });
        tokio::pin!(exec);
        tokio::select! {
            biased;
            _ = &mut exec => panic!("tool finished before offload"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::advance(Duration::from_secs(41)).await;
        let out = exec.await;
        assert!(out.offloaded);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(50)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let done = coord.take_finished();
        assert_eq!(done.len(), 1, "{done:?}");
        assert_eq!(done[0].1.state, ToolState::Interrupted);
        assert!(done[0].1.joined_text().contains("timeout"));
    }
}
