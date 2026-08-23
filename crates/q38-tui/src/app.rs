//! Fullscreen event loop. Grok pager `event_loop`: select on TTY, agent
//! events, and a tick while thinking is streaming.

use std::io::{stdout, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use q38_loop::config::Config;
use q38_loop::permit::{ApprovalMode, PermitHub};
use q38_loop::policy::ThinkPolicy;
use q38_loop::session::{catalog, new_session_id, SessionEvent, SessionLog, SessionMode};
use q38_loop::sidecar::{
    Dispatch, EventSink, PolicyCaps, RpcRequest, SidecarOpts, SidecarSession, TurnRequest,
};
use q38_loop::{CancelFlag, MediaPart};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::overlay::{Overlay, OverlayAction};
use crate::prompt::{Prompt, PromptAction};
use crate::transcript::Transcript;
use crate::turn::execute_turn;

pub struct TuiOpts {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub session_id: String,
    pub mode: SessionMode,
    pub policy: ThinkPolicy,
    pub effort_locked: bool,
    pub agents_md: bool,
    pub agents_md_head: bool,
}

struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

struct LiveTurn {
    join: JoinHandle<q38_loop::sidecar::TurnResult>,
    cancel: CancelFlag,
}

pub async fn run(opts: TuiOpts) -> Result<ExitCode> {
    if !std::io::stdin().is_terminal() || !stdout().is_terminal() {
        anyhow::bail!("q38 TUI needs a TTY (use --print for oneshot)");
    }

    let mut session = SidecarSession::new(SidecarOpts {
        session_id: if opts.session_id.is_empty() {
            SessionLog::sessions_dir()
                .ok()
                .map(|dir| catalog::resume_console_id(&dir, ""))
                .unwrap_or_else(new_session_id)
        } else {
            opts.session_id.clone()
        },
        workspace: opts.workspace.clone(),
        mode: opts.mode,
        policy: opts.policy.clone(),
        caps: PolicyCaps::from_config(&opts.cfg),
        persist: true,
        effort_locked: opts.effort_locked,
        model: opts.cfg.server.model.clone(),
        family: opts.cfg.server.family,
        window: opts.cfg.context.working_window,
        busy: opts.cfg.channels.busy_policy(),
        channels: opts.cfg.channels.clone(),
        channel: "cli".into(),
        low_precision: opts.cfg.policy.low_precision,
        approvals: ApprovalMode::parse(&opts.cfg.features.approvals).unwrap_or(ApprovalMode::Ask),
    });
    match session.handle(&rpc(
        1,
        "session.open",
        json!({
            "session": session.snapshot().session_id,
            "workspace": opts.workspace.display().to_string(),
            "mode": opts.mode.as_str(),
        }),
    )) {
        Dispatch::Result { .. } => {}
        Dispatch::Error(err) => anyhow::bail!("{}", err.message),
        other => anyhow::bail!("session.open: {other:?}"),
    }

    let mut transcript = Transcript::default();
    for event in session.events() {
        transcript.apply(event);
    }
    let snap = session.snapshot();
    transcript.push_system(format!(
        "q38  {}  {}  /setup  /plan  /approvals  /help",
        snap.model,
        snap.mode.as_str()
    ));

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = RawGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut prompt = Prompt::default();
    let mut events = EventStream::new();
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let (permit, mut permit_rx) = PermitHub::pair(session.approvals());
    let mut overlay: Option<Overlay> = None;
    let mut live: Option<LiveTurn> = None;
    let mut scroll: i32 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let mut cfg = opts.cfg.clone();
    let agents_md = opts.agents_md;
    let agents_md_head = opts.agents_md_head;

    loop {
        let running = live.is_some();
        terminal.draw(|f| {
            draw(
                f,
                &transcript,
                &prompt,
                overlay.as_ref(),
                &session,
                running,
                &mut scroll,
            );
        })?;

        tokio::select! {
            biased;
            Some(event) = ev_rx.recv() => {
                transcript.apply(&event);
            }
            Some(req) = permit_rx.recv() => {
                overlay = Some(Overlay::Permit(req));
            }
            _ = tick.tick(), if running => {}
            maybe = events.next() => {
                let Some(Ok(event)) = maybe else {
                    break;
                };
                match event {
                    Event::Key(key) => {
                        if handle_key(
                            key,
                            &mut prompt,
                            &mut overlay,
                            &mut transcript,
                            &mut session,
                            &mut live,
                            &mut scroll,
                            &ev_tx,
                            &mut cfg,
                            &permit,
                            agents_md,
                            agents_md_head,
                        )
                        .await?
                        {
                            break;
                        }
                    }
                    Event::Paste(s) => prompt.insert_str(&s),
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            transcript.follow = false;
                            scroll = scroll.saturating_sub(1);
                        }
                        MouseEventKind::ScrollDown => {
                            scroll = scroll.saturating_add(1);
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            result = await_turn(&mut live) => {
                if let Some(result) = result {
                    let extra = session.finish_turn(&result);
                    for e in extra {
                        transcript.apply(&e);
                    }
                    if let Some(err) = result.error {
                        transcript.push_system(err);
                    } else if session.plan_mode()
                        && !result.aborted
                        && !result.text.trim().is_empty()
                    {
                        overlay = Some(Overlay::PlanReview);
                    }
                    if let Some(next) = session.pop_follow_up() {
                        start_turn(
                            &mut session,
                            &mut transcript,
                            &mut live,
                            &ev_tx,
                            cfg.clone(),
                            Some(permit.clone()),
                            agents_md,
                            agents_md_head,
                            next,
                            Vec::new(),
                            false,
                        );
                    }
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn await_turn(live: &mut Option<LiveTurn>) -> Option<q38_loop::sidecar::TurnResult> {
    if live.is_none() {
        std::future::pending::<()>().await;
        return None;
    }
    let result = {
        let slot = live.as_mut().expect("live");
        (&mut slot.join).await
    };
    *live = None;
    match result {
        Ok(result) => Some(result),
        Err(_) => Some(q38_loop::sidecar::TurnResult::fail("turn task failed")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_key(
    key: KeyEvent,
    prompt: &mut Prompt,
    overlay: &mut Option<Overlay>,
    transcript: &mut Transcript,
    session: &mut SidecarSession,
    live: &mut Option<LiveTurn>,
    scroll: &mut i32,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    cfg: &mut Config,
    permit: &PermitHub,
    agents_md: bool,
    agents_md_head: bool,
) -> Result<bool> {
    if let Some(layer) = overlay.as_mut() {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            layer.abort();
            *overlay = None;
            prompt.clear();
            if let Some(slot) = live {
                slot.cancel.cancel();
            }
            let _ = session.handle(&rpc(0, "turn.abort", json!({})));
            return Ok(false);
        }
        match layer.handle_key(key, prompt, cfg) {
            OverlayAction::None => return Ok(false),
            OverlayAction::Close => {
                *overlay = None;
                prompt.clear();
                return Ok(false);
            }
            OverlayAction::Saved { cfg: next, note } => {
                *cfg = next;
                session.set_model(&cfg.server.model);
                transcript.push_system(note);
                *overlay = None;
                prompt.clear();
                return Ok(false);
            }
            OverlayAction::Implement => {
                *overlay = None;
                prompt.clear();
                submit(
                    session,
                    transcript,
                    live,
                    overlay,
                    prompt,
                    ev_tx,
                    cfg,
                    permit,
                    agents_md,
                    agents_md_head,
                    "/plan go".into(),
                );
                return Ok(false);
            }
            OverlayAction::ExitPlan => {
                *overlay = None;
                prompt.clear();
                submit(
                    session,
                    transcript,
                    live,
                    overlay,
                    prompt,
                    ev_tx,
                    cfg,
                    permit,
                    agents_md,
                    agents_md_head,
                    "/plan off".into(),
                );
                return Ok(false);
            }
        }
    }

    let running = live.is_some();
    match prompt.handle_key(key, running) {
        PromptAction::Quit => return Ok(true),
        PromptAction::Cancel => {}
        PromptAction::Interrupt => {
            if let Some(slot) = live {
                slot.cancel.cancel();
            }
            let _ = session.handle(&rpc(0, "turn.abort", json!({})));
        }
        PromptAction::ToggleThink => transcript.cycle_last_think(),
        PromptAction::Follow => {
            transcript.follow = true;
            *scroll = 0;
        }
        PromptAction::Scroll { delta } => {
            transcript.follow = false;
            *scroll = scroll.saturating_add(delta);
        }
        PromptAction::Submit => {
            let text = prompt.take();
            if matches!(text.trim(), "/quit" | "/exit") {
                return Ok(true);
            }
            submit(
                session,
                transcript,
                live,
                overlay,
                prompt,
                ev_tx,
                cfg,
                permit,
                agents_md,
                agents_md_head,
                text,
            );
        }
        PromptAction::None => {}
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn submit(
    session: &mut SidecarSession,
    transcript: &mut Transcript,
    live: &mut Option<LiveTurn>,
    overlay: &mut Option<Overlay>,
    prompt: &mut Prompt,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    cfg: &mut Config,
    permit: &PermitHub,
    agents_md: bool,
    agents_md_head: bool,
    text: String,
) {
    let trimmed = text.trim();
    if trimmed == "/setup" || trimmed.eq_ignore_ascii_case("/model setup") {
        *overlay = Some(Overlay::setup(cfg, prompt));
        return;
    }
    if text.trim().starts_with('/') {
        match session.handle(&rpc(2, "slash", json!({"text": text}))) {
            Dispatch::Error(err) if err.message.contains("unknown slash") => {
                start_turn(
                    session,
                    transcript,
                    live,
                    ev_tx,
                    cfg.clone(),
                    Some(permit.clone()),
                    agents_md,
                    agents_md_head,
                    text,
                    Vec::new(),
                    true,
                );
            }
            Dispatch::Error(err) => transcript.push_system(err.message),
            other => {
                permit.set_mode(session.approvals());
                apply_dispatch(
                    session,
                    transcript,
                    live,
                    ev_tx,
                    cfg.clone(),
                    Some(permit.clone()),
                    agents_md,
                    agents_md_head,
                    other,
                );
            }
        }
        return;
    }
    start_turn(
        session,
        transcript,
        live,
        ev_tx,
        cfg.clone(),
        Some(permit.clone()),
        agents_md,
        agents_md_head,
        text,
        Vec::new(),
        true,
    );
}

#[allow(clippy::too_many_arguments)]
fn apply_dispatch(
    session: &mut SidecarSession,
    transcript: &mut Transcript,
    live: &mut Option<LiveTurn>,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    cfg: Config,
    permit: Option<PermitHub>,
    agents_md: bool,
    agents_md_head: bool,
    dispatch: Dispatch,
) {
    match dispatch {
        Dispatch::Result { result, events } => {
            for e in &events {
                transcript.apply(e);
            }
            if events.is_empty() {
                if let Some(text) = result.get("text").and_then(|v| v.as_str()) {
                    transcript.push_system(text);
                } else if result.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                    if let Some(session_id) = result.get("session").and_then(|v| v.as_str()) {
                        transcript.push_system(format!("session {session_id}"));
                    }
                }
            }
        }
        Dispatch::Error(err) => transcript.push_system(err.message),
        Dispatch::TurnStart { prompt, parts } => start_turn(
            session,
            transcript,
            live,
            ev_tx,
            cfg,
            permit,
            agents_md,
            agents_md_head,
            prompt,
            parts,
            true,
        ),
        Dispatch::Abort | Dispatch::AbortClear { .. } => {
            if let Some(slot) = live {
                slot.cancel.cancel();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_turn(
    session: &mut SidecarSession,
    transcript: &mut Transcript,
    live: &mut Option<LiveTurn>,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    cfg: Config,
    permit: Option<PermitHub>,
    agents_md: bool,
    agents_md_head: bool,
    prompt: String,
    parts: Vec<MediaPart>,
    paint_user: bool,
) {
    if live.is_some() {
        match session.handle(&rpc(3, "turn.queue", json!({"prompt": prompt}))) {
            Dispatch::Result { result, .. } => {
                if let Some(text) = result.get("text").and_then(|v| v.as_str()) {
                    transcript.push_system(text);
                } else {
                    transcript.push_system("queued");
                }
            }
            Dispatch::TurnStart { prompt, parts } => start_turn(
                session,
                transcript,
                live,
                ev_tx,
                cfg,
                permit,
                agents_md,
                agents_md_head,
                prompt,
                parts,
                true,
            ),
            Dispatch::Error(err) => transcript.push_system(err.message),
            _ => {}
        }
        return;
    }
    if session.turn_in_flight() {
        spawn_live(
            session,
            transcript,
            live,
            ev_tx,
            cfg,
            permit,
            agents_md,
            agents_md_head,
            prompt,
            parts,
            paint_user,
        );
        return;
    }
    let params = if parts.is_empty() {
        json!({"prompt": prompt})
    } else {
        json!({
            "prompt": prompt,
            "content_parts": parts.iter().map(|p| json!({
                "type": p.kind.as_str(),
                "url": p.url,
                "mime": p.mime,
            })).collect::<Vec<_>>(),
        })
    };
    match session.handle(&rpc(3, "turn.start", params)) {
        Dispatch::TurnStart { prompt, parts } => spawn_live(
            session,
            transcript,
            live,
            ev_tx,
            cfg,
            permit,
            agents_md,
            agents_md_head,
            prompt,
            parts,
            paint_user,
        ),
        Dispatch::Result { result, events } => {
            for e in &events {
                transcript.apply(e);
            }
            if let Some(text) = result.get("text").and_then(|v| v.as_str()) {
                transcript.push_system(text);
            }
        }
        Dispatch::Error(err) => transcript.push_system(err.message),
        other => apply_dispatch(
            session,
            transcript,
            live,
            ev_tx,
            cfg,
            permit,
            agents_md,
            agents_md_head,
            other,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_live(
    session: &mut SidecarSession,
    transcript: &mut Transcript,
    live: &mut Option<LiveTurn>,
    ev_tx: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    cfg: Config,
    permit: Option<PermitHub>,
    agents_md: bool,
    agents_md_head: bool,
    prompt: String,
    parts: Vec<MediaPart>,
    paint_user: bool,
) {
    if paint_user {
        transcript.push_user(&prompt);
    }
    let cancel = CancelFlag::new();
    let req = TurnRequest {
        prompt,
        parts,
        snapshot: session.snapshot(),
        cancel: cancel.clone(),
        emit: EventSink::new(ev_tx.clone()),
        messages: Vec::new(),
        steer: session.steer_slot(),
        persist: true,
        permit,
    };
    let join = tokio::spawn(async move { execute_turn(cfg, agents_md, agents_md_head, req).await });
    *live = Some(LiveTurn { join, cancel });
}

fn draw(
    f: &mut ratatui::Frame,
    transcript: &Transcript,
    prompt: &Prompt,
    overlay: Option<&Overlay>,
    session: &SidecarSession,
    running: bool,
    scroll: &mut i32,
) {
    let composer_h = overlay
        .map(|o| o.height())
        .unwrap_or_else(|| prompt_height(prompt, f.area().width));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(composer_h),
            Constraint::Length(1),
        ])
        .split(f.area());

    let width = chunks[0].width.saturating_sub(2) as usize;
    let lines = transcript.render_lines(width.max(8));
    let view_h = chunks[0].height.saturating_sub(2) as usize;
    let total = lines.len();
    let max_off = total.saturating_sub(view_h);
    if transcript.follow {
        *scroll = max_off as i32;
    } else {
        *scroll = (*scroll).clamp(0, max_off as i32);
    }
    let start = *scroll as usize;
    let slice: Vec<Line> = lines.iter().skip(start).take(view_h).cloned().collect();
    let title = if running { " q38 · running " } else { " q38 " };
    f.render_widget(
        Paragraph::new(slice).block(Block::default().borders(Borders::TOP).title(title)),
        chunks[0],
    );

    if let Some(layer) = overlay {
        f.render_widget(layer.paragraph(prompt), chunks[1]);
    } else {
        let composer = format!("❯ {}", prompt.text());
        f.render_widget(
            Paragraph::new(composer).block(Block::default().borders(Borders::TOP)),
            chunks[1],
        );
    }

    let snap = session.snapshot();
    let effort = snap
        .policy
        .effort
        .map(|e| e.as_str())
        .unwrap_or(if snap.policy.enabled { "on" } else { "off" });
    let plan = if snap.plan_mode {
        "plan"
    } else {
        snap.mode.as_str()
    };
    let info = format!(
        " {plan} · {} · {} · {}{} · /setup /plan /approvals /lossy ",
        snap.approvals.as_str(),
        effort,
        &snap.session_id.chars().take(8).collect::<String>(),
        if snap.low_precision {
            " · 低精度"
        } else {
            ""
        }
    );
    f.render_widget(
        Paragraph::new(info).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn prompt_height(prompt: &Prompt, width: u16) -> u16 {
    let w = width.saturating_sub(4) as usize;
    let lines = crate::transcript::wrap(&format!("❯ {}", prompt.text()), w.max(8));
    (lines.len() as u16 + 1).clamp(2, 8)
}

fn rpc(id: u64, method: &str, params: Value) -> RpcRequest {
    RpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(id)),
        method: method.into(),
        params: Some(params),
    }
}
