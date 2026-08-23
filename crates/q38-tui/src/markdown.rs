//! Streaming-friendly markdown → ratatui lines.
//!
//! Pretty-renders assistant/think bubbles (headings, lists, fences, mermaid)
//! without Grok's `xai-grok-markdown` crate. Re-parse on each paint: 27B
//! token rate is well under wrap cost.

use std::sync::LazyLock;

use pulldown_cmark::{CodeBlockKind, CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::mermaid;

static SYNTAX: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

pub fn render(md: &str, width: usize) -> Vec<Line<'static>> {
    render_inner(md, width, true)
}

/// CommonMark soft breaks stay as line breaks (think bubbles / line-faithful).
pub fn render_keep_breaks(md: &str, width: usize) -> Vec<Line<'static>> {
    render_inner(md, width, false)
}

fn render_inner(md: &str, width: usize, collapse_soft_breaks: bool) -> Vec<Line<'static>> {
    if md.is_empty() {
        return Vec::new();
    }
    let width = width.max(8);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(md, opts);
    let mut w = Writer::new(width, collapse_soft_breaks);
    w.run(parser);
    w.finish()
}

struct Writer {
    width: usize,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    quote: usize,
    code: Option<CodeBuf>,
    table: Option<TableBuf>,
    pending_link: Option<String>,
    collapse_soft_breaks: bool,
}

struct ListState {
    ordered: bool,
    next: u64,
}

struct CodeBuf {
    lang: String,
    body: String,
}

struct TableBuf {
    rows: Vec<Vec<String>>,
    cell: String,
}

impl Writer {
    fn new(width: usize, collapse_soft_breaks: bool) -> Self {
        Self {
            width,
            lines: Vec::new(),
            spans: Vec::new(),
            styles: vec![Style::default()],
            lists: Vec::new(),
            quote: 0,
            code: None,
            table: None,
            pending_link: None,
            collapse_soft_breaks,
        }
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, extra: Style) {
        self.styles.push(self.style().patch(extra));
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn run<'a, I: Iterator<Item = Event<'a>>>(&mut self, parser: I) {
        for ev in parser {
            self.event(ev);
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        if self.code.is_some() {
            match ev {
                Event::Text(t) | Event::Code(t) | Event::Html(t) | Event::InlineHtml(t) => {
                    if let Some(code) = &mut self.code {
                        code.body.push_str(&t);
                    }
                }
                Event::End(TagEnd::CodeBlock) => self.end_code(),
                _ => {}
            }
            return;
        }
        if self.table.is_some() {
            self.table_event(ev);
            return;
        }
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                let style = Style::default().fg(Color::Cyan);
                self.push_span(t.to_string(), style);
            }
            Event::SoftBreak => {
                if self.collapse_soft_breaks {
                    self.text(" ");
                } else {
                    self.break_line();
                }
            }
            Event::HardBreak => self.break_line(),
            Event::Rule => {
                self.flush_para();
                let w = self.width.min(48);
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(w),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                self.push_span(mark.to_string(), self.style());
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                self.push_span(t.to_string(), Style::default().add_modifier(Modifier::DIM));
            }
            Event::FootnoteReference(t) => {
                self.push_span(
                    format!("[{t}]"),
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                self.push_span(t.to_string(), Style::default().fg(Color::Magenta));
            }
        }
    }

    fn table_event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                if let Some(table) = self.table.as_mut() {
                    table.rows.push(Vec::new());
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.clear();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    let cell = std::mem::take(&mut table.cell);
                    if let Some(row) = table.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            Event::End(TagEnd::Table) => self.end_table(),
            Event::Text(t) | Event::Code(t) => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.push(' ');
                }
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_para();
                let style = heading_style(level);
                self.push_style(style);
            }
            Tag::BlockQuote(_) => {
                self.flush_para();
                self.quote += 1;
            }
            Tag::List(start) => {
                self.flush_para();
                self.lists.push(ListState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.flush_para();
                let prefix = match self.lists.last_mut() {
                    Some(l) if l.ordered => {
                        let n = l.next;
                        l.next += 1;
                        format!("{n}. ")
                    }
                    _ => "• ".to_string(),
                };
                self.push_span(
                    prefix,
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                );
            }
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT))
            }
            Tag::Link { dest_url, .. } => {
                self.pending_link = Some(dest_url.to_string());
                self.push_style(Style::default().add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                let alt = if title.is_empty() {
                    dest_url.to_string()
                } else {
                    title.to_string()
                };
                self.push_span(
                    format!("[{alt}]"),
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            Tag::CodeBlock(kind) => {
                self.flush_para();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => fence_lang(&lang),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBuf {
                    lang,
                    body: String::new(),
                });
            }
            Tag::Table(_) => {
                self.flush_para();
                self.table = Some(TableBuf {
                    rows: Vec::new(),
                    cell: String::new(),
                });
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_)
            | Tag::Superscript
            | Tag::Subscript => {}
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_para(),
            TagEnd::Heading(_) => {
                self.flush_para();
                self.pop_style();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_para();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.flush_para();
                self.lists.pop();
                self.blank();
            }
            TagEnd::Item => self.flush_para(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(url) = self.pending_link.take() {
                    if !url.is_empty() {
                        self.push_span(
                            format!(" ({url})"),
                            Style::default().add_modifier(Modifier::DIM),
                        );
                    }
                }
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::Table => self.end_table(),
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        self.push_span(t.to_string(), self.style());
    }

    fn push_span(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        self.spans.push(Span::styled(text, style));
    }

    fn break_line(&mut self) {
        self.flush_raw(false);
    }

    fn blank(&mut self) {
        if self.lines.last().is_none_or(|l| !line_is_empty(l)) {
            self.lines.push(Line::from(""));
        }
    }

    fn flush_para(&mut self) {
        self.flush_raw(true);
    }

    fn flush_raw(&mut self, word_wrap: bool) {
        if self.spans.is_empty() {
            return;
        }
        let prefix = quote_prefix(self.quote);
        let indent = if self.lists.is_empty() {
            0
        } else {
            self.lists.len().saturating_sub(1) * 2
        };
        let lead = format!("{}{}", prefix, " ".repeat(indent));
        let avail = self.width.saturating_sub(lead.width()).max(8);
        let wrapped = if word_wrap {
            wrap_spans_words(std::mem::take(&mut self.spans), avail)
        } else {
            wrap_spans_chars(std::mem::take(&mut self.spans), avail)
        };
        for (i, mut line) in wrapped.into_iter().enumerate() {
            if !lead.is_empty() {
                let mut spans = vec![Span::styled(
                    if i == 0 {
                        lead.clone()
                    } else {
                        " ".repeat(lead.width())
                    },
                    Style::default().add_modifier(Modifier::DIM),
                )];
                spans.append(&mut line.spans);
                line = Line::from(spans);
            }
            self.lines.push(line);
        }
    }

    fn end_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        let lang = code.lang.trim().to_ascii_lowercase();
        if lang == "mermaid" {
            self.lines.extend(mermaid::render(&code.body, self.width));
            self.blank();
            return;
        }
        self.lines
            .extend(fence_block(&lang, &code.body, self.width));
        self.blank();
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        for row in table.rows {
            let joined = row.join(" │ ");
            for w in wrap_plain(&joined, self.width) {
                self.lines.push(Line::from(Span::styled(
                    w,
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
        }
        self.blank();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if let Some(code) = self.code.take() {
            let lang = code.lang.trim().to_ascii_lowercase();
            if lang == "mermaid" {
                self.lines.extend(mermaid::render(&code.body, self.width));
            } else {
                self.lines
                    .extend(fence_block(&lang, &code.body, self.width));
            }
        }
        self.flush_para();
        while self.lines.last().is_some_and(line_is_empty) {
            self.lines.pop();
        }
        self.lines
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    let s = Style::default().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 => s.fg(Color::Yellow),
        HeadingLevel::H2 => s.fg(Color::LightYellow),
        _ => s,
    }
}

fn fence_lang(info: &CowStr<'_>) -> String {
    info.split_whitespace().next().unwrap_or("").to_string()
}

fn quote_prefix(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        "│ ".repeat(depth)
    }
}

fn fence_block(lang: &str, body: &str, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(8);
    let mut out = Vec::new();
    let tag = if lang.is_empty() { "code" } else { lang };
    out.push(Line::from(Span::styled(
        format!("┌ {tag}"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    )));
    let painted = highlight_code(lang, body);
    if painted.is_empty() {
        out.push(code_gutter(""));
    } else {
        for line in painted {
            let wrapped = wrap_spans_chars(line.spans, inner);
            for w in wrapped {
                let mut spans = vec![Span::styled(
                    "│ ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                )];
                spans.extend(w.spans);
                out.push(Line::from(spans));
            }
        }
    }
    out.push(Line::from(Span::styled(
        "└",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    )));
    out
}

fn code_gutter(s: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "│ ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
        ),
        Span::styled(s.to_string(), Style::default().fg(Color::Cyan)),
    ])
}

fn highlight_code(lang: &str, body: &str) -> Vec<Line<'static>> {
    let body = body.strip_suffix('\n').unwrap_or(body);
    if body.is_empty() {
        return Vec::new();
    }
    let syntax = SYNTAX
        .find_syntax_by_token(lang)
        .or_else(|| SYNTAX.find_syntax_by_extension(lang))
        .unwrap_or_else(|| SYNTAX.find_syntax_plain_text());
    let theme = THEMES
        .themes
        .get("base16-ocean.dark")
        .or_else(|| THEMES.themes.values().next());
    let Some(theme) = theme else {
        return body
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(Color::Cyan),
                ))
            })
            .collect();
    };
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(body) {
        let ranges = h.highlight_line(line, &SYNTAX).unwrap_or_default();
        let mut spans = Vec::new();
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(text.to_string(), syn_to_ratatui(style)));
        }
        if spans.is_empty() {
            out.push(Line::from(""));
        } else {
            out.push(Line::from(spans));
        }
    }
    out
}

fn syn_to_ratatui(s: SynStyle) -> Style {
    let fg = s.foreground;
    let mut style = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if s.font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style
        .contains(syntect::highlighting::FontStyle::ITALIC)
    {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style
        .contains(syntect::highlighting::FontStyle::UNDERLINE)
    {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn line_is_empty(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

#[cfg(test)]
pub fn line_plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
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
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
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

fn wrap_spans_words(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut words: Vec<(String, Style)> = Vec::new();
    for span in spans {
        let style = span.style;
        for word in span.content.split_inclusive(' ') {
            if !word.is_empty() {
                words.push((word.to_string(), style));
            }
        }
    }
    wrap_units(words, width, true)
}

fn wrap_spans_chars(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut units = Vec::new();
    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            units.push((ch.to_string(), style));
        }
    }
    wrap_units(units, width, false)
}

fn wrap_units(units: Vec<(String, Style)>, width: usize, keep_words: bool) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from(
            units
                .into_iter()
                .map(|(t, s)| Span::styled(t, s))
                .collect::<Vec<_>>(),
        )];
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for (text, style) in units {
        if text == "\n" {
            lines.push(take_line(&mut cur));
            cur_w = 0;
            continue;
        }
        let w = text.width();
        if keep_words && cur_w > 0 && cur_w + w > width {
            lines.push(take_line(&mut cur));
            cur_w = 0;
        }
        if w > width && (cur.is_empty() || !keep_words) {
            for ch in text.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + cw > width && !cur.is_empty() {
                    lines.push(take_line(&mut cur));
                    cur_w = 0;
                }
                push_span_merge(&mut cur, ch.to_string(), style);
                cur_w += cw;
            }
            continue;
        }
        if !keep_words && cur_w > 0 && cur_w + w > width {
            lines.push(take_line(&mut cur));
            cur_w = 0;
        }
        push_span_merge(&mut cur, text, style);
        cur_w += w;
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(take_line(&mut cur));
    }
    lines
}

fn take_line(cur: &mut Vec<Span<'static>>) -> Line<'static> {
    Line::from(std::mem::take(cur))
}

fn push_span_merge(cur: &mut Vec<Span<'static>>, text: String, style: Style) {
    if let Some(last) = cur.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(&text);
            return;
        }
    }
    cur.push(Span::styled(text, style));
}

pub fn patch_line(line: Line<'static>, extra: Style) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(extra)))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(md: &str) -> String {
        render(md, 80)
            .iter()
            .map(line_plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn heading_drops_hash() {
        let out = plain("# Hello");
        assert!(out.contains("Hello"));
        assert!(!out.contains('#'));
    }

    #[test]
    fn rust_fence_hides_backticks() {
        let md = "```rust\nfn main() {}\n```";
        let out = plain(md);
        assert!(out.contains("fn main"));
        assert!(out.contains("rust"));
        assert!(!out.contains("```"));
        assert!(out.contains('┌'));
    }

    #[test]
    fn mermaid_fence_becomes_art() {
        let md = "see:\n\n```mermaid\ngraph TD\nA[User] --> B[q38]\n```";
        let out = plain(md);
        assert!(out.contains("◇ mermaid"));
        assert!(out.contains("User"));
        assert!(out.contains("q38"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn incomplete_fence_still_renders() {
        let out = plain("```rust\nfn x(");
        assert!(out.contains("fn x"));
        assert!(!out.contains("```"));
    }
}
