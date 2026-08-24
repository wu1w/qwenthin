//! NDJSON JSON-RPC serve loop for `q38 --sidecar`.

use std::future::{pending, Future};

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::Result;
use crate::media::MediaPart;
use crate::session::{derive_messages, SessionEvent};
use crate::tool_calls::CancelFlag;

use super::types::{Dispatch, EventSink, RpcError, RpcRequest, TurnRequest, TurnResult, JSONRPC};
use super::SidecarSession;

/// Read NDJSON requests from `reader`, write responses/notifications to `writer`.
///
/// `on_turn` is the CLI/`Agent::run` callback. The library does not talk HTTP.
pub async fn serve_rpc<R, W, F, Fut>(
    reader: R,
    mut writer: W,
    mut session: SidecarSession,
    on_turn: F,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn(TurnRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = TurnResult> + Send + 'static,
{
    let mut lines = reader.lines();
    let on_turn = std::sync::Arc::new(on_turn);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let mut live: Option<LiveTurn> = None;
    let mut pending_follow_id: Option<Value> = None;

    loop {
        tokio::select! {
            biased;
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        handle_line(
                            &mut session,
                            &mut writer,
                            &mut live,
                            &on_turn,
                            &ev_tx,
                            &mut pending_follow_id,
                            &line,
                        ).await?;
                    }
                    Ok(None) => {
                        if let Some(turn) = live.take() {
                            turn.cancel.cancel();
                            turn.join.abort();
                            let _ = turn.join.await;
                        }
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Some(event) = ev_rx.recv() => {
                if !session.persist {
                    session.record(event.clone());
                }
                write_line(&mut writer, &encode_notification(&event)).await?;
            }
            done = await_turn(&mut live) => {
                finish_rpc_turn(
                    &mut session,
                    &mut writer,
                    &mut live,
                    &on_turn,
                    &ev_tx,
                    &mut pending_follow_id,
                    done,
                ).await?;
            }
        }
    }
    Ok(())
}

struct LiveTurn {
    rpc_id: Option<Value>,
    cancel: CancelFlag,
    join: tokio::task::JoinHandle<TurnResult>,
}

async fn await_turn(live: &mut Option<LiveTurn>) -> (Option<Value>, TurnResult) {
    let Some(turn) = live.as_mut() else {
        pending::<()>().await;
        unreachable!("pending never resolves");
    };
    let result = (&mut turn.join)
        .await
        .unwrap_or_else(|_| TurnResult::aborted());
    let id = live.take().and_then(|t| t.rpc_id);
    (id, result)
}

async fn handle_line<W, F, Fut>(
    session: &mut SidecarSession,
    writer: &mut W,
    live: &mut Option<LiveTurn>,
    on_turn: &std::sync::Arc<F>,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    pending_follow_id: &mut Option<Value>,
    line: &str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    F: Fn(TurnRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = TurnResult> + Send + 'static,
{
    let req = match parse_request_line(line) {
        Ok(req) => req,
        Err(err) => {
            return write_line(writer, &encode_error(Value::Null, &err)).await;
        }
    };
    match session.handle(&req) {
        Dispatch::Result { result, events } => {
            emit_events(writer, &events).await?;
            reply(writer, req.id, Ok(result)).await
        }
        Dispatch::Error(err) => reply(writer, req.id, Err(err)).await,
        Dispatch::AbortClear { cleared } => {
            if let Some(turn) = live.as_ref() {
                turn.cancel.cancel();
            }
            reply(writer, req.id, Ok(json!({"ok": true, "cleared": cleared}))).await
        }
        Dispatch::Abort => {
            let redirect = session.has_redirect();
            if let Some(turn) = live.as_ref() {
                turn.cancel.cancel();
                if redirect {
                    *pending_follow_id = req.id;
                    return Ok(());
                }
                return reply(writer, req.id, Ok(json!({"ok": true}))).await;
            }
            if let Some(prompt) = session.pop_follow_up() {
                spawn_turn(
                    session,
                    writer,
                    live,
                    on_turn,
                    ev_tx,
                    req.id,
                    prompt,
                    Vec::new(),
                )
                .await
            } else {
                reply(writer, req.id, Ok(json!({"ok": true}))).await
            }
        }
        Dispatch::TurnStart { prompt, parts } => {
            spawn_turn(session, writer, live, on_turn, ev_tx, req.id, prompt, parts).await
        }
    }
}

async fn spawn_turn<W, F, Fut>(
    session: &mut SidecarSession,
    writer: &mut W,
    live: &mut Option<LiveTurn>,
    on_turn: &std::sync::Arc<F>,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    rpc_id: Option<Value>,
    prompt: String,
    parts: Vec<MediaPart>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    F: Fn(TurnRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = TurnResult> + Send + 'static,
{
    session.maybe_autotitle(&prompt);
    session.turn_in_flight = true;
    let user = SessionEvent::user(&prompt);
    if !session.persist {
        session.record(user.clone());
    }
    write_line(writer, &encode_notification(&user)).await?;
    let messages = if session.persist {
        Vec::new()
    } else {
        derive_messages(session.events())
    };
    let cancel = CancelFlag::new();
    let req_turn = TurnRequest {
        prompt,
        parts,
        snapshot: session.snapshot(),
        cancel: cancel.clone(),
        emit: EventSink { tx: ev_tx.clone() },
        messages,
        steer: session.steer_slot(),
        persist: session.persist,
        permit: None,
        clarify: None,
    };
    let on_turn = on_turn.clone();
    let join = tokio::spawn(async move { on_turn(req_turn).await });
    *live = Some(LiveTurn {
        rpc_id,
        cancel,
        join,
    });
    Ok(())
}

async fn finish_rpc_turn<W, F, Fut>(
    session: &mut SidecarSession,
    writer: &mut W,
    live: &mut Option<LiveTurn>,
    on_turn: &std::sync::Arc<F>,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    pending_follow_id: &mut Option<Value>,
    done: (Option<Value>, TurnResult),
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    F: Fn(TurnRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = TurnResult> + Send + 'static,
{
    let (id, result) = done;
    let events = session.finish_turn(&result);
    emit_events(writer, &events).await?;
    if let Some(err) = result.error {
        reply(writer, id, Err(RpcError::internal(err))).await?;
    } else {
        reply(
            writer,
            id,
            Ok(json!({"ok": true, "aborted": result.aborted})),
        )
        .await?;
    }
    if let Some(prompt) = session.pop_follow_up() {
        let follow_id = pending_follow_id.take();
        spawn_turn(
            session,
            writer,
            live,
            on_turn,
            ev_tx,
            follow_id,
            prompt,
            Vec::new(),
        )
        .await?;
    }
    Ok(())
}

async fn emit_events<W: AsyncWrite + Unpin>(writer: &mut W, events: &[SessionEvent]) -> Result<()> {
    for event in events {
        write_line(writer, &encode_notification(event)).await?;
    }
    Ok(())
}

async fn reply<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Option<Value>,
    outcome: std::result::Result<Value, RpcError>,
) -> Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    let line = match outcome {
        Ok(result) => encode_response(id, result),
        Err(err) => encode_error(id, &err),
    };
    write_line(writer, &line).await
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> Result<()> {
    if let Err(e) = async {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    }
    .await
    {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(e.into());
    }
    Ok(())
}

pub fn parse_request_line(line: &str) -> std::result::Result<RpcRequest, RpcError> {
    let req: RpcRequest =
        serde_json::from_str(line.trim()).map_err(|e| RpcError::parse(e.to_string()))?;
    if req.method.is_empty() {
        return Err(RpcError::invalid_request("method is required"));
    }
    Ok(req)
}

pub fn encode_response(id: Value, result: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": JSONRPC,
        "id": id,
        "result": result,
    }))
    .expect("response json")
}

pub fn encode_error(id: Value, err: &RpcError) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": JSONRPC,
        "id": id,
        "error": err,
    }))
    .expect("error json")
}

pub fn encode_notification(event: &SessionEvent) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": JSONRPC,
        "method": "event.append",
        "params": event,
    }))
    .expect("notification json")
}
