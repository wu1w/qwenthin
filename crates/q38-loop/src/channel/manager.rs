//! Per-session queue + consume. Wash of QwenPaw `UnifiedQueueManager` +
//! `BaseChannel.consume_one` (debounce, then one worker per session).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;

use crate::error::Result;

use super::access::{self, GateDecision};
use super::envelope::NativePayload;
use super::router::SessionRouter;
use super::ChannelsConfig;

const DEBOUNCE: Duration = Duration::from_millis(300);
const QUEUE_CAP: usize = 64;

pub struct IngestResult {
    pub session_id: String,
    pub denied: Option<&'static str>,
}

#[derive(Clone)]
pub struct ChannelManager {
    inner: Arc<Mutex<Inner>>,
    ingest_tx: mpsc::Sender<NativePayload>,
}

struct Inner {
    cfg: ChannelsConfig,
    router: SessionRouter,
    sessions: HashMap<String, mpsc::Sender<NativePayload>>,
}

pub trait ChannelHandler: Send + Sync + 'static {
    fn handle(
        &self,
        env: NativePayload,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<super::envelope::ContentPart>>> + Send>>;
}

impl<F, Fut> ChannelHandler for F
where
    F: Fn(NativePayload) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<super::envelope::ContentPart>>> + Send + 'static,
{
    fn handle(
        &self,
        env: NativePayload,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<super::envelope::ContentPart>>> + Send>> {
        Box::pin((self)(env))
    }
}

impl ChannelManager {
    pub fn start<H>(cfg: ChannelsConfig, router: SessionRouter, handler: H) -> Self
    where
        H: ChannelHandler,
    {
        let (ingest_tx, ingest_rx) = mpsc::channel::<NativePayload>(256);
        let inner = Arc::new(Mutex::new(Inner {
            cfg,
            router,
            sessions: HashMap::new(),
        }));
        let mgr = Self {
            inner: inner.clone(),
            ingest_tx,
        };
        tokio::spawn(dispatch_loop(inner, ingest_rx, Arc::new(handler)));
        mgr
    }

    pub async fn ingest(&self, mut env: NativePayload) -> Result<IngestResult> {
        let ep = {
            let g = self.inner.lock().await;
            g.cfg
                .endpoint_for(&env.channel)
                .cloned()
                .or_else(|| g.cfg.endpoints.iter().find(|e| e.enabled).cloned())
        };
        if let Some(ep) = ep.as_ref() {
            if let GateDecision::Deny(why) = access::admit(ep, &env) {
                return Ok(IngestResult {
                    session_id: String::new(),
                    denied: Some(why),
                });
            }
            if env.channel.is_empty() {
                env.channel = if ep.kind.is_empty() {
                    ep.id.clone()
                } else {
                    ep.kind.clone()
                };
            }
        }
        let session_id = {
            let mut g = self.inner.lock().await;
            g.router.resolve(&env)?
        };
        env.session_id = session_id.clone();
        self.ingest_tx
            .send(env)
            .await
            .map_err(|_| crate::error::Error::msg("channel ingest closed"))?;
        Ok(IngestResult {
            session_id,
            denied: None,
        })
    }
}

async fn dispatch_loop<H: ChannelHandler>(
    inner: Arc<Mutex<Inner>>,
    mut rx: mpsc::Receiver<NativePayload>,
    handler: Arc<H>,
) {
    let mut pending: HashMap<String, Vec<NativePayload>> = HashMap::new();
    let mut ticks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let (flush_tx, mut flush_rx) = mpsc::unbounded_channel::<String>();

    loop {
        tokio::select! {
            env = rx.recv() => {
                // flush_tx 被本任务自己持有，flush_rx 永远不会关闭，原来的
                // else 分支永不触发。ingest 端全部丢弃（serve_endpoint 被
                // abort / manager 被丢弃）时这里拿到 None，直接退出。
                let Some(env) = env else { break };
                let key = env.session_id.clone();
                pending.entry(key.clone()).or_default().push(env);
                if let Some(old) = ticks.remove(&key) {
                    old.abort();
                }
                let tx = flush_tx.clone();
                ticks.insert(key.clone(), tokio::spawn(async move {
                    sleep(DEBOUNCE).await;
                    let _ = tx.send(key);
                }));
            }
            Some(key) = flush_rx.recv() => {
                ticks.remove(&key);
                let Some(batch) = pending.remove(&key) else { continue };
                let Some(merged) = NativePayload::merge(batch) else { continue };
                let tx = session_tx(&inner, &merged.session_id, handler.clone()).await;
                let _ = tx.send(merged).await;
            }
        }
    }
    for (_, tick) in ticks {
        tick.abort();
    }
}

async fn session_tx<H: ChannelHandler>(
    inner: &Arc<Mutex<Inner>>,
    session_id: &str,
    handler: Arc<H>,
) -> mpsc::Sender<NativePayload> {
    let mut g = inner.lock().await;
    if let Some(tx) = g.sessions.get(session_id) {
        return tx.clone();
    }
    let (tx, rx) = mpsc::channel::<NativePayload>(QUEUE_CAP);
    let cfg = g.cfg.clone();
    g.sessions.insert(session_id.to_string(), tx.clone());
    drop(g);
    let inner = inner.clone();
    let sid = session_id.to_string();
    tokio::spawn(async move {
        session_worker(rx, handler, cfg).await;
        let mut g = inner.lock().await;
        g.sessions.remove(&sid);
    });
    tx
}

async fn session_worker<H: ChannelHandler>(
    mut rx: mpsc::Receiver<NativePayload>,
    handler: Arc<H>,
    cfg: ChannelsConfig,
) {
    while let Some(env) = rx.recv().await {
        let ep = cfg.endpoint_for(&env.channel).cloned();
        match handler.handle(env.clone()).await {
            Ok(parts) => {
                if let Err(e) = super::outbound::deliver(ep.as_ref(), &env, &parts).await {
                    eprintln!("q38 channel deliver: {e}");
                }
            }
            Err(e) => {
                let parts = super::outbound::reply_text(format!("error: {e}"));
                let _ = super::outbound::deliver(ep.as_ref(), &env, &parts).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_loop_exits_when_ingest_side_drops() {
        let dir = std::env::temp_dir().join(format!("q38-chan-{}", uuid::Uuid::new_v4().simple()));
        let router = SessionRouter::open(dir.join("routes.json")).unwrap();
        let inner = Arc::new(Mutex::new(Inner {
            cfg: ChannelsConfig::default(),
            router,
            sessions: HashMap::new(),
        }));
        let (tx, rx) = mpsc::channel::<NativePayload>(4);
        let handler = Arc::new(|_env: NativePayload| async move {
            Ok(Vec::<super::super::envelope::ContentPart>::new())
        });
        let task = tokio::spawn(dispatch_loop(inner, rx, handler));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("dispatch_loop must exit once all ingest senders drop")
            .unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
