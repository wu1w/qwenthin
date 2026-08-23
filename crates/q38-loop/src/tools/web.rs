//! One `web` tool: search (`query`) or fetch a page (`url`).
//!
//! Out of the box it needs no key: keyless engine scrape (Bing HTML, then
//! DuckDuckGo lite) and a readability-style extractor for pages. When a Tavily
//! key exists (config → env → mcp.toml sniff) the same tool switches to Tavily
//! REST transparently — no npx spawn, no MCP handshake — and falls back to the
//! builtin path if the API errors. Output format is identical across
//! providers, so the model never learns or cares which backend answered.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::WebConfig;
use crate::mcp::McpRegistry;
use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools::{arg_str, folded_response, BlobStore, ToolLimits};

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8";
/// Per-engine budget inside one search call, leaving room for the next engine.
const ENGINE_TIMEOUT_S: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Builtin,
    Tavily,
}

#[derive(Clone)]
pub struct WebRunner {
    cfg: WebConfig,
    provider: Provider,
    tavily_key: Option<String>,
    client: reqwest::Client,
}

impl WebRunner {
    /// Resolve the provider once at session start. `auto` = Tavily when a key
    /// is found anywhere the user could plausibly have put one, else builtin.
    pub fn new(cfg: WebConfig, mcp: &McpRegistry) -> Self {
        let key = resolve_tavily_key(&cfg, mcp);
        let provider = match cfg.provider.as_str() {
            "builtin" => Provider::Builtin,
            "tavily" => Provider::Tavily,
            _ => {
                if key.is_some() {
                    Provider::Tavily
                } else {
                    Provider::Builtin
                }
            }
        };
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            cfg,
            provider,
            tavily_key: key,
            client,
        }
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub async fn run(
        &self,
        call: &ToolCall,
        limits: ToolLimits,
        blobs: Option<&BlobStore>,
    ) -> ToolResponse {
        let query = arg_str(&call.arguments, "query").unwrap_or_default();
        let url = arg_str(&call.arguments, "url").unwrap_or_default();
        if !url.trim().is_empty() {
            return self.fetch(call, url.trim(), limits, blobs).await;
        }
        if !query.trim().is_empty() {
            return self.search(call, query.trim(), limits, blobs).await;
        }
        ToolResponse::text(
            &call.id,
            "Error: 需要 query（搜索）或 url（抓取网页正文）。",
            ToolState::Error,
        )
    }

    async fn search(
        &self,
        call: &ToolCall,
        query: &str,
        limits: ToolLimits,
        blobs: Option<&BlobStore>,
    ) -> ToolResponse {
        let n = self.cfg.max_results.clamp(1, 10);
        if self.provider == Provider::Tavily {
            match self.tavily_search(query, n).await {
                Ok(hits) if !hits.is_empty() => {
                    let text = format_hits("tavily", query, &hits);
                    return folded_response(&call.id, text, ToolState::Success, limits, blobs);
                }
                Ok(_) => {}
                Err(_) if self.tavily_key.is_some() => {}
                Err(e) => {
                    return ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error)
                }
            }
            // Tavily empty/failed: builtin engines still answer the call.
        }
        let mut errors = Vec::new();
        for engine in &self.cfg.engines {
            let out = match engine.as_str() {
                "bing" => self.bing_search(query, n).await,
                "duckduckgo" | "ddg" => self.ddg_search(query, n).await,
                other => Err(format!("unknown engine {other}")),
            };
            match out {
                Ok(hits) if !hits.is_empty() => {
                    let text = format_hits(engine, query, &hits);
                    return folded_response(&call.id, text, ToolState::Success, limits, blobs);
                }
                Ok(_) => errors.push(format!("{engine}: 0 results")),
                Err(e) => errors.push(format!("{engine}: {e}")),
            }
        }
        ToolResponse::text(
            &call.id,
            format!(
                "Error: 搜索无结果（{}）。换个关键词再试。",
                errors.join("; ")
            ),
            ToolState::Error,
        )
    }

    async fn fetch(
        &self,
        call: &ToolCall,
        url: &str,
        limits: ToolLimits,
        blobs: Option<&BlobStore>,
    ) -> ToolResponse {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return ToolResponse::text(
                &call.id,
                "Error: url 需以 http:// 或 https:// 开头。",
                ToolState::Error,
            );
        }
        if self.provider == Provider::Tavily {
            if let Ok(page) = self.tavily_extract(url).await {
                if !page.trim().is_empty() {
                    let text = format!("[web] {url}\n\n{page}");
                    return folded_response(&call.id, text, ToolState::Success, limits, blobs);
                }
            }
            // Fall through to the builtin fetcher on API error or empty body.
        }
        match self.builtin_fetch(url).await {
            Ok((title, body)) => {
                let head = if title.is_empty() {
                    format!("[web] {url}")
                } else {
                    format!("[web] {title} — {url}")
                };
                folded_response(
                    &call.id,
                    format!("{head}\n\n{body}"),
                    ToolState::Success,
                    limits,
                    blobs,
                )
            }
            Err(e) => ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error),
        }
    }

    async fn get_text(&self, url: &str, timeout_s: u64) -> Result<String, String> {
        let resp = self
            .client
            .get(url)
            .header("Accept-Language", ACCEPT_LANGUAGE)
            .timeout(Duration::from_secs(timeout_s.max(1)))
            .send()
            .await
            .map_err(|e| short_err(&e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = self.read_capped(resp).await?;
        Ok(decode_body(&bytes, &ct))
    }

    /// Stream the body up to `fetch_max_bytes`; a page cut mid-tag still
    /// extracts fine, so oversize is truncation, not an error.
    async fn read_capped(&self, mut resp: reqwest::Response) -> Result<Vec<u8>, String> {
        let cap = self.cfg.fetch_max_bytes.max(64 * 1024);
        let mut out: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| short_err(&e.to_string()))? {
            let room = cap.saturating_sub(out.len());
            if room == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        Ok(out)
    }

    async fn bing_search(&self, query: &str, n: usize) -> Result<Vec<Hit>, String> {
        let url = format!(
            "https://www.bing.com/search?q={}&count={}",
            percent_encode(query),
            n.max(10)
        );
        let html = self.get_text(&url, ENGINE_TIMEOUT_S).await?;
        Ok(parse_bing(&html, n))
    }

    async fn ddg_search(&self, query: &str, n: usize) -> Result<Vec<Hit>, String> {
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            percent_encode(query)
        );
        let html = self.get_text(&url, ENGINE_TIMEOUT_S).await?;
        Ok(parse_ddg(&html, n))
    }

    async fn builtin_fetch(&self, url: &str) -> Result<(String, String), String> {
        let resp = self
            .client
            .get(url)
            .header("Accept-Language", ACCEPT_LANGUAGE)
            .timeout(Duration::from_secs(self.cfg.timeout_s.max(1)))
            .send()
            .await
            .map_err(|e| short_err(&e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = self.read_capped(resp).await?;
        if ct.contains("text/html") || ct.contains("application/xhtml") || ct.is_empty() {
            let html = decode_body(&bytes, &ct);
            let title = html_title(&html);
            let body = extract_readable(&html);
            if body.trim().is_empty() {
                return Err("页面无可提取正文（可能需要 JS 渲染）".into());
            }
            return Ok((title, body));
        }
        if ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") {
            return Ok((String::new(), decode_body(&bytes, &ct)));
        }
        Err(format!(
            "不支持的内容类型 {ct}；二进制请用 bash curl -o 下载"
        ))
    }

    async fn tavily_search(&self, query: &str, n: usize) -> Result<Vec<Hit>, String> {
        let key = self.tavily_key.as_deref().ok_or(
            "Tavily 未配置 key（config.toml [web] tavily_api_key 或环境变量 TAVILY_API_KEY）",
        )?;
        let body = json!({
            "api_key": key,
            "query": query,
            "max_results": n,
            "search_depth": "basic",
            "include_answer": false,
        });
        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .bearer_auth(key)
            .json(&body)
            .timeout(Duration::from_secs(self.cfg.timeout_s.max(1)))
            .send()
            .await
            .map_err(|e| short_err(&e.to_string()))?;
        if !resp.status().is_success() {
            return Err(format!("tavily HTTP {}", resp.status().as_u16()));
        }
        let v: Value = resp.json().await.map_err(|e| short_err(&e.to_string()))?;
        let mut hits = Vec::new();
        if let Some(results) = v.get("results").and_then(|r| r.as_array()) {
            for r in results.iter().take(n) {
                hits.push(Hit {
                    title: r["title"].as_str().unwrap_or("").to_string(),
                    url: r["url"].as_str().unwrap_or("").to_string(),
                    snippet: clip(r["content"].as_str().unwrap_or(""), 300),
                });
            }
        }
        Ok(hits)
    }

    async fn tavily_extract(&self, url: &str) -> Result<String, String> {
        let key = self.tavily_key.as_deref().ok_or("no tavily key")?;
        let body = json!({ "api_key": key, "urls": [url] });
        let resp = self
            .client
            .post("https://api.tavily.com/extract")
            .bearer_auth(key)
            .json(&body)
            .timeout(Duration::from_secs(self.cfg.timeout_s.max(1)))
            .send()
            .await
            .map_err(|e| short_err(&e.to_string()))?;
        if !resp.status().is_success() {
            return Err(format!("tavily HTTP {}", resp.status().as_u16()));
        }
        let v: Value = resp.json().await.map_err(|e| short_err(&e.to_string()))?;
        let text = v["results"][0]["raw_content"].as_str().unwrap_or("");
        Ok(text.to_string())
    }
}

fn resolve_tavily_key(cfg: &WebConfig, mcp: &McpRegistry) -> Option<String> {
    let own = cfg.tavily_api_key.trim();
    if !own.is_empty() {
        return Some(own.to_string());
    }
    if let Ok(env) = std::env::var("TAVILY_API_KEY") {
        if !env.trim().is_empty() {
            return Some(env.trim().to_string());
        }
    }
    // Users who already wired tavily-mcp keep working without touching config.
    for srv in &mcp.servers {
        if let Some(k) = srv.env.get("TAVILY_API_KEY") {
            if !k.trim().is_empty() {
                return Some(k.trim().to_string());
            }
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn format_hits(_source: &str, query: &str, hits: &[Hit]) -> String {
    let mut s = format!("[web] {} 条结果：{query}\n", hits.len());
    for (i, h) in hits.iter().enumerate() {
        s.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title, h.url));
        if !h.snippet.is_empty() {
            s.push_str(&format!("   {}\n", h.snippet));
        }
    }
    s
}

fn short_err(e: &str) -> String {
    let mut s = e.replace('\n', " ");
    if s.len() > 200 {
        s.truncate(200);
    }
    s
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------- parsing --

/// Bing organic results: `<li class="b_algo">…<h2><a href=URL>TITLE</a></h2>`
/// with the snippet in the first following `<p>`.
fn parse_bing(html: &str, n: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<li class=\"b_algo\"") {
        let block_rest = &rest[start + 1..];
        let end = block_rest
            .find("<li class=\"b_algo\"")
            .unwrap_or(block_rest.len());
        let block = &block_rest[..end];
        if let Some(hit) = parse_bing_block(block) {
            hits.push(hit);
            if hits.len() >= n {
                break;
            }
        }
        rest = block_rest;
    }
    hits
}

fn parse_bing_block(block: &str) -> Option<Hit> {
    let h2 = block.find("<h2")?;
    let after_h2 = &block[h2..];
    let href_at = after_h2.find("href=\"")?;
    let href_rest = &after_h2[href_at + 6..];
    let href_end = href_rest.find('"')?;
    let mut url = href_rest[..href_end].to_string();
    url = decode_entities(&url);
    if let Some(real) = bing_unwrap_redirect(&url) {
        url = real;
    }
    if !url.starts_with("http") {
        return None;
    }
    let title_start = href_rest[href_end..].find('>')? + href_end + 1;
    let title_end = href_rest[title_start..].find("</a>")? + title_start;
    let title = clean_fragment(&href_rest[title_start..title_end]);
    let snippet = block
        .find("<p")
        .and_then(|p| {
            let pr = &block[p..];
            let open = pr.find('>')? + 1;
            let close = pr.find("</p>")?;
            (open < close).then(|| clean_fragment(&pr[open..close]))
        })
        .unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    Some(Hit {
        title,
        url,
        snippet: clip(&snippet, 300),
    })
}

/// Cookieless Bing sometimes wraps hrefs as `/ck/a?...&u=a1<base64url>&...`.
fn bing_unwrap_redirect(url: &str) -> Option<String> {
    if !url.contains("bing.com/ck/a") {
        return None;
    }
    let u = url.split("u=").nth(1)?.split('&').next()?;
    let b64 = u.strip_prefix("a1")?.trim_end_matches('=');
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(b64)
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    s.starts_with("http").then_some(s)
}

/// DuckDuckGo lite HTML: `<a class="result__a" href="//duckduckgo.com/l/?uddg=…">`.
fn parse_ddg(html: &str, n: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("class=\"result__a\"") {
        let seg = &rest[at..];
        let parsed = (|| {
            let href_at = seg.find("href=\"")?;
            let href_rest = &seg[href_at + 6..];
            let href_end = href_rest.find('"')?;
            let href = decode_entities(&href_rest[..href_end]);
            let url = ddg_unwrap_redirect(&href)?;
            let title_start = href_rest[href_end..].find('>')? + href_end + 1;
            let title_end = href_rest[title_start..].find("</a>")? + title_start;
            let title = clean_fragment(&href_rest[title_start..title_end]);
            let snippet = seg
                .find("result__snippet")
                .and_then(|s| {
                    let sr = &seg[s..];
                    let open = sr.find('>')? + 1;
                    let close = sr.find("</a>").or_else(|| sr.find("</td>"))?;
                    (open < close).then(|| clean_fragment(&sr[open..close]))
                })
                .unwrap_or_default();
            (!title.is_empty()).then_some(Hit {
                title,
                url,
                snippet: clip(&snippet, 300),
            })
        })();
        if let Some(hit) = parsed {
            hits.push(hit);
            if hits.len() >= n {
                break;
            }
        }
        rest = &rest[at + 17..];
    }
    hits
}

fn ddg_unwrap_redirect(href: &str) -> Option<String> {
    if let Some(enc) = href.split("uddg=").nth(1) {
        let enc = enc.split('&').next().unwrap_or(enc);
        let url = percent_decode(enc);
        return url.starts_with("http").then_some(url);
    }
    href.starts_with("http").then(|| href.to_string())
}

// ------------------------------------------------------------- extraction --

pub fn html_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(open) = lower.find("<title") else {
        return String::new();
    };
    let Some(gt) = lower[open..].find('>') else {
        return String::new();
    };
    let start = open + gt + 1;
    let Some(close) = lower[start..].find("</title") else {
        return String::new();
    };
    clean_fragment(&html[start..start + close])
}

/// Readability-lite without an HTML parser dependency: drop non-content
/// blocks, prefer `<article>`/`<main>`, turn block ends into newlines, strip
/// the rest. Good enough for docs/news/wiki; JS-only apps come back empty and
/// the caller says so.
pub fn extract_readable(html: &str) -> String {
    let mut s = strip_block(html, "script");
    s = strip_block(&s, "style");
    s = strip_block(&s, "noscript");
    s = strip_block(&s, "svg");
    s = strip_block(&s, "template");
    s = strip_comments(&s);
    let region = pick_region(&s);
    let mut r = strip_block(region, "nav");
    r = strip_block(&r, "header");
    r = strip_block(&r, "footer");
    r = strip_block(&r, "aside");
    r = strip_block(&r, "form");
    let text = tags_to_text(&r);
    collapse_blank(&text)
}

fn pick_region(html: &str) -> &str {
    for tag in ["article", "main", "body"] {
        if let Some(r) = find_element(html, tag) {
            return r;
        }
    }
    html
}

fn find_element<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open_tag = format!("<{tag}");
    let close_tag = format!("</{tag}");
    let mut from = 0;
    let start = loop {
        let at = lower[from..].find(&open_tag)? + from;
        let after = lower.as_bytes().get(at + open_tag.len()).copied();
        // `<main` must not match `<mainframe`.
        if matches!(after, Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\n')) {
            let gt = lower[at..].find('>')? + at;
            break gt + 1;
        }
        from = at + open_tag.len();
    };
    let end = lower[start..].find(&close_tag).map(|e| e + start)?;
    (start < end).then(|| &html[start..end])
}

/// Remove `<tag …>…</tag>` blocks, case-insensitive, unclosed tail included.
fn strip_block(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open_tag = format!("<{tag}");
    let close_tag = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find(&open_tag) {
        let at = pos + rel;
        let after = lower.as_bytes().get(at + open_tag.len()).copied();
        if !matches!(
            after,
            Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'/')
        ) {
            out.push_str(&html[pos..at + open_tag.len()]);
            pos = at + open_tag.len();
            continue;
        }
        out.push_str(&html[pos..at]);
        match lower[at..].find(&close_tag) {
            Some(rel_close) => {
                let close_at = at + rel_close;
                let skip = lower[close_at..]
                    .find('>')
                    .map(|g| close_at + g + 1)
                    .unwrap_or(lower.len());
                pos = skip;
            }
            None => return out,
        }
    }
    out.push_str(&html[pos..]);
    out
}

fn strip_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(rel) = html[pos..].find("<!--") {
        let at = pos + rel;
        out.push_str(&html[pos..at]);
        match html[at..].find("-->") {
            Some(rel_end) => pos = at + rel_end + 3,
            None => return out,
        }
    }
    out.push_str(&html[pos..]);
    out
}

/// Block-level closes become newlines, `<li>` becomes a bullet, then all
/// remaining tags are dropped and entities decoded.
fn tags_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] != b'<' {
            let next = html[pos..].find('<').map(|n| pos + n).unwrap_or(html.len());
            out.push_str(&html[pos..next]);
            pos = next;
            continue;
        }
        let end = match html[pos..].find('>') {
            Some(e) => pos + e + 1,
            None => break,
        };
        let tag = &lower[pos..end];
        if tag.starts_with("<br") {
            out.push('\n');
        } else if tag.starts_with("<li") {
            out.push_str("\n- ");
        } else if tag.starts_with("</p")
            || tag.starts_with("</div")
            || tag.starts_with("</li")
            || tag.starts_with("</h1")
            || tag.starts_with("</h2")
            || tag.starts_with("</h3")
            || tag.starts_with("</h4")
            || tag.starts_with("</h5")
            || tag.starts_with("</h6")
            || tag.starts_with("</tr")
            || tag.starts_with("</section")
            || tag.starts_with("</blockquote")
            || tag.starts_with("</pre")
            || tag.starts_with("</table")
            || tag.starts_with("</ul")
            || tag.starts_with("</ol")
        {
            out.push('\n');
        } else if tag.starts_with("</td") || tag.starts_with("</th") {
            out.push('\t');
        }
        pos = end;
    }
    decode_entities(&out)
}

fn collapse_blank(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(t);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn clean_fragment(s: &str) -> String {
    let no_tags = tags_to_text(s);
    no_tags.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --------------------------------------------------------------- encoding --

fn decode_body(bytes: &[u8], content_type: &str) -> String {
    let label = charset_of(content_type)
        .or_else(|| sniff_meta_charset(bytes))
        .unwrap_or_else(|| "utf-8".to_string());
    let enc = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = enc.decode(bytes);
    text.into_owned()
}

fn charset_of(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    let at = lower.find("charset=")?;
    let rest = &lower[at + 8..];
    let val: String = rest
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!val.is_empty()).then_some(val)
}

/// `<meta charset=…>` / `http-equiv` sniff over the first 4 KiB — GBK-era
/// Chinese pages rarely declare charset in the HTTP header.
fn sniff_meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    charset_of(&text)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // 字节层解析：输入来自外站 href，`%` 后可能紧跟多字节 UTF-8，
            // 对 &str 按字节切片会切在 char boundary 中间直接 panic。
            let v = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = v {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(semi) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[at + 1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" | "#x27" => Some('\''),
            "nbsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "middot" => Some('·'),
            _ => {
                if let Some(num) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(num, 16).ok().and_then(char::from_u32)
                } else if let Some(num) = ent.strip_prefix('#') {
                    num.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[at + semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities_decode_including_numeric() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt;"), "a & b <c>");
        assert_eq!(decode_entities("&#20013;&#x6587;"), "中文");
        assert_eq!(decode_entities("no entities"), "no entities");
        assert_eq!(decode_entities("dangling & rest"), "dangling & rest");
    }

    #[test]
    fn percent_roundtrip_handles_chinese() {
        let q = "苹果 M5 发布日期";
        let enc = percent_encode(q);
        assert!(!enc.contains(' '));
        assert_eq!(percent_decode(&enc), q);
    }

    #[test]
    fn percent_decode_survives_malicious_href() {
        // `%` 后紧跟多字节字符：旧实现按字节切 &str 直接 panic。
        assert_eq!(percent_decode("%中"), "%中");
        assert_eq!(percent_decode("a%中文b"), "a%中文b");
        // 非法/截断的转义原样保留。
        assert_eq!(percent_decode("%%"), "%%");
        assert_eq!(percent_decode("%%41"), "%A");
        assert_eq!(percent_decode("tail%a"), "tail%a");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%zz"), "%zz");
        // 合法转义仍工作。
        assert_eq!(percent_decode("%41+%E4%B8%AD"), "A 中");
    }

    #[test]
    fn ddg_redirect_unwraps_uddg() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust%2Dlang.org%2F&rut=abc";
        assert_eq!(
            ddg_unwrap_redirect(href).as_deref(),
            Some("https://www.rust-lang.org/")
        );
        assert_eq!(
            ddg_unwrap_redirect("https://direct.example/a").as_deref(),
            Some("https://direct.example/a")
        );
        assert!(ddg_unwrap_redirect("javascript:void(0)").is_none());
    }

    #[test]
    fn bing_ck_redirect_unwraps_base64() {
        use base64::Engine;
        let real = "https://example.com/page?x=1";
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(real);
        let href = format!("https://www.bing.com/ck/a?!&&p=abc&u=a1{b64}&ntb=1");
        assert_eq!(bing_unwrap_redirect(&href).as_deref(), Some(real));
        assert!(bing_unwrap_redirect("https://example.com/plain").is_none());
    }

    #[test]
    fn parses_bing_organic_block() {
        let html = r#"<ol id="b_results">
<li class="b_algo"><h2><a href="https://www.rust-lang.org/">Rust <b>语言</b></a></h2>
<div class="b_caption"><p>A language empowering everyone &amp; more.</p></div></li>
<li class="b_algo"><h2><a href="https://doc.rust-lang.org/book/">The Book</a></h2>
<p>Learn Rust.</p></li></ol>"#;
        let hits = parse_bing(html, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust 语言");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert_eq!(hits[0].snippet, "A language empowering everyone & more.");
        assert_eq!(hits[1].title, "The Book");
    }

    #[test]
    fn parses_ddg_lite_block() {
        let html = r##"<div class="result results_links">
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fdoc">Example <b>Doc</b></a>
<a class="result__snippet" href="#">Snippet text here.</a></div>"##;
        let hits = parse_ddg(html, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.org/doc");
        assert_eq!(hits[0].title, "Example Doc");
        assert_eq!(hits[0].snippet, "Snippet text here.");
    }

    #[test]
    fn readable_extraction_prefers_article_and_drops_chrome() {
        let html = r#"<html><head><title>新闻 &middot; 标题</title>
<script>var x = "<p>fake</p>";</script><style>p{color:red}</style></head>
<body><nav>首页 导航 很长</nav>
<article><h1>正文标题</h1><p>第一段。</p><p>第二段 &amp; 引用。</p>
<ul><li>要点一</li><li>要点二</li></ul></article>
<footer>版权所有</footer></body></html>"#;
        let text = extract_readable(html);
        assert!(text.contains("正文标题"), "{text}");
        assert!(text.contains("第一段。"), "{text}");
        assert!(text.contains("- 要点一"), "{text}");
        assert!(!text.contains("导航"), "{text}");
        assert!(!text.contains("版权所有"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
        assert_eq!(html_title(html), "新闻 · 标题");
    }

    #[test]
    fn gbk_body_is_decoded_via_meta_sniff() {
        let (gbk, _, _) = encoding_rs::GBK
            .encode("<html><head><meta charset=\"gbk\"></head><body><p>中文页面</p></body></html>");
        let text = decode_body(&gbk, "text/html");
        assert!(text.contains("中文页面"), "{text}");
    }

    #[test]
    fn charset_header_wins() {
        assert_eq!(
            charset_of("text/html; charset=GB2312").as_deref(),
            Some("gb2312")
        );
        assert_eq!(charset_of("text/html"), None);
    }

    #[test]
    fn provider_resolution_auto_prefers_key() {
        let mut cfg = WebConfig::default();
        cfg.tavily_api_key = "tvly-x".into();
        let r = WebRunner::new(cfg, &McpRegistry::default());
        assert_eq!(r.provider(), Provider::Tavily);

        let mut cfg = WebConfig::default();
        cfg.provider = "builtin".into();
        cfg.tavily_api_key = "tvly-x".into();
        let r = WebRunner::new(cfg, &McpRegistry::default());
        assert_eq!(r.provider(), Provider::Builtin);
    }

    #[test]
    fn provider_auto_sniffs_mcp_env_key() {
        use std::collections::BTreeMap;
        let mut env = BTreeMap::new();
        env.insert("TAVILY_API_KEY".to_string(), "tvly-from-mcp".to_string());
        let srv = crate::mcp::McpServer {
            name: "tavily".into(),
            command: "npx".into(),
            env,
            ..Default::default()
        };
        let reg = McpRegistry::with_servers(vec![srv], Duration::from_secs(30));
        let r = WebRunner::new(WebConfig::default(), &reg);
        // Host env TAVILY_API_KEY may also satisfy the lookup; either source
        // must resolve to the Tavily provider with a non-empty key.
        assert_eq!(r.provider(), Provider::Tavily);
        assert!(r.tavily_key.as_deref().is_some_and(|k| !k.is_empty()));
    }

    #[tokio::test]
    #[ignore = "live network"]
    async fn live_builtin_search_finds_results() {
        let mut cfg = WebConfig::default();
        cfg.provider = "builtin".into();
        let r = WebRunner::new(cfg, &McpRegistry::default());
        let call = ToolCall {
            id: "t1".into(),
            name: "web".into(),
            arguments: json!({"query": "Rust programming language"}),
        };
        let resp = r.run(&call, ToolLimits::default(), None).await;
        let text = resp.joined_text();
        eprintln!("--- builtin search ---\n{text}");
        assert_eq!(resp.state, ToolState::Success, "{text}");
        assert!(text.contains("1. "), "{text}");
        assert!(text.contains("http"), "{text}");
    }

    #[tokio::test]
    #[ignore = "live network"]
    async fn live_builtin_fetch_extracts_body() {
        let mut cfg = WebConfig::default();
        cfg.provider = "builtin".into();
        let r = WebRunner::new(cfg, &McpRegistry::default());
        let call = ToolCall {
            id: "t2".into(),
            name: "web".into(),
            arguments: json!({"url": "https://example.com/"}),
        };
        let resp = r.run(&call, ToolLimits::default(), None).await;
        let text = resp.joined_text();
        eprintln!("--- builtin fetch ---\n{text}");
        assert_eq!(resp.state, ToolState::Success, "{text}");
        assert!(text.contains("Example Domain"), "{text}");
    }

    #[tokio::test]
    #[ignore = "live network"]
    async fn live_tavily_search_via_env_key() {
        if std::env::var("TAVILY_API_KEY").is_err() {
            eprintln!("skip: TAVILY_API_KEY unset");
            return;
        }
        let r = WebRunner::new(WebConfig::default(), &McpRegistry::default());
        assert_eq!(r.provider(), Provider::Tavily);
        let call = ToolCall {
            id: "t3".into(),
            name: "web".into(),
            arguments: json!({"query": "Rust 1.80 release notes"}),
        };
        let resp = r.run(&call, ToolLimits::default(), None).await;
        let text = resp.joined_text();
        eprintln!("--- tavily search ---\n{text}");
        assert_eq!(resp.state, ToolState::Success, "{text}");
        assert!(text.contains("1. "), "{text}");
    }

    #[test]
    fn hit_format_is_uniform() {
        let hits = vec![Hit {
            title: "T".into(),
            url: "https://u".into(),
            snippet: "S".into(),
        }];
        let s = format_hits("bing", "q", &hits);
        assert_eq!(s, "[web] 1 条结果：q\n1. T\n   https://u\n   S\n");
    }
}
