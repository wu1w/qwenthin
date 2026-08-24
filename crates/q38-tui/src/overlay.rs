//! Grok-shaped composer overlays: `/setup`, permission ask, plan approve.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use q38_loop::clarify::{ClarifyDecision, ClarifyRequest};
use q38_loop::config::Config;
use q38_loop::permit::{PermitDecision, PermitRequest};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::prompt::Prompt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupStep {
    Url,
    Key,
    Model,
}

pub struct SetupWizard {
    step: SetupStep,
    url: String,
    key: String,
    model: String,
}

pub enum Overlay {
    Setup(SetupWizard),
    Permit(PermitRequest),
    Clarify {
        req: ClarifyRequest,
        other: Option<String>,
    },
    PlanReview,
}

pub enum OverlayAction {
    None,
    Close,
    Saved { cfg: Config, note: String },
    Implement,
    ExitPlan,
}

impl SetupWizard {
    pub fn new(cfg: &Config) -> Self {
        Self {
            step: SetupStep::Url,
            url: cfg.server.base_url.clone(),
            key: String::new(),
            model: cfg.server.model.clone(),
        }
    }

    fn title(&self) -> &'static str {
        match self.step {
            SetupStep::Url => "setup 1/3  endpoint  (https://host/v1)",
            SetupStep::Key => "setup 2/3  api_key  (empty = keep)",
            SetupStep::Model => "setup 3/3  model  (empty = GET /v1/models)",
        }
    }

    fn field(&self) -> &str {
        match self.step {
            SetupStep::Url => &self.url,
            SetupStep::Key => &self.key,
            SetupStep::Model => &self.model,
        }
    }

    fn commit_field(&mut self, value: String) {
        match self.step {
            SetupStep::Url => self.url = value,
            SetupStep::Key => self.key = value,
            SetupStep::Model => self.model = value,
        }
    }

    fn load_prompt(&self, prompt: &mut Prompt) {
        prompt.clear();
        let shown = match self.step {
            SetupStep::Key => String::new(),
            _ => self.field().to_string(),
        };
        prompt.insert_str(&shown);
    }
}

impl Overlay {
    pub fn setup(cfg: &Config, prompt: &mut Prompt) -> Self {
        let w = SetupWizard::new(cfg);
        w.load_prompt(prompt);
        Overlay::Setup(w)
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        prompt: &mut Prompt,
        cfg: &Config,
    ) -> OverlayAction {
        if key.kind != crossterm::event::KeyEventKind::Press
            && key.kind != crossterm::event::KeyEventKind::Repeat
        {
            return OverlayAction::None;
        }
        match self {
            Overlay::Setup(wiz) => handle_setup(wiz, key, prompt, cfg),
            Overlay::Permit(req) => handle_permit(req, key),
            Overlay::Clarify { req, other } => handle_clarify(req, other, key, prompt),
            Overlay::PlanReview => handle_plan(key),
        }
    }

    pub fn paragraph(&self, prompt: &Prompt) -> Paragraph<'static> {
        match self {
            Overlay::Setup(wiz) => {
                let body = match wiz.step {
                    SetupStep::Key => {
                        let n = prompt.text().chars().count();
                        format!("❯ {}", "*".repeat(n))
                    }
                    _ => format!("❯ {}", prompt.text()),
                };
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        wiz.title(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(body),
                    Line::from(Span::styled(
                        "enter next  esc cancel",
                        Style::default().add_modifier(Modifier::DIM),
                    )),
                ])
                .block(Block::default().borders(Borders::TOP).title(" setup "))
            }
            Overlay::Permit(req) => {
                let ask = &req.ask;
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        format!("allow `{}`", ask.tool),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(clip(&ask.preview, 80)),
                    Line::from(Span::styled(
                        "y allow   a always this tool   n/esc deny",
                        Style::default().add_modifier(Modifier::DIM),
                    )),
                ])
                .block(Block::default().borders(Borders::TOP).title(" permission "))
            }
            Overlay::PlanReview => Paragraph::new(vec![
                Line::from(Span::styled(
                    "plan ready",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from("y implement   n stay in plan   q exit plan"),
            ])
            .block(Block::default().borders(Borders::TOP).title(" plan ")),
            Overlay::Clarify { req, other } => {
                let ask = &req.ask;
                let mut lines = vec![
                    Line::from(Span::styled(
                        ask.title.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(clip(&ask.prompt, 80)),
                ];
                for (i, opt) in ask.options.iter().enumerate() {
                    let mark = if i == 0 { "*" } else { " " };
                    lines.push(Line::from(format!(
                        "{mark}{}. {}",
                        i + 1,
                        clip(&opt.label, 72)
                    )));
                }
                if other.is_some() {
                    lines.push(Line::from(format!("other ❯ {}", prompt.text())));
                    lines.push(Line::from(Span::styled(
                        "enter submit  esc back",
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        "1-4 pick  s skip  o other",
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
                Paragraph::new(lines).block(Block::default().borders(Borders::TOP).title(" ask "))
            }
        }
    }

    pub fn height(&self) -> u16 {
        match self {
            Overlay::Clarify { req, other } => {
                let extra = if other.is_some() { 2 } else { 1 };
                (3 + req.ask.options.len() as u16 + extra).clamp(5, 12)
            }
            _ => 4,
        }
    }

    pub fn abort(&mut self) {
        match self {
            Overlay::Permit(req) => {
                let tx = std::mem::replace(&mut req.reply, tokio::sync::oneshot::channel().0);
                let _ = tx.send(PermitDecision::Deny);
            }
            Overlay::Clarify { req, .. } => {
                let tx = std::mem::replace(&mut req.reply, tokio::sync::oneshot::channel().0);
                let _ = tx.send(ClarifyDecision::Skip);
            }
            _ => {}
        }
    }
}

fn handle_setup(
    wiz: &mut SetupWizard,
    key: KeyEvent,
    prompt: &mut Prompt,
    cfg: &Config,
) -> OverlayAction {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            prompt.clear();
            OverlayAction::Close
        }
        (KeyCode::Enter, m) if m.is_empty() || m == KeyModifiers::NONE => {
            wiz.commit_field(prompt.take());
            match wiz.step {
                SetupStep::Url => {
                    if wiz.url.trim().is_empty() {
                        wiz.load_prompt(prompt);
                        return OverlayAction::None;
                    }
                    wiz.step = SetupStep::Key;
                    wiz.load_prompt(prompt);
                    OverlayAction::None
                }
                SetupStep::Key => {
                    wiz.step = SetupStep::Model;
                    wiz.load_prompt(prompt);
                    OverlayAction::None
                }
                SetupStep::Model => match save_setup(cfg, wiz) {
                    Ok((cfg, note)) => {
                        prompt.clear();
                        OverlayAction::Saved { cfg, note }
                    }
                    Err(note) => {
                        prompt.clear();
                        OverlayAction::Saved {
                            cfg: cfg.clone(),
                            note,
                        }
                    }
                },
            }
        }
        _ => {
            let _ = prompt.handle_key(key, false);
            OverlayAction::None
        }
    }
}

fn handle_permit(req: &mut PermitRequest, key: KeyEvent) -> OverlayAction {
    let dec = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => PermitDecision::Allow,
        KeyCode::Char('a') | KeyCode::Char('A') => PermitDecision::Always,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => PermitDecision::Deny,
        _ => return OverlayAction::None,
    };
    let tx = std::mem::replace(&mut req.reply, tokio::sync::oneshot::channel().0);
    let _ = tx.send(dec);
    OverlayAction::Close
}

fn handle_clarify(
    req: &mut ClarifyRequest,
    other: &mut Option<String>,
    key: KeyEvent,
    prompt: &mut Prompt,
) -> OverlayAction {
    if other.is_some() {
        match key.code {
            KeyCode::Esc => {
                *other = None;
                prompt.clear();
                OverlayAction::None
            }
            KeyCode::Enter => {
                let text = prompt.take();
                let dec = if text.trim().is_empty() {
                    ClarifyDecision::Skip
                } else {
                    ClarifyDecision::Other { text }
                };
                let tx = std::mem::replace(&mut req.reply, tokio::sync::oneshot::channel().0);
                let _ = tx.send(dec);
                OverlayAction::Close
            }
            _ => {
                let _ = prompt.handle_key(key, false);
                OverlayAction::None
            }
        }
    } else {
        let n = req.ask.options.len();
        let dec = match key.code {
            KeyCode::Char(c) if c >= '1' && c <= '4' => {
                let i = (c as u8 - b'1') as usize;
                if i < n {
                    let o = &req.ask.options[i];
                    ClarifyDecision::Pick {
                        id: o.id.clone(),
                        label: o.label.clone(),
                    }
                } else {
                    return OverlayAction::None;
                }
            }
            KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Esc => ClarifyDecision::Skip,
            KeyCode::Char('o') | KeyCode::Char('O') => {
                *other = Some(String::new());
                prompt.clear();
                return OverlayAction::None;
            }
            _ => return OverlayAction::None,
        };
        let tx = std::mem::replace(&mut req.reply, tokio::sync::oneshot::channel().0);
        let _ = tx.send(dec);
        OverlayAction::Close
    }
}

fn handle_plan(key: KeyEvent) -> OverlayAction {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => OverlayAction::Implement,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => OverlayAction::Close,
        KeyCode::Char('q') | KeyCode::Char('Q') => OverlayAction::ExitPlan,
        _ => OverlayAction::None,
    }
}

fn save_setup(_cfg: &Config, wiz: &SetupWizard) -> Result<(Config, String), String> {
    let url = wiz.url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return Err("endpoint empty; not saved".into());
    }
    let key = wiz.key.trim().to_string();
    let model = wiz.model.trim().to_string();
    let path = Config::default_path().map_err(|e| e.to_string())?;
    let next = Config::mutate_disk(&path, |disk| {
        disk.server.base_url = url;
        if !key.is_empty() {
            disk.server.api_key = key;
        }
        if !model.is_empty() {
            disk.server.model = model;
        }
    })
    .map_err(|e| e.to_string())?;
    let mut note = format!(
        "saved {}  model={}",
        next.server.base_url,
        if next.server.model.is_empty() {
            "(first /v1/models id)"
        } else {
            next.server.model.as_str()
        }
    );
    if std::env::var("Q38_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some()
        || std::env::var("Q38_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_some()
        || std::env::var("Q38_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_some()
    {
        note.push_str("  (Q38_* env still wins on next TUI/CLI start; q38 web uses the file)");
    }
    Ok((next, note))
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn setup_three_enters_reaches_save_shape() {
        let cfg = Config::default();
        let mut prompt = Prompt::default();
        let mut overlay = Overlay::setup(&cfg, &mut prompt);
        assert!(matches!(overlay, Overlay::Setup(_)));
        prompt.clear();
        prompt.insert_str("https://llm.example/v1");
        let Overlay::Setup(wiz) = &mut overlay else {
            panic!("setup");
        };
        let act = handle_setup(wiz, press(KeyCode::Enter), &mut prompt, &cfg);
        assert!(matches!(act, OverlayAction::None));
        assert_eq!(wiz.step, SetupStep::Key);
        let act = handle_setup(wiz, press(KeyCode::Enter), &mut prompt, &cfg);
        assert!(matches!(act, OverlayAction::None));
        assert_eq!(wiz.step, SetupStep::Model);
    }
}
