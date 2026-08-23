//! Compact terminal Mermaid: flowchart + sequence as Unicode boxes.
//!
//! Washed from the Grok pager idea (`xai-grok-markdown` mermaid): inline art
//! in the transcript, not the out-of-process PNG engine / [Open Image] row.

use std::collections::{HashMap, VecDeque};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_NODES: usize = 48;
const MAX_EDGES: usize = 128;
const MAX_ACTORS: usize = 8;
const MAX_MSGS: usize = 64;
const LABEL_WRAP: usize = 20;
const MAX_CANVAS_W: usize = 120;
const MAX_CANVAS_H: usize = 80;

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Down,
    Right,
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Rect,
    Round,
    Diamond,
}

struct Node {
    label: String,
    shape: Shape,
}

struct Edge {
    from: usize,
    to: usize,
    label: Option<String>,
}

struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    index: HashMap<String, usize>,
    dir: Dir,
}

impl Graph {
    fn upsert(&mut self, id: &str, label: Option<&str>, shape: Shape) -> Option<usize> {
        if let Some(&i) = self.index.get(id) {
            if let Some(label) = label {
                self.nodes[i].label = label.to_string();
                self.nodes[i].shape = shape;
            }
            return Some(i);
        }
        if self.nodes.len() >= MAX_NODES {
            return None;
        }
        let i = self.nodes.len();
        self.index.insert(id.to_string(), i);
        self.nodes.push(Node {
            label: label.unwrap_or(id).to_string(),
            shape,
        });
        Some(i)
    }
}

/// Render a ` ```mermaid ` body. Always returns at least a framed source listing.
pub fn render(src: &str, width: usize) -> Vec<Line<'static>> {
    let src = src.trim();
    if src.is_empty() {
        return vec![label_line("mermaid")];
    }
    let art = parse_graph(src)
        .and_then(|g| layout_graph(&g, width))
        .or_else(|| parse_sequence(src).and_then(|s| layout_sequence(&s, width)));
    match art {
        Some(lines) if !lines.is_empty() => {
            let mut out = vec![label_line("mermaid")];
            out.extend(lines);
            out
        }
        _ => fallback(src, width, false),
    }
}

fn label_line(kind: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("◇ {kind}"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    ))
}

fn fallback(src: &str, width: usize, too_wide: bool) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(8);
    let hint = if too_wide {
        "◇ mermaid  (too wide — source)"
    } else {
        "◇ mermaid"
    };
    let mut out = vec![dim_cyan(hint)];
    let bar = "─".repeat(inner.min(40));
    out.push(dim_cyan(&format!("┌{bar}")));
    for line in src.lines() {
        let mut s = line.to_string();
        if s.width() > inner {
            s = clip(&s, inner);
        }
        out.push(dim_cyan(&format!("│{s}")));
    }
    out.push(dim_cyan(&format!("└{bar}")));
    out
}

fn dim_cyan(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_string(),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    ))
}

fn node_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn edge_style() -> Style {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
}

fn styled_rows(rows: &[String], node_rows: &[bool]) -> Vec<Line<'static>> {
    rows.iter()
        .zip(node_rows.iter().chain(std::iter::repeat(&false)))
        .map(|(row, is_node)| {
            let style = if *is_node { node_style() } else { edge_style() };
            Line::from(Span::styled(row.clone(), style))
        })
        .collect()
}

fn parse_graph(src: &str) -> Option<Graph> {
    let mut stmts = Vec::new();
    for line in src.lines() {
        split_statements(strip_comment(line), &mut stmts);
    }
    let header = stmts.first()?;
    let mut tok = header.split_whitespace();
    let kind = tok.next()?.to_ascii_lowercase();
    if kind != "graph" && kind != "flowchart" {
        return None;
    }
    let dir = match tok.next().unwrap_or("TD").to_ascii_uppercase().as_str() {
        "LR" | "RL" => Dir::Right,
        _ => Dir::Down,
    };
    let mut g = Graph {
        nodes: Vec::new(),
        edges: Vec::new(),
        index: HashMap::new(),
        dir,
    };
    for st in &stmts[1..] {
        let first = st
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match first.as_str() {
            "subgraph" | "end" | "classdef" | "class" | "style" | "linkstyle" | "click"
            | "direction" => continue,
            _ => {}
        }
        parse_flow_stmt(st, &mut g)?;
        if g.edges.len() > MAX_EDGES {
            return None;
        }
    }
    if g.nodes.is_empty() {
        None
    } else {
        Some(g)
    }
}

fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if line[i..].starts_with("%%") => return line[..i].trim_end(),
            None => {}
        }
    }
    line
}

fn split_statements(line: &str, out: &mut Vec<String>) {
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote = None::<char>;
    for c in line.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' | '(' | '{' => {
                depth += 1;
                cur.push(c);
            }
            ']' | ')' | '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ';' if depth == 0 => {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        out.push(t);
    }
}

struct ParsedNode {
    id: String,
    label: Option<String>,
    shape: Shape,
}

fn parse_flow_stmt(stmt: &str, g: &mut Graph) -> Option<()> {
    let mut rest = stmt.trim();
    let mut prev: Option<usize> = None;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some((label, after)) = take_edge(rest) {
            let from = prev?;
            rest = after;
            let (node, after) = take_node(rest)?;
            rest = after;
            let to = g.upsert(&node.id, node.label.as_deref(), node.shape)?;
            if g.edges.len() >= MAX_EDGES {
                return None;
            }
            g.edges.push(Edge { from, to, label });
            prev = Some(to);
            continue;
        }
        let (node, after) = take_node(rest)?;
        rest = after;
        prev = Some(g.upsert(&node.id, node.label.as_deref(), node.shape)?);
    }
    Some(())
}

fn take_edge(s: &str) -> Option<(Option<String>, &str)> {
    let s = s.trim_start();
    const OPS: &[&str] = &[
        "-->", "---", "-.->", "==>", "-->>", "x--", "--x", "o--", "--o", "<-->",
    ];
    for op in OPS {
        if let Some(rest) = s.strip_prefix(op) {
            return take_pipe_label(rest);
        }
    }
    if let Some(rest) = s.strip_prefix("--") {
        if rest.starts_with('>') || rest.starts_with('-') || rest.starts_with('.') {
            return None;
        }
        if let Some(idx) = rest.find("-->") {
            let label = rest[..idx].trim();
            let label = if label.is_empty() {
                None
            } else {
                Some(unquote(label))
            };
            return Some((label, rest[idx + 3..].trim_start()));
        }
    }
    None
}

fn take_pipe_label(rest: &str) -> Option<(Option<String>, &str)> {
    let t = rest.trim_start();
    if let Some(inner) = t.strip_prefix('|') {
        let end = inner.find('|')?;
        let label = inner[..end].trim();
        let label = if label.is_empty() {
            None
        } else {
            Some(unquote(label))
        };
        return Some((label, inner[end + 1..].trim_start()));
    }
    Some((None, t))
}

fn take_node(s: &str) -> Option<(ParsedNode, &str)> {
    let s = s.trim_start();
    let (id, rest) = take_ident(s)?;
    let (label, shape, rest) = take_shape(rest);
    Some((
        ParsedNode {
            id: id.to_string(),
            label: label.map(|l| decode_entities(&unquote(&l))),
            shape,
        },
        rest,
    ))
}

fn take_ident(s: &str) -> Option<(&str, &str)> {
    if s.starts_with('"') {
        let end = s[1..].find('"')?;
        return Some((&s[1..1 + end], &s[2 + end..]));
    }
    let n = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .count();
    if n == 0 {
        return None;
    }
    Some((&s[..n], &s[n..]))
}

fn take_shape(s: &str) -> (Option<String>, Shape, &str) {
    let s = s.trim_start();
    if s.starts_with("[[") {
        return close_shape(s, 2, "]]", Shape::Rect);
    }
    if s.starts_with("((") {
        return close_shape(s, 2, "))", Shape::Round);
    }
    if s.starts_with("{{") {
        return close_shape(s, 2, "}}", Shape::Rect);
    }
    if s.starts_with("[(") {
        return close_shape(s, 2, ")]", Shape::Round);
    }
    if s.starts_with("[/") {
        return close_shape(s, 2, "/]", Shape::Rect);
    }
    match s.as_bytes().first() {
        Some(b'[') => close_shape(s, 1, "]", Shape::Rect),
        Some(b'(') => close_shape(s, 1, ")", Shape::Round),
        Some(b'{') => close_shape(s, 1, "}", Shape::Diamond),
        Some(b'>') => close_shape(s, 1, "]", Shape::Rect),
        _ => (None, Shape::Rect, s),
    }
}

fn close_shape<'a>(
    s: &'a str,
    open_len: usize,
    close: &str,
    shape: Shape,
) -> (Option<String>, Shape, &'a str) {
    let body = &s[open_len..];
    if let Some(end) = find_close(body, close) {
        let raw = body[..end].trim();
        (
            Some(unquote(raw)),
            shape,
            body[end + close.len()..].trim_start(),
        )
    } else {
        (None, shape, s)
    }
}

fn find_close(body: &str, close: &str) -> Option<usize> {
    let mut quote = None::<char>;
    let mut i = 0;
    while i + close.len() <= body.len() {
        let c = body[i..].chars().next()?;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += c.len_utf8();
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            i += 1;
            continue;
        }
        if body[i..].starts_with(close) {
            return Some(i);
        }
        i += c.len_utf8();
    }
    None
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("<br/>", "\n")
        .replace("<br>", "\n")
        .replace("<br />", "\n")
}

fn assign_ranks(g: &Graph) -> Vec<usize> {
    let n = g.nodes.len();
    let mut indeg = vec![0usize; n];
    let mut adj = vec![Vec::new(); n];
    for e in &g.edges {
        if e.from < n && e.to < n && e.from != e.to {
            adj[e.from].push(e.to);
            indeg[e.to] += 1;
        }
    }
    let mut rank = vec![0usize; n];
    let mut q: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0usize;
    while let Some(u) = q.pop_front() {
        seen += 1;
        for &v in &adj[u] {
            rank[v] = rank[v].max(rank[u] + 1);
            indeg[v] -= 1;
            if indeg[v] == 0 {
                q.push_back(v);
            }
        }
    }
    if seen < n {
        let extra = rank.iter().copied().max().unwrap_or(0) + 1;
        for (i, r) in rank.iter_mut().enumerate() {
            if indeg[i] > 0 {
                *r = extra;
            }
        }
    }
    rank
}

fn wrap_label(label: &str, max_w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in label.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in para.split_inclusive(' ') {
            let w = word.width();
            if cur_w > 0 && cur_w + w > max_w {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if w > max_w && cur.is_empty() {
                let mut chunk = String::new();
                let mut cw = 0usize;
                for ch in word.chars() {
                    let dw = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if cw + dw > max_w && !chunk.is_empty() {
                        lines.push(std::mem::take(&mut chunk));
                        cw = 0;
                    }
                    chunk.push(ch);
                    cw += dw;
                }
                cur = chunk;
                cur_w = cw;
            } else {
                cur.push_str(word);
                cur_w += w;
            }
        }
        if !cur.is_empty() || para.is_empty() {
            lines.push(cur);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines.truncate(3);
    lines
}

struct BoxArt {
    lines: Vec<String>,
    w: usize,
    h: usize,
}

fn node_box(node: &Node, max_inner: usize) -> BoxArt {
    let inner = wrap_label(&node.label, max_inner.max(1));
    let content_w = inner.iter().map(|l| l.width()).max().unwrap_or(1).max(1);
    let w = content_w + 2;
    let (tl, tr, bl, br, hbar, vbar) = match node.shape {
        Shape::Round => ('╭', '╮', '╰', '╯', '─', '│'),
        Shape::Diamond => ('╱', '╲', '╲', '╱', '─', '│'),
        Shape::Rect => ('┌', '┐', '└', '┘', '─', '│'),
    };
    let mut lines = Vec::new();
    lines.push(format!("{tl}{}{tr}", hbar.to_string().repeat(content_w)));
    for row in &inner {
        let pad = content_w.saturating_sub(row.width());
        lines.push(format!("{vbar}{row}{}{vbar}", " ".repeat(pad)));
    }
    lines.push(format!("{bl}{}{br}", hbar.to_string().repeat(content_w)));
    let h = lines.len();
    BoxArt { lines, w, h }
}

fn layout_graph(g: &Graph, width: usize) -> Option<Vec<Line<'static>>> {
    let n = g.nodes.len();
    if n == 0 {
        return None;
    }
    let ranks = assign_ranks(g);
    let max_rank = ranks.iter().copied().max()?;
    let mut by_rank: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (i, r) in ranks.iter().enumerate() {
        by_rank[*r].push(i);
    }
    let inner_cap = LABEL_WRAP.min(width.saturating_sub(6).max(4));
    let boxes: Vec<BoxArt> = g
        .nodes
        .iter()
        .map(|node| node_box(node, inner_cap))
        .collect();

    let gap = 3usize;
    let mut col_x = vec![0usize; n];
    let mut col_y = vec![0usize; n];
    let mut canvas_w = 0usize;
    let mut canvas_h = 0usize;

    match g.dir {
        Dir::Down => {
            let mut y = 0usize;
            for rank in &by_rank {
                if rank.is_empty() {
                    continue;
                }
                let row_h = rank.iter().map(|&i| boxes[i].h).max().unwrap_or(1);
                let mut x = 0usize;
                for &i in rank {
                    col_x[i] = x;
                    col_y[i] = y;
                    x += boxes[i].w + gap;
                }
                canvas_w = canvas_w.max(x.saturating_sub(gap));
                y += row_h + 2;
            }
            canvas_h = y.saturating_sub(2);
        }
        Dir::Right => {
            let mut x = 0usize;
            for rank in &by_rank {
                if rank.is_empty() {
                    continue;
                }
                let col_w = rank.iter().map(|&i| boxes[i].w).max().unwrap_or(1);
                let mut y = 0usize;
                for &i in rank {
                    col_x[i] = x;
                    col_y[i] = y;
                    y += boxes[i].h + 1;
                }
                canvas_w = canvas_w.max(x + col_w);
                canvas_h = canvas_h.max(y.saturating_sub(1));
                x += col_w + 4;
            }
        }
    }

    if canvas_w == 0 || canvas_h == 0 {
        return None;
    }
    if canvas_w > width.min(MAX_CANVAS_W) || canvas_h > MAX_CANVAS_H {
        return None;
    }

    let mut grid = vec![vec![' '; canvas_w]; canvas_h];
    let mut is_node = vec![false; canvas_h];
    for i in 0..n {
        blit(&mut grid, col_x[i], col_y[i], &boxes[i]);
        for dy in 0..boxes[i].h {
            let y = col_y[i] + dy;
            if y < is_node.len() {
                is_node[y] = true;
            }
        }
    }
    for e in &g.edges {
        connect(
            &mut grid,
            col_x[e.from],
            col_y[e.from],
            boxes[e.from].w,
            boxes[e.from].h,
            col_x[e.to],
            col_y[e.to],
            boxes[e.to].w,
            g.dir,
        );
        if let Some(label) = &e.label {
            let lx = (col_x[e.from] + col_x[e.to]) / 2;
            let ly = match g.dir {
                Dir::Down => (col_y[e.from] + boxes[e.from].h + col_y[e.to]) / 2,
                Dir::Right => (col_y[e.from] + col_y[e.to]) / 2,
            };
            stamp_label(&mut grid, lx, ly, label);
        }
    }

    let rows: Vec<String> = grid
        .iter()
        .map(|row| {
            let s: String = row.iter().collect();
            s.trim_end().to_string()
        })
        .collect();
    Some(styled_rows(&rows, &is_node))
}

fn blit(grid: &mut [Vec<char>], x: usize, y: usize, art: &BoxArt) {
    for (dy, line) in art.lines.iter().enumerate() {
        let gy = y + dy;
        if gy >= grid.len() {
            break;
        }
        for (dx, ch) in line.chars().enumerate() {
            let gx = x + dx;
            if gx < grid[gy].len() {
                grid[gy][gx] = ch;
            }
        }
    }
}

fn put(grid: &mut [Vec<char>], x: usize, y: usize, ch: char) {
    if y < grid.len() && x < grid[y].len() {
        let cur = grid[y][x];
        if cur == ' ' || "│─┼┤├┬┴┐┌┘└►▼".contains(cur) {
            grid[y][x] = merge_box(cur, ch);
        }
    }
}

fn merge_box(old: char, new: char) -> char {
    match (old, new) {
        (' ', c) => c,
        ('│', '─') | ('─', '│') => '┼',
        ('│', '►') => '►',
        ('─', '▼') => '▼',
        (_, c) if "►▼".contains(c) => c,
        (c, _) => c,
    }
}

#[allow(clippy::too_many_arguments)]
fn connect(
    grid: &mut [Vec<char>],
    fx: usize,
    fy: usize,
    fw: usize,
    fh: usize,
    tx: usize,
    ty: usize,
    tw: usize,
    dir: Dir,
) {
    match dir {
        Dir::Down => {
            let x0 = fx + fw / 2;
            let x1 = tx + tw / 2;
            let y0 = fy + fh;
            let y1 = ty.saturating_sub(1);
            if y0 >= grid.len() {
                return;
            }
            let mid_y = y0.min(y1);
            for y in y0..=mid_y.min(grid.len().saturating_sub(1)) {
                put(grid, x0, y, '│');
            }
            if x0 != x1 {
                let (lo, hi) = if x0 < x1 { (x0, x1) } else { (x1, x0) };
                for x in lo..=hi.min(grid[0].len().saturating_sub(1)) {
                    put(grid, x, mid_y, '─');
                }
            }
            for y in mid_y..=y1.min(grid.len().saturating_sub(1)) {
                put(grid, x1, y, '│');
            }
            if ty > 0 {
                put(grid, x1, ty.saturating_sub(1), '▼');
            }
        }
        Dir::Right => {
            let y0 = fy + fh / 2;
            let y1 = ty;
            let x0 = fx + fw;
            let x1 = tx.saturating_sub(1);
            let mid_x = x0.min(x1);
            if y0 < grid.len() {
                for x in x0..=mid_x.min(grid[y0].len().saturating_sub(1)) {
                    put(grid, x, y0, '─');
                }
            }
            if y0 != y1 {
                let (lo, hi) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
                for y in lo..=hi.min(grid.len().saturating_sub(1)) {
                    put(grid, mid_x, y, '│');
                }
            }
            if y1 < grid.len() {
                for x in mid_x..=x1.min(grid[y1].len().saturating_sub(1)) {
                    put(grid, x, y1, '─');
                }
                put(grid, x1.min(grid[y1].len().saturating_sub(1)), y1, '►');
            }
        }
    }
}

fn stamp_label(grid: &mut [Vec<char>], x: usize, y: usize, label: &str) {
    if y >= grid.len() {
        return;
    }
    let t = clip(label, 12);
    let start = x.saturating_sub(t.width() / 2);
    for (i, ch) in t.chars().enumerate() {
        let gx = start + i;
        if gx < grid[y].len() && grid[y][gx] == ' ' {
            grid[y][gx] = ch;
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

struct Sequence {
    actors: Vec<String>,
    msgs: Vec<SeqMsg>,
}

struct SeqMsg {
    from: usize,
    to: usize,
    text: String,
    dashed: bool,
}

fn parse_sequence(src: &str) -> Option<Sequence> {
    let mut lines = src.lines();
    let header = lines.next()?.trim().to_ascii_lowercase().replace(' ', "");
    if !header.starts_with("sequencediagram") {
        return None;
    }
    let mut seq = Sequence {
        actors: Vec::new(),
        msgs: Vec::new(),
    };
    let mut index: HashMap<String, usize> = HashMap::new();
    for raw in lines {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("participant ") || lower.starts_with("actor ") {
            let rest = line.splitn(2, ' ').nth(1)?.trim();
            let (id, label) = if let Some((id, alias)) = rest.split_once(" as ") {
                (id.trim(), alias.trim())
            } else {
                (rest, rest)
            };
            if seq.actors.len() >= MAX_ACTORS {
                return None;
            }
            if !index.contains_key(id) {
                index.insert(id.to_string(), seq.actors.len());
                seq.actors.push(unquote(label));
            }
            continue;
        }
        if lower.starts_with("note ")
            || lower.starts_with("activate ")
            || lower.starts_with("deactivate ")
        {
            continue;
        }
        if let Some(msg) = parse_seq_msg(line, &mut seq, &mut index) {
            seq.msgs.push(msg);
            if seq.msgs.len() > MAX_MSGS {
                return None;
            }
        }
    }
    if seq.actors.is_empty() || seq.msgs.is_empty() {
        None
    } else {
        Some(seq)
    }
}

fn actor_id(seq: &mut Sequence, index: &mut HashMap<String, usize>, id: &str) -> Option<usize> {
    if let Some(&i) = index.get(id) {
        return Some(i);
    }
    if seq.actors.len() >= MAX_ACTORS {
        return None;
    }
    let i = seq.actors.len();
    index.insert(id.to_string(), i);
    seq.actors.push(id.to_string());
    Some(i)
}

fn parse_seq_msg(
    line: &str,
    seq: &mut Sequence,
    index: &mut HashMap<String, usize>,
) -> Option<SeqMsg> {
    const ARROWS: &[(&str, bool)] = &[
        ("-->>", true),
        ("->>", false),
        ("-->", true),
        ("->", false),
        ("-->>+", true),
        ("->>-", false),
    ];
    for (arrow, dashed) in ARROWS {
        if let Some(idx) = line.find(arrow) {
            let from = line[..idx].trim();
            let rest = &line[idx + arrow.len()..];
            let (to, text) = rest.split_once(':')?;
            let from_i = actor_id(seq, index, from)?;
            let to_i = actor_id(seq, index, to.trim())?;
            return Some(SeqMsg {
                from: from_i,
                to: to_i,
                text: text.trim().to_string(),
                dashed: *dashed,
            });
        }
    }
    None
}

fn layout_sequence(seq: &Sequence, width: usize) -> Option<Vec<Line<'static>>> {
    let n = seq.actors.len();
    if n == 0 {
        return None;
    }
    let col_w = seq
        .actors
        .iter()
        .map(|a| a.width().max(3) + 4)
        .max()
        .unwrap_or(8)
        .min(18);
    let total = n * col_w;
    if total > width.min(MAX_CANVAS_W) {
        return None;
    }
    let mut rows: Vec<String> = Vec::new();
    let mut names = String::new();
    for (i, a) in seq.actors.iter().enumerate() {
        let pad = col_w.saturating_sub(a.width());
        let left = pad / 2;
        names.push_str(&format!(
            "{}{a}{}",
            " ".repeat(left),
            " ".repeat(pad - left)
        ));
        if i + 1 < n {
            names.push(' ');
        }
    }
    rows.push(names.trim_end().to_string());
    let lifeline = |buf: &mut String| {
        buf.clear();
        for i in 0..n {
            let mid = col_w / 2;
            buf.push_str(&" ".repeat(mid));
            buf.push('│');
            buf.push_str(&" ".repeat(col_w - mid - 1));
            if i + 1 < n {
                buf.push(' ');
            }
        }
    };
    let mut life = String::new();
    lifeline(&mut life);
    rows.push(life.trim_end().to_string());
    for msg in &seq.msgs {
        let mut arrow = String::new();
        lifeline(&mut arrow);
        let a0 = msg.from.min(msg.to);
        let a1 = msg.from.max(msg.to);
        let x0 = a0 * (col_w + 1) + col_w / 2;
        let x1 = a1 * (col_w + 1) + col_w / 2;
        let mut chars: Vec<char> = arrow.chars().collect();
        while chars.len() <= x1 {
            chars.push(' ');
        }
        let fill = if msg.dashed { '┄' } else { '─' };
        for x in (x0 + 1)..x1 {
            if x < chars.len() {
                chars[x] = fill;
            }
        }
        let head = if msg.from < msg.to { '►' } else { '◄' };
        let hx = if msg.from < msg.to { x1 } else { x0 };
        if hx < chars.len() {
            chars[hx] = head;
        }
        let line: String = chars.iter().collect();
        rows.push(line.trim_end().to_string());
        if !msg.text.is_empty() {
            let mut lab = String::new();
            lifeline(&mut lab);
            let mut chars: Vec<char> = lab.chars().collect();
            let text = clip(&msg.text, (x1 - x0).saturating_sub(1).max(3));
            let start = x0 + 1;
            for (i, ch) in text.chars().enumerate() {
                let x = start + i;
                if x < chars.len() {
                    chars[x] = ch;
                }
            }
            let line: String = chars.iter().collect();
            rows.push(line.trim_end().to_string());
        }
        let mut life = String::new();
        lifeline(&mut life);
        rows.push(life.trim_end().to_string());
    }
    let node_rows = vec![true; rows.len()];
    Some(styled_rows(&rows, &node_rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn flowchart_boxes_user_to_q38() {
        let src = "graph TD\nA[User] --> B[q38]";
        let out = plain(&render(src, 80));
        assert!(out.contains("◇ mermaid"));
        assert!(out.contains("User"));
        assert!(out.contains("q38"));
        assert!(out.contains('┌') || out.contains('╭'));
        assert!(!out.contains("```"));
    }

    #[test]
    fn sequence_draws_arrow() {
        let src = "sequenceDiagram\nAlice->>Bob: hi";
        let out = plain(&render(src, 80));
        assert!(out.contains("Alice"));
        assert!(out.contains("Bob"));
        assert!(out.contains('►') || out.contains('─'));
        assert!(out.contains("hi"));
    }

    #[test]
    fn unknown_falls_back_to_source() {
        let src = "pie title Pets\n\"dogs\" : 386";
        let out = plain(&render(src, 80));
        assert!(out.contains("pie title Pets"));
        assert!(out.contains('┌'));
    }
}
