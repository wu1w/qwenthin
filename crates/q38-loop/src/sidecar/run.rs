//! Shared agent turn for sidecar / TUI / web. Frozen `tools[]` — vision goes
//! through `ChatMessage.parts`, not a new OpenAI tool.

use crate::config::Config;
use crate::session::SessionMode;
use crate::template::ChatMessage;
use crate::{Agent, HttpCompleter, RunOpts, ToolSet};

use super::types::{TurnRequest, TurnResult};

pub async fn execute_turn(
    cfg: Config,
    agents_md: bool,
    agents_md_head: bool,
    req: TurnRequest,
) -> TurnResult {
    if req.cancel.is_cancelled() {
        return TurnResult::aborted();
    }

    let mut cfg = cfg;
    if !req.snapshot.model.is_empty()
        && std::env::var("Q38_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
    {
        cfg.server.model = req.snapshot.model.clone();
    }

    let mut opts = RunOpts::from_config(&cfg, req.snapshot.workspace.clone());
    opts.print = false;
    opts.session_id = if req.snapshot.session_id.is_empty() {
        "q38".into()
    } else {
        req.snapshot.session_id.clone()
    };
    opts.agents_md = agents_md;
    opts.agents_md_head = agents_md_head;
    match req.snapshot.mode {
        SessionMode::Chat => {
            opts.with_tools = false;
            opts.tool_set = ToolSet::None;
        }
        SessionMode::Code => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Code;
        }
        SessionMode::Think => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Agent;
            opts.max_steps = cfg.policy.max_steps_think;
        }
        SessionMode::Agent => {
            opts.with_tools = true;
            opts.tool_set = ToolSet::Agent;
        }
    }
    opts.generation_reserve = req
        .snapshot
        .policy
        .max_tokens
        .saturating_add(req.snapshot.policy.max_think_tokens);
    opts.effort_locked = req.snapshot.effort_locked
        || matches!(req.snapshot.mode, SessionMode::Think | SessionMode::Chat)
        || !req.snapshot.policy.enabled;
    opts.persist_session = req.persist;
    opts.session_mode = req.snapshot.mode;
    opts.plan_mode = req.snapshot.plan_mode;
    opts.low_precision = req.snapshot.low_precision;
    opts.confined = req.snapshot.workspace_confined;
    opts.permit = req.permit;

    let cancel = req.cancel.clone();
    let steer = req.steer.clone();
    let messages = req.messages;
    let persist = req.persist;
    let prompt = req.prompt;
    let parts = req.parts;
    let policy = req.snapshot.policy;
    let completer = tokio::select! {
        biased;
        _ = cancel.cancelled() => return TurnResult::aborted(),
        c = HttpCompleter::connect(&cfg, policy.clone()) => match c {
            Ok(c) => c,
            Err(e) => {
                return if cancel.is_cancelled() {
                    TurnResult::aborted()
                } else {
                    TurnResult::fail(e.to_string())
                };
            }
        },
    };
    let mut agent = match Agent::new(completer, opts) {
        Ok(a) => a,
        Err(e) => return TurnResult::fail(e.to_string()),
    };
    agent.set_cancel(cancel.clone());
    agent.set_steer(steer);
    agent.set_emit(req.emit);
    let out = if persist || messages.is_empty() {
        if parts.is_empty() {
            agent.run(&prompt).await
        } else {
            let text = if prompt.trim().is_empty() {
                " "
            } else {
                prompt.as_str()
            };
            let mut msg = ChatMessage::user(text);
            msg.parts = parts;
            agent.run_message(msg).await
        }
    } else {
        agent.load_messages(messages);
        agent.drive().await
    };
    match out {
        Ok(out) => TurnResult {
            text: out.text,
            stop_reason: out.stop_reason.clone(),
            aborted: out.stop_reason.as_deref() == Some("aborted"),
            error: None,
            events: Vec::new(),
            pending_steer: out.pending_steer,
            streamed: true,
        },
        Err(e) => {
            if cancel.is_cancelled() {
                TurnResult::aborted()
            } else {
                TurnResult::fail(e.to_string())
            }
        }
    }
}
