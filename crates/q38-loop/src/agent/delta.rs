//! Live token paint. QwenPaw `TextStream` (REASONING vs MESSAGE, `delta: true`)
//! plus Hermes' think-panel / answer-bubble split.
//!
//! Tool-call JSON is never painted as chat text. Deltas are sidecar
//! notifications; the committed `assistant` event still lands once.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::session::{DeltaChannel, SessionEvent};
use crate::sidecar::EventSink;

const TAGS: [&str; 3] = ["<think>", "</think>", "<tool_call>"];
const TOOL_OPEN: &str = "<tool_call>";
const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

#[derive(Default)]
pub(super) struct StdioState {
    think_streamed: AtomicBool,
    text_streamed: AtomicBool,
    think_open: AtomicBool,
}

impl StdioState {
    pub(super) fn text_streamed(&self) -> bool {
        self.text_streamed.load(Ordering::Relaxed)
    }

    pub(super) fn think_streamed(&self) -> bool {
        self.think_streamed.load(Ordering::Relaxed)
    }
}

/// Where token chunks go: TUI `event.append` or CLI stdio.
#[derive(Clone)]
pub struct TokenSink {
    kind: TokenSinkKind,
}

#[derive(Clone)]
enum TokenSinkKind {
    Events(EventSink),
    Stdio(Arc<StdioState>),
}

impl TokenSink {
    pub fn events(emit: EventSink) -> Self {
        Self {
            kind: TokenSinkKind::Events(emit),
        }
    }

    pub(super) fn stdio(state: Arc<StdioState>) -> Self {
        Self {
            kind: TokenSinkKind::Stdio(state),
        }
    }

    pub fn reset(&self) {
        match &self.kind {
            TokenSinkKind::Events(emit) => emit.append(SessionEvent::delta_reset()),
            TokenSinkKind::Stdio(state) => {
                if state.think_open.swap(false, Ordering::Relaxed) {
                    eprintln!();
                }
            }
        }
    }

    pub fn reasoning(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        match &self.kind {
            TokenSinkKind::Events(emit) => {
                emit.append(SessionEvent::delta_chunk(DeltaChannel::Reasoning, text));
            }
            TokenSinkKind::Stdio(state) => {
                if !state.think_open.swap(true, Ordering::Relaxed) {
                    eprint!("[think]\n");
                }
                eprint!("{text}");
                let _ = std::io::stderr().flush();
                state.think_streamed.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn content(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        match &self.kind {
            TokenSinkKind::Events(emit) => {
                emit.append(SessionEvent::delta_chunk(DeltaChannel::Content, text));
            }
            TokenSinkKind::Stdio(state) => {
                if state.think_open.swap(false, Ordering::Relaxed) {
                    eprintln!();
                }
                print!("{text}");
                let _ = std::io::stdout().flush();
                state.text_streamed.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Incremental painter over accumulated SSE `reasoning_content` / `content`.
pub(super) struct StreamPaint {
    sink: TokenSink,
    think_n: usize,
    text_n: usize,
    hide_text: bool,
    started: bool,
}

impl StreamPaint {
    pub(super) fn new(sink: TokenSink) -> Self {
        Self {
            sink,
            think_n: 0,
            text_n: 0,
            hide_text: false,
            started: false,
        }
    }

    /// Raw llama.cpp / OpenAI deltas: split `<think>` live; hold back tag prefixes.
    pub(super) fn push_raw(&mut self, reasoning: &str, content: &str, has_tools: bool) {
        self.start();
        if has_tools {
            self.hide_text = true;
        }
        let (think, text) = visible_raw(reasoning, content);
        self.emit_think(&think);
        if !self.hide_text {
            self.emit_text(&text);
        }
    }

    /// Already-parsed `ModelTurn` (JSON fallback). No tag split.
    pub(super) fn push_clean(&mut self, reasoning: &str, content: &str) {
        self.start();
        self.emit_think(reasoning);
        self.emit_text(content);
    }

    fn start(&mut self) {
        if self.started {
            return;
        }
        self.sink.reset();
        self.started = true;
    }

    fn emit_think(&mut self, full: &str) {
        emit_suffix(&mut self.think_n, full, |d| self.sink.reasoning(d));
    }

    fn emit_text(&mut self, full: &str) {
        emit_suffix(&mut self.text_n, full, |d| self.sink.content(d));
    }
}

fn emit_suffix(emitted: &mut usize, full: &str, send: impl FnOnce(&str)) {
    if full.len() < *emitted {
        *emitted = full.len();
        return;
    }
    if full.len() > *emitted {
        let rest = &full[*emitted..];
        if !rest.is_empty() {
            send(rest);
        }
        *emitted = full.len();
    }
}

/// Hold back a suffix that is a proper prefix of `<think>` / `</think>` / `<tool_call>`.
pub(super) fn hold_back_tag_prefix(s: &str) -> (&str, &str) {
    let max = TAGS.iter().map(|t| t.len()).max().unwrap_or(0);
    let start = s.len().saturating_sub(max);
    for i in start..=s.len() {
        if !s.is_char_boundary(i) {
            continue;
        }
        let suf = &s[i..];
        if suf.is_empty() {
            continue;
        }
        if TAGS
            .iter()
            .any(|t| t.starts_with(suf) && suf.len() < t.len())
        {
            return (&s[..i], suf);
        }
    }
    (s, "")
}

fn split_think_live(content: &str) -> (String, String) {
    let mut reasoning = String::new();
    let mut out = String::new();
    let mut rest = content;
    loop {
        let Some(start) = rest.find(THINK_OPEN) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_open = &rest[start + THINK_OPEN.len()..];
        match after_open.find(THINK_CLOSE) {
            Some(end) => {
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(&after_open[..end]);
                rest = &after_open[end + THINK_CLOSE.len()..];
            }
            None => {
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(after_open);
                break;
            }
        }
    }
    (reasoning, out)
}

fn before_tool(s: &str) -> &str {
    match s.find(TOOL_OPEN) {
        Some(i) => &s[..i],
        None => s,
    }
}

fn visible_raw(reasoning: &str, content: &str) -> (String, String) {
    let r = hold_back_tag_prefix(reasoning).0;
    let c = hold_back_tag_prefix(content).0;
    let think = if !reasoning.is_empty() {
        before_tool(r).to_string()
    } else {
        let (tag_think, _) = split_think_live(c);
        before_tool(&tag_think).to_string()
    };
    let (_, visible) = split_think_live(c);
    let text = before_tool(&visible).to_string();
    (think, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionEvent;

    fn paint_events(chunks: &[(&str, &str, bool)]) -> Vec<SessionEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut paint = StreamPaint::new(TokenSink::events(EventSink { tx }));
        for (r, c, tools) in chunks {
            paint.push_raw(r, c, *tools);
        }
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn texts(events: &[SessionEvent], channel: DeltaChannel) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Delta(d) if !d.reset && d.channel == channel => Some(d.text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn hold_back_partial_think_tag() {
        let (a, b) = hold_back_tag_prefix("hello <thi");
        assert_eq!(a, "hello ");
        assert_eq!(b, "<thi");
        let (a, b) = hold_back_tag_prefix("a < 3");
        assert_eq!(a, "a < 3");
        assert!(b.is_empty());
        let (a, b) = hold_back_tag_prefix("x<");
        assert_eq!(a, "x");
        assert_eq!(b, "<");
    }

    #[test]
    fn reasoning_then_content_skips_tool_json() {
        let events = paint_events(&[
            ("plan", "", false),
            ("plan", "hi", false),
            ("plan", "hi", true),
            ("plan", "hi{\"path\":", true),
        ]);
        assert!(matches!(
            events.first(),
            Some(SessionEvent::Delta(d)) if d.reset
        ));
        assert_eq!(texts(&events, DeltaChannel::Reasoning), "plan");
        assert_eq!(texts(&events, DeltaChannel::Content), "hi");
        assert!(!texts(&events, DeltaChannel::Content).contains("path"));
    }

    #[test]
    fn content_think_tags_go_to_reasoning_channel() {
        let events = paint_events(&[
            ("", "<thi", false),
            ("", "<think>abc", false),
            ("", "<think>abc</think>Hi", false),
        ]);
        assert_eq!(texts(&events, DeltaChannel::Reasoning), "abc");
        assert_eq!(texts(&events, DeltaChannel::Content), "Hi");
    }

    #[test]
    fn xml_tool_call_is_not_answer_text() {
        let events = paint_events(&[
            ("", "I'll read\n<tool", false),
            (
                "",
                "I'll read\n<tool_call>\n<function=read>\n</tool_call>",
                false,
            ),
        ]);
        assert_eq!(texts(&events, DeltaChannel::Content), "I'll read\n");
        assert!(!texts(&events, DeltaChannel::Content).contains("function"));
    }
}
