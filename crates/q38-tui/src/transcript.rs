//! Scrollback blocks. Interaction model from Grok Build
//! `xai-grok-pager` thinking/agent/user blocks:
//! streaming `push_chunk`, truncated think (last N lines + "…"), committed
//! assistant replaces the in-progress bubble.

use std::time::Instant;

use q38_loop::session::{DeltaChannel, SessionEvent};
use q38_loop::template::is_hidden_user_text;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::markdown::{self, patch_line};

/// Grok `scrollback.blocks.thinking.truncated_lines` default.
pub const THINK_TRUNCATED_LINES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    Truncated,
    Expanded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Think,
    Assistant,
    Tool,
    System,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub kind: Kind,
    pub text: String,
    pub running: bool,
    pub fold: Fold,
    pub started: Instant,
}

impl Block {
    fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            running: false,
            fold: if kind == Kind::Think {
                Fold::Truncated
            } else {
                Fold::Expanded
            },
            started: Instant::now(),
        }
    }

    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        match self.kind {
            Kind::Think => think_lines(&self.text, width, self.fold, self.running),
            Kind::User => paint_plain(Kind::User, false, prefixed("❯ ", &self.text, width)),
            Kind::Tool => paint_plain(Kind::Tool, false, prefixed("⚙ ", &self.text, width)),
            Kind::System => paint_plain(Kind::System, false, wrap(&self.text, width)),
            Kind::Assistant => {
                let extra = kind_style(Kind::Assistant, self.running);
                markdown::render(&self.text, width)
                    .into_iter()
                    .map(|l| patch_line(l, extra))
                    .collect()
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Live {
    think: String,
    content: String,
    started: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct Transcript {
    blocks: Vec<Block>,
    live: Option<Live>,
    pub follow: bool,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            live: None,
            follow: true,
        }
    }
}

impl Transcript {
    #[allow(dead_code)]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() {
            self.blocks.push(Block::new(Kind::System, text));
        }
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::new(Kind::User, text));
        self.follow = true;
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Delta(d) => {
                if d.reset {
                    self.live = Some(Live {
                        started: Some(Instant::now()),
                        ..Live::default()
                    });
                    return;
                }
                let live = self.live.get_or_insert_with(|| Live {
                    started: Some(Instant::now()),
                    ..Live::default()
                });
                match d.channel {
                    DeltaChannel::Reasoning => live.think.push_str(&d.text),
                    DeltaChannel::Content => live.content.push_str(&d.text),
                }
            }
            SessionEvent::User(u) => {
                if !is_hidden_user_text(&u.text)
                    && self
                        .blocks
                        .last()
                        .is_none_or(|b| b.kind != Kind::User || b.text != u.text)
                {
                    self.blocks.push(Block::new(Kind::User, u.text.clone()));
                }
            }
            SessionEvent::Assistant(a) => {
                let live = self.live.take();
                let think = if !a.reasoning.is_empty() {
                    a.reasoning.clone()
                } else {
                    live.as_ref().map(|l| l.think.clone()).unwrap_or_default()
                };
                if !think.is_empty() {
                    let mut b = Block::new(Kind::Think, think);
                    b.running = false;
                    if let Some(t0) = live.as_ref().and_then(|l| l.started) {
                        b.started = t0;
                    }
                    self.blocks.push(b);
                }
                let content = if a.content.is_empty() {
                    live.map(|l| l.content).unwrap_or_default()
                } else {
                    a.content.clone()
                };
                if !content.is_empty() {
                    self.blocks.push(Block::new(Kind::Assistant, content));
                }
            }
            SessionEvent::Tool(t) => {
                self.commit_live_think();
                let preview = clip_one_line(&t.output, 120);
                self.blocks
                    .push(Block::new(Kind::Tool, format!("{}  {preview}", t.name)));
            }
            SessionEvent::Stop(s) => {
                self.commit_live_think();
                if s.reason == "aborted" {
                    self.push_system("aborted");
                }
                self.live = None;
            }
            SessionEvent::Policy(p) => {
                let label = if p.policy.enabled {
                    format!(
                        "thinking on  effort={}",
                        p.policy.effort.map(|e| e.as_str()).unwrap_or("on")
                    )
                } else {
                    "thinking off".into()
                };
                self.push_system(label);
            }
            SessionEvent::Compact(_) => self.push_system("compact"),
            SessionEvent::Start(_) | SessionEvent::Fork(_) | SessionEvent::Undo(_) => {}
        }
        self.follow = true;
    }

    fn commit_live_think(&mut self) {
        if let Some(live) = &self.live {
            if !live.think.is_empty() {
                let mut think = Block::new(Kind::Think, live.think.clone());
                think.running = false;
                if let Some(t0) = live.started {
                    think.started = t0;
                }
                self.blocks.push(think);
            }
        }
    }

    pub fn cycle_last_think(&mut self) {
        if let Some(b) = self.blocks.iter_mut().rev().find(|b| b.kind == Kind::Think) {
            b.fold = match b.fold {
                Fold::Truncated => Fold::Expanded,
                Fold::Expanded => Fold::Truncated,
            };
        }
    }

    pub fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for b in &self.blocks {
            out.extend(b.lines(width));
        }
        if let Some(live) = &self.live {
            if !live.think.is_empty() || live.started.is_some() {
                let mut think = Block::new(Kind::Think, live.think.clone());
                think.running = true;
                if let Some(t0) = live.started {
                    think.started = t0;
                }
                out.extend(think.lines(width));
            }
            if !live.content.is_empty() {
                let extra = kind_style(Kind::Assistant, true);
                out.extend(
                    markdown::render(&live.content, width)
                        .into_iter()
                        .map(|l| patch_line(l, extra)),
                );
            }
        }
        out
    }
}

fn think_lines(text: &str, width: usize, fold: Fold, running: bool) -> Vec<Line<'static>> {
    let header = Line::from(Span::styled(
        if running { "Thinking…" } else { "Thought" },
        kind_style(Kind::Think, running),
    ));
    let extra = kind_style(Kind::Think, running);
    let body: Vec<Line<'static>> = markdown::render_keep_breaks(text, width)
        .into_iter()
        .map(|l| patch_line(l, extra))
        .collect();
    match fold {
        Fold::Expanded => {
            let mut lines = vec![header];
            lines.extend(body);
            lines
        }
        Fold::Truncated => {
            let n = THINK_TRUNCATED_LINES;
            if body.len() <= n {
                let mut lines = vec![header];
                lines.extend(body);
                lines
            } else {
                let mut lines = vec![header, Line::from(Span::styled("…", extra))];
                lines.extend(body[body.len() - n..].iter().cloned());
                lines
            }
        }
    }
}

fn paint_plain(kind: Kind, running: bool, lines: Vec<String>) -> Vec<Line<'static>> {
    let style = kind_style(kind, running);
    lines
        .into_iter()
        .map(|s| Line::from(Span::styled(s, style)))
        .collect()
}

fn kind_style(kind: Kind, running: bool) -> Style {
    let s = Style::default();
    match kind {
        Kind::Think => s.add_modifier(Modifier::DIM | Modifier::ITALIC),
        Kind::System | Kind::Tool => s.add_modifier(Modifier::DIM),
        Kind::User => s.add_modifier(Modifier::BOLD),
        Kind::Assistant if running => s.add_modifier(Modifier::DIM),
        Kind::Assistant => s,
    }
}

fn prefixed(prefix: &str, text: &str, width: usize) -> Vec<String> {
    let inner = width.saturating_sub(prefix.width());
    let wrapped = wrap(text, inner.max(8));
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("{prefix}{line}")
            } else {
                format!("{}{line}", " ".repeat(prefix.width()))
            }
        })
        .collect()
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in para.split_inclusive(' ') {
            let w = word.width();
            if cur_w > 0 && cur_w + w > width {
                lines.push(cur);
                cur = String::new();
                cur_w = 0;
            }
            if w > width && cur.is_empty() {
                let mut chunk = String::new();
                let mut chunk_w = 0usize;
                for ch in word.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    if chunk_w + cw > width && !chunk.is_empty() {
                        lines.push(std::mem::take(&mut chunk));
                        chunk_w = 0;
                    }
                    chunk.push(ch);
                    chunk_w += cw;
                }
                cur = chunk;
                cur_w = chunk_w;
                continue;
            }
            cur.push_str(word);
            cur_w += w;
        }
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn clip_one_line(s: &str, max: usize) -> String {
    let flat: String = s.lines().next().unwrap_or("").chars().take(max).collect();
    flat
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::line_plain;
    use q38_loop::session::{DeltaChannel, SessionEvent};

    #[test]
    fn deltas_paint_think_then_content_then_assistant_commits() {
        let mut t = Transcript::default();
        t.apply(&SessionEvent::delta_reset());
        t.apply(&SessionEvent::delta_chunk(DeltaChannel::Reasoning, "hmm"));
        t.apply(&SessionEvent::delta_chunk(DeltaChannel::Content, "hi"));
        let live = t.render_lines(80);
        let plain: Vec<String> = live.iter().map(line_plain).collect();
        assert!(plain.iter().any(|s| s.contains("Thinking")));
        assert!(plain.iter().any(|s| s.contains("hi")));
        t.apply(&SessionEvent::assistant("hello", "hmm", None));
        assert_eq!(t.blocks()[0].kind, Kind::Think);
        assert_eq!(t.blocks()[0].text, "hmm");
        assert_eq!(t.blocks()[1].kind, Kind::Assistant);
        assert_eq!(t.blocks()[1].text, "hello");
        assert!(t.live_is_clear());
    }

    #[test]
    fn hidden_user_is_skipped() {
        let mut t = Transcript::default();
        t.apply(&SessionEvent::user(
            "<tool_response>\nsecret\n</tool_response>",
        ));
        assert!(t.blocks().is_empty());
    }

    #[test]
    fn think_truncates_to_last_n() {
        let body = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = think_lines(&body, 80, Fold::Truncated, false);
        assert_eq!(line_plain(&lines[0]), "Thought");
        assert_eq!(line_plain(&lines[1]), "…");
        assert_eq!(line_plain(lines.last().unwrap()), "line9");
        assert_eq!(lines.len(), 2 + THINK_TRUNCATED_LINES);
    }

    #[test]
    fn tool_json_never_becomes_assistant_via_delta() {
        let mut t = Transcript::default();
        t.apply(&SessionEvent::delta_reset());
        t.apply(&SessionEvent::delta_chunk(
            DeltaChannel::Content,
            "I'll read\n",
        ));
        t.apply(&SessionEvent::assistant(
            "I'll read",
            String::new(),
            Some(vec![q38_loop::OpenAiToolCall::function(
                "c1",
                "read",
                "{\"path\":\"a.rs\"}",
            )]),
        ));
        t.apply(&SessionEvent::tool("c1", "read", "fn main"));
        assert!(!t
            .blocks()
            .iter()
            .any(|b| b.kind == Kind::Assistant && b.text.contains("path")));
        assert!(t.blocks().iter().any(|b| b.kind == Kind::Tool));
    }

    #[test]
    fn assistant_markdown_hides_fence_markers() {
        let mut t = Transcript::default();
        t.apply(&SessionEvent::assistant(
            "# Title\n\n```rust\nfn main() {}\n```",
            String::new(),
            None,
        ));
        let out: String = t
            .render_lines(80)
            .iter()
            .map(line_plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("Title"));
        assert!(!out.contains('#'));
        assert!(out.contains("fn main"));
        assert!(!out.contains("```"));
        assert!(out.contains('┌'));
    }
}

impl Transcript {
    #[cfg(test)]
    fn live_is_clear(&self) -> bool {
        self.live.is_none()
    }
}
