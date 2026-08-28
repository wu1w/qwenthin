//! OpenAI-compat chat completion. Same request builder as probe.
//!
//! Streams when a token sink is armed (TUI/CLI) or when thinking is on and
//! `max_think_tokens > 0` so the watchdog can drop the body at the think cap.
//! XML `<tool_call>` is merged with OpenAI `tool_calls` after think-split.
//! When the native `tool_calls` array is empty, complete XML blocks are also
//! recovered from `reasoning_content` (QwenPaw `tag_parser` path for local Qwen
//! that leaks tools into think).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use reqwest::Client;
use serde_json::{json, Value};

use super::delta::StreamPaint;
use super::xml_tools::{calls_leak_channel_markup, extract_xml_tools, text_before_first};
use super::{Completer, ModelTurn, TokenSink};
use crate::adapter::{build_chat_body, ChatRequestSpec};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::family::{EndpointCaps, EngineProfile, Family};
use crate::policy::ThinkPolicy;
use crate::probe::{fetch_model, ProbeReport};
use crate::template::ChatMessage;
use crate::tokenize::count_tokens;
use crate::tool_calls::ToolCall;

pub struct HttpCompleter {
    client: Client,
    url: String,
    api_key: String,
    model: String,
    caps: EndpointCaps,
    policy: Mutex<ThinkPolicy>,
    token_sink: Mutex<Option<TokenSink>>,
    /// llama.cpp `--parallel` slot count; 0 = unknown (do not send `id_slot`).
    total_slots: u32,
    id_slot: Mutex<Option<i64>>,
    low_precision: AtomicBool,
}

impl HttpCompleter {
    pub async fn connect(cfg: &Config, policy: ThinkPolicy) -> Result<Self> {
        let client = crate::llm_http::stream_client(cfg)?;
        let (model, owned) = match configured_chat_model(cfg) {
            Some(model) => (model, None),
            None => {
                // GET /models must not inherit the 30-minute stream timeout.
                let probe = crate::llm_http::probe_client(cfg.server.connect_timeout_s.min(5), 15)?;
                fetch_model(&probe, cfg).await?
            }
        };
        let caps = caps_for(cfg, &model, owned.as_deref());
        let total_slots = if should_probe_slots(caps.profile) {
            fetch_total_slots(cfg).await
        } else {
            0
        };
        let mut policy = policy;
        if !caps.enable_thinking {
            policy = ThinkPolicy::off();
            policy.max_tokens = cfg.policy.max_tokens;
        }
        Ok(Self {
            client,
            url: format!(
                "{}/chat/completions",
                cfg.server.base_url.trim_end_matches('/')
            ),
            api_key: cfg.server.api_key.clone(),
            model,
            caps,
            policy: Mutex::new(policy),
            token_sink: Mutex::new(None),
            total_slots,
            id_slot: Mutex::new(None),
            low_precision: AtomicBool::new(cfg.policy.low_precision),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_policy(&self, p: ThinkPolicy) {
        let mut g = lock_policy(&self.policy);
        if *g != p {
            *g = p;
        }
    }

    pub fn policy(&self) -> ThinkPolicy {
        lock_policy(&self.policy).clone()
    }

    fn token_sink(&self) -> Option<TokenSink> {
        lock_sink(&self.token_sink).clone()
    }

    async fn post(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        stream: bool,
    ) -> Result<reqwest::Response> {
        let policy = self.policy();
        let pin = matches!(
            self.caps.profile,
            crate::family::EngineProfile::LlamaCpp | crate::family::EngineProfile::Auto
        );
        let body = build_chat_body(&ChatRequestSpec {
            model: &self.model,
            messages,
            tools,
            stream,
            policy: &policy,
            caps: &self.caps,
            id_slot: if pin {
                lock_slot(&self.id_slot).clone()
            } else {
                None
            },
            cache_prompt: pin,
            lossy_repeat: self.low_precision.load(Ordering::Relaxed),
        });
        let mut req = self.client.post(&self.url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        Ok(req.send().await?.error_for_status()?)
    }
}

impl Completer for HttpCompleter {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<ModelTurn> {
        let policy = self.policy();
        let sink = self.token_sink();
        let watchdog = if policy.enabled && policy.max_think_tokens > 0 {
            Some(policy.max_think_tokens)
        } else {
            None
        };
        let stream = sink.is_some() || watchdog.is_some();
        let mut resp = self.post(messages, tools, stream).await?;
        if stream {
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if ct.contains("application/json") {
                let v: Value = resp.json().await?;
                let turn = turn_from_json(&v, false)?;
                paint_clean(sink, &turn);
                return Ok(turn);
            }
            match read_sse(&mut resp, self.caps.family, watchdog, sink).await {
                Ok(turn) => return Ok(turn),
                Err(Error::Watchdog) => {
                    drop(resp);
                    return Ok(ModelTurn::watchdog());
                }
                Err(e) => return Err(e),
            }
        }
        let v: Value = resp.json().await?;
        let turn = turn_from_json(&v, false)?;
        paint_clean(sink, &turn);
        Ok(turn)
    }

    fn prefix_meter(&self) -> Option<(Family, crate::policy::TemplateKwargs)> {
        let policy = self.policy();
        Some((self.caps.family, policy.template_kwargs(&self.caps)))
    }

    fn set_policy(&self, p: ThinkPolicy) {
        HttpCompleter::set_policy(self, p);
    }

    fn policy(&self) -> Option<ThinkPolicy> {
        Some(HttpCompleter::policy(self))
    }

    fn set_token_sink(&self, sink: Option<TokenSink>) {
        *lock_sink(&self.token_sink) = sink;
    }

    fn pin_session(&self, session_id: &str) {
        *lock_slot(&self.id_slot) = session_slot(session_id, self.total_slots);
    }

    fn set_low_precision(&self, on: bool) {
        self.low_precision.store(on, Ordering::Relaxed);
    }

    fn media_caps(&self) -> crate::media::MediaCaps {
        let origin = self
            .url
            .strip_suffix("/chat/completions")
            .map(|u| u.to_string())
            .unwrap_or_else(|| self.url.clone());
        self.caps.media_caps(Some((origin, self.api_key.clone())))
    }
}

fn caps_for(cfg: &Config, model: &str, owned: Option<&str>) -> EndpointCaps {
    if let Ok(path) = Config::probe_path() {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(report) = serde_json::from_str::<ProbeReport>(&raw) {
                if report.red.is_empty() {
                    return report.to_caps();
                }
            }
        }
    }
    let family = cfg.server.family.resolve(model);
    let profile = cfg
        .server
        .profile
        .resolve(&cfg.server.base_url, model, owned);
    EndpointCaps::for_family(family, profile)
}

fn configured_chat_model(cfg: &Config) -> Option<String> {
    let m = cfg.server.model.trim();
    if m.is_empty() {
        None
    } else {
        Some(m.to_string())
    }
}

fn should_probe_slots(profile: EngineProfile) -> bool {
    matches!(profile, EngineProfile::LlamaCpp)
}

fn lock_policy(m: &Mutex<ThinkPolicy>) -> std::sync::MutexGuard<'_, ThinkPolicy> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_sink(m: &Mutex<Option<TokenSink>>) -> std::sync::MutexGuard<'_, Option<TokenSink>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_slot(m: &Mutex<Option<i64>>) -> std::sync::MutexGuard<'_, Option<i64>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Pin a session onto a stable llama.cpp slot. When `total_slots > 1`, the last
/// slot is left for unpinned probes so they cannot wipe a long prefix.
pub fn session_slot(session_id: &str, total_slots: u32) -> Option<i64> {
    if total_slots == 0 {
        return None;
    }
    if total_slots == 1 {
        return Some(0);
    }
    let usable = u64::from(total_slots.saturating_sub(1));
    let digest = crate::vendor::sha256_hex(session_id.as_bytes());
    let n = u64::from_str_radix(&digest[..8], 16).unwrap_or(0);
    Some((n % usable) as i64)
}

async fn fetch_total_slots(cfg: &Config) -> u32 {
    let Ok(client) = crate::llm_http::probe_client(1, 2) else {
        return 0;
    };
    let base = cfg.server.base_url.trim_end_matches('/');
    let candidates = [
        format!("{base}/props"),
        format!("{base}/slots"),
        base.strip_suffix("/v1")
            .map(|root| format!("{root}/props"))
            .unwrap_or_default(),
        base.strip_suffix("/v1")
            .map(|root| format!("{root}/slots"))
            .unwrap_or_default(),
    ];
    for url in candidates {
        if url.is_empty() {
            continue;
        }
        let mut req = client.get(&url);
        if !cfg.server.api_key.is_empty() {
            req = req.bearer_auth(&cfg.server.api_key);
        }
        let Ok(resp) = req.send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(v) = resp.json::<Value>().await else {
            continue;
        };
        if let Some(n) = pointer_u64(&v, "/total_slots").or_else(|| pointer_u64(&v, "/slots")) {
            if n > 0 && n < 10_000 {
                return n as u32;
            }
        }
        if let Some(arr) = v.as_array() {
            if !arr.is_empty() && arr.len() < 10_000 {
                return arr.len() as u32;
            }
        }
    }
    0
}

fn paint_clean(sink: Option<TokenSink>, turn: &ModelTurn) {
    let Some(sink) = sink else {
        return;
    };
    if turn.watchdog_hit {
        return;
    }
    if turn.reasoning.is_empty() && turn.content.is_empty() {
        return;
    }
    let mut paint = StreamPaint::new(sink);
    paint.push_clean(&turn.reasoning, &turn.content);
}

fn turn_from_json(v: &Value, truncated: bool) -> Result<ModelTurn> {
    if let Some(err) = v.get("error") {
        return Err(Error::Http(err.to_string()));
    }
    Ok(if truncated {
        parse_turn_opts(v, true)?.into_turn()
    } else {
        parse_turn(v)?.into_turn()
    })
}

/// Parsed completion plus a parse-fail signal for the agent loop.
#[derive(Debug)]
pub struct ParseOutcome {
    pub turn: ModelTurn,
    pub parse_fail: bool,
}

impl ParseOutcome {
    fn into_turn(mut self) -> ModelTurn {
        self.turn.parse_fail = self.parse_fail;
        self.turn
    }
}

pub fn parse_turn(v: &Value) -> Result<ParseOutcome> {
    parse_turn_opts(v, false)
}

fn parse_turn_opts(v: &Value, truncated: bool) -> Result<ParseOutcome> {
    let msg = &v["choices"][0]["message"];
    if msg.is_null() {
        return Err(Error::Http(
            "chat completion missing choices[0].message".into(),
        ));
    }
    let mut content = message_text(msg);
    let mut reasoning = msg["reasoning_content"]
        .as_str()
        .or_else(|| msg["reasoning"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if reasoning.is_empty() {
        let (think, rest) = split_think(&content);
        reasoning = think;
        content = rest;
    } else {
        content = strip_think(&content);
    }

    let raw_openai = msg["tool_calls"].as_array().cloned();
    let openai_calls: Vec<ToolCall> = raw_openai
        .as_ref()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| parse_tool_call(item, truncated))
                .collect()
        })
        .unwrap_or_default();

    let native = !openai_calls.is_empty();
    let xml = extract_xml_tools(&content);
    let xml_unclosed = xml.unclosed;
    let had_xml = xml.had_tag();
    let xml_ranges = xml.ranges.clone();
    let mut parse_fail = xml.parse_fail(truncated) && !native;

    let mut tool_calls = if parse_fail {
        Vec::new()
    } else if truncated && xml_unclosed {
        // Incomplete cancelled XML is discarded; keep complete OpenAI calls.
        openai_calls
    } else {
        merge_calls(openai_calls, xml.calls)
    };

    // QwenPaw: with no native tool_use, recover complete <tool_call> blocks from
    // thinking. Incomplete/malformed think XML is skipped, not a parse_fail.
    if !native {
        let from_think = extract_xml_tools(&reasoning);
        if !from_think.calls.is_empty() {
            if parse_fail {
                parse_fail = false;
                tool_calls = from_think.calls;
            } else {
                tool_calls = merge_calls(from_think.calls, tool_calls);
            }
            reasoning = text_before_first(&reasoning, &from_think.ranges);
        }
    }

    // Native llama.cpp arguments can swallow `</think>` + the next XML call.
    // Do not execute; empty-plural / truncated paths stay as they are.
    if !truncated && calls_leak_channel_markup(&tool_calls) {
        parse_fail = true;
        tool_calls.clear();
    }

    if !parse_fail && had_xml {
        content = text_before_first(&content, &xml_ranges);
    }

    let raw_tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(super::openai_tool_calls(&tool_calls))
    };

    Ok(ParseOutcome {
        turn: ModelTurn {
            content: content.trim().to_string(),
            reasoning,
            tool_calls,
            raw_tool_calls,
            prompt_tokens: parse_prompt_tokens(v),
            completion_tokens: parse_completion_tokens(v),
            watchdog_hit: false,
            parse_fail,
            cached_tokens: parse_cached_tokens(v),
            decode_tok_s: parse_decode_tok_s(v),
        },
        parse_fail,
    })
}

fn json_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_i64().and_then(|i| u64::try_from(i).ok()).or_else(|| {
                n.as_f64().and_then(|f| {
                    if f.is_finite() && f >= 0.0 && f <= u64::MAX as f64 {
                        Some(f.round() as u64)
                    } else {
                        None
                    }
                })
            })
        }),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn pointer_u64(v: &Value, path: &str) -> Option<u64> {
    v.pointer(path).and_then(json_u64)
}

/// Prompt size: OpenAI `usage.prompt_tokens`, else llama.cpp
/// `timings.cache_n + timings.prompt_n` (`prompt_n` is uncached prefill).
pub fn parse_prompt_tokens(v: &Value) -> u64 {
    if let Some(n) = pointer_u64(v, "/usage/prompt_tokens") {
        if n > 0 {
            return n;
        }
    }
    let cache = pointer_u64(v, "/timings/cache_n").unwrap_or(0);
    if let Some(prompt_n) = pointer_u64(v, "/timings/prompt_n") {
        return cache.saturating_add(prompt_n);
    }
    0
}

pub fn parse_completion_tokens(v: &Value) -> u64 {
    pointer_u64(v, "/usage/completion_tokens")
        .or_else(|| pointer_u64(v, "/timings/predicted_n"))
        .unwrap_or(0)
}

/// llama.cpp `timings.predicted_per_second`, else `predicted_n / predicted_ms`.
pub fn parse_decode_tok_s(v: &Value) -> Option<f64> {
    let direct = v
        .pointer("/timings/predicted_per_second")
        .and_then(json_f64)
        .filter(|n| n.is_finite() && *n > 0.0);
    if direct.is_some() {
        return direct;
    }
    let n = pointer_u64(v, "/timings/predicted_n").unwrap_or(0) as f64;
    let ms = v.pointer("/timings/predicted_ms").and_then(json_f64)?;
    if n > 0.0 && ms.is_finite() && ms > 0.0 {
        Some(n / (ms / 1000.0))
    } else {
        None
    }
}

fn json_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// OpenAI `prompt_tokens_details.cached_tokens`, llama.cpp `timings.cache_n`.
pub fn parse_cached_tokens(v: &Value) -> Option<u64> {
    let n = pointer_u64(v, "/usage/prompt_tokens_details/cached_tokens")
        .or_else(|| pointer_u64(v, "/usage/cached_tokens"))
        .or_else(|| pointer_u64(v, "/usage/cache_n"))
        .or_else(|| pointer_u64(v, "/timings/cache_n"));
    let prompt = parse_prompt_tokens(v);
    n.map(|c| if prompt > 0 { c.min(prompt) } else { c })
}

fn merge_calls(openai: Vec<ToolCall>, xml: Vec<ToolCall>) -> Vec<ToolCall> {
    let mut seen: std::collections::HashSet<String> = openai.iter().map(|c| c.id.clone()).collect();
    let mut out = openai;
    for call in xml {
        if seen.contains(&call.id) {
            continue;
        }
        seen.insert(call.id.clone());
        out.push(call);
    }
    out
}

fn parse_tool_call(v: &Value, truncated: bool) -> Option<ToolCall> {
    let id = v["id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let name = v["function"]["name"].as_str()?.to_string();
    if name.is_empty() {
        return None;
    }
    let arguments = match &v["function"]["arguments"] {
        Value::String(s) => match serde_json::from_str(s) {
            Ok(parsed) => parsed,
            Err(_) if truncated => return None,
            Err(_) => Value::String(s.clone()),
        },
        other => other.clone(),
    };
    Some(ToolCall {
        id,
        name,
        arguments,
    })
}

fn message_text(msg: &Value) -> String {
    match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn split_think(content: &str) -> (String, String) {
    let mut reasoning = String::new();
    let mut out = String::new();
    let mut rest = content;
    loop {
        let Some(start) = rest.find("<think>") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 7..];
        match after_open.find("</think>") {
            Some(end) => {
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(after_open[..end].trim());
                rest = &after_open[end + 8..];
            }
            None => {
                // QwenPaw extract_thinking_from_text: unclosed <think> → remaining
                // text is before the tag; the rest is thought. Tool recovery from
                // that thought happens in parse_turn, not here.
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(after_open.trim());
                break;
            }
        }
    }
    (reasoning, out)
}

fn strip_think(content: &str) -> String {
    split_think(content).1
}

struct StreamAcc {
    content: String,
    reasoning: String,
    tool_calls: Vec<Value>,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: Option<u64>,
    timings: Option<Value>,
}

impl StreamAcc {
    fn new() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: None,
            timings: None,
        }
    }

    fn apply(&mut self, chunk: &Value) {
        if let Some(n) = parse_cached_tokens(chunk) {
            self.cached_tokens = Some(n);
        }
        if chunk.get("timings").is_some() {
            self.timings = Some(chunk["timings"].clone());
        }
        let prompt = parse_prompt_tokens(chunk);
        if prompt > 0 {
            self.prompt_tokens = prompt;
        }
        let completion = parse_completion_tokens(chunk);
        if completion > 0 {
            self.completion_tokens = completion;
        }
        let choice = &chunk["choices"][0];
        if choice.is_null() {
            return;
        }
        if choice.get("delta").is_none() {
            if let Some(msg) = choice.get("message") {
                if msg.is_object() {
                    self.content = message_text(msg);
                    self.reasoning = msg["reasoning_content"]
                        .as_str()
                        .or_else(|| msg["reasoning"].as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(arr) = msg["tool_calls"].as_array() {
                        self.tool_calls = arr.clone();
                    }
                    return;
                }
            }
        }
        let delta = &choice["delta"];
        if let Some(s) = delta["content"].as_str() {
            self.content.push_str(s);
        }
        if let Some(s) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
        {
            self.reasoning.push_str(s);
        }
        if let Some(arr) = delta["tool_calls"].as_array() {
            for tc in arr {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(json!({
                        "type": "function",
                        "function": {"name": "", "arguments": ""}
                    }));
                }
                merge_tool_delta(&mut self.tool_calls[idx], tc);
            }
        }
    }

    fn into_message_json(self) -> Value {
        let tool_calls = if self.tool_calls.is_empty() {
            Value::Null
        } else {
            Value::Array(self.tool_calls)
        };
        let mut usage = json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
        });
        if let Some(cached) = self.cached_tokens {
            usage["prompt_tokens_details"] = json!({"cached_tokens": cached});
            usage["cache_n"] = json!(cached);
        }
        let mut root = json!({
            "choices": [{
                "message": {
                    "content": self.content,
                    "reasoning_content": self.reasoning,
                    "tool_calls": tool_calls,
                }
            }],
            "usage": usage,
        });
        if let Some(t) = self.timings {
            root["timings"] = t;
        }
        root
    }
}

fn merge_tool_delta(dst: &mut Value, src: &Value) {
    if let Some(id) = src["id"].as_str() {
        dst["id"] = json!(id);
    }
    if let Some(t) = src["type"].as_str() {
        dst["type"] = json!(t);
    }
    let f = &src["function"];
    if f.is_null() {
        return;
    }
    if dst.get("function").is_none() {
        dst["function"] = json!({"name": "", "arguments": ""});
    }
    if let Some(n) = f["name"].as_str() {
        if !n.is_empty() {
            dst["function"]["name"] = json!(n);
        }
    }
    match &f["arguments"] {
        Value::String(s) => {
            let cur = dst["function"]["arguments"]
                .as_str()
                .unwrap_or("")
                .to_string();
            dst["function"]["arguments"] = json!(format!("{cur}{s}"));
        }
        Value::Null => {}
        other => dst["function"]["arguments"] = other.clone(),
    }
}

async fn read_sse(
    resp: &mut reqwest::Response,
    family: Family,
    watchdog: Option<u32>,
    sink: Option<TokenSink>,
) -> Result<ModelTurn> {
    let mut acc = StreamAcc::new();
    let mut paint = sink.map(StreamPaint::new);
    let mut sse = SseBuf::default();
    let mut pending = Vec::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(e) => return Err(Error::Http(e.to_string())),
        };
        pending.extend_from_slice(&chunk);
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) => e.valid_up_to(),
        };
        if valid == 0 {
            continue;
        }
        let text = std::str::from_utf8(&pending[..valid]).expect("valid_up_to");
        let events = sse.push(text);
        pending.drain(..valid);
        for event in events {
            if let Some(err) = event.get("error") {
                return Err(Error::Http(err.to_string()));
            }
            acc.apply(&event);
            if let Some(p) = paint.as_mut() {
                p.push_raw(&acc.reasoning, &acc.content, !acc.tool_calls.is_empty());
            }
            if watchdog_hit(family, watchdog, &acc) {
                return Err(Error::Watchdog);
            }
        }
    }
    let leftover = sse.flush();
    for event in leftover {
        if let Some(err) = event.get("error") {
            return Err(Error::Http(err.to_string()));
        }
        acc.apply(&event);
        if let Some(p) = paint.as_mut() {
            p.push_raw(&acc.reasoning, &acc.content, !acc.tool_calls.is_empty());
        }
        if watchdog_hit(family, watchdog, &acc) {
            return Err(Error::Watchdog);
        }
    }
    turn_from_json(&acc.into_message_json(), false)
}

fn watchdog_hit(family: Family, cap: Option<u32>, acc: &StreamAcc) -> bool {
    let Some(cap) = cap else {
        return false;
    };
    count_think_tokens(family, &acc.reasoning, &acc.content) >= cap
}

fn count_think_tokens(family: Family, reasoning: &str, content: &str) -> u32 {
    let blob = think_blob(reasoning, content);
    if blob.is_empty() {
        return 0;
    }
    count_tokens(family, &blob).unwrap_or_else(|_| {
        let n = blob.split_whitespace().count() as u32;
        n.max(1)
    })
}

fn think_blob(reasoning: &str, content: &str) -> String {
    if !reasoning.is_empty() {
        return reasoning.to_string();
    }
    let Some(start) = content.find("<think>") else {
        return String::new();
    };
    let rest = &content[start + 7..];
    match rest.find("</think>") {
        Some(end) => rest[..end].to_string(),
        None => rest.to_string(),
    }
}

#[derive(Default)]
struct SseBuf {
    leftover: String,
}

impl SseBuf {
    fn push(&mut self, chunk: &str) -> Vec<Value> {
        self.leftover.push_str(chunk);
        let mut out = Vec::new();
        while let Some(idx) = find_event_break(&self.leftover) {
            let raw = self.leftover[..idx].to_string();
            let skip = if self.leftover[idx..].starts_with("\r\n\r\n") {
                4
            } else {
                2
            };
            self.leftover = self.leftover[idx + skip..].to_string();
            if let Some(v) = parse_sse_event(&raw) {
                out.push(v);
            }
        }
        out
    }

    fn flush(&mut self) -> Vec<Value> {
        if self.leftover.trim().is_empty() {
            return Vec::new();
        }
        let raw = std::mem::take(&mut self.leftover);
        parse_sse_event(&raw).into_iter().collect()
    }
}

fn find_event_break(s: &str) -> Option<usize> {
    match (s.find("\r\n\r\n"), s.find("\n\n")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_sse_event(raw: &str) -> Option<Value> {
    let mut data = String::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        if rest == "[DONE]" {
            return None;
        }
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest);
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_openai_tool_call() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.tool_calls[0].name, "read");
        assert_eq!(o.turn.tool_calls[0].arguments["path"], "a.rs");
        assert!(o.turn.content.is_empty());
        assert!(!o.turn.watchdog_hit);
    }

    #[test]
    fn parses_xml_tool_call() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "<tool_call>\n<function=read>\n<parameter=path>\nnote.txt\n</parameter>\n</function>\n</tool_call>"
                }
            }],
            "usage": {}
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.tool_calls[0].name, "read");
        assert_eq!(o.turn.tool_calls[0].arguments["path"], "note.txt");
        assert!(!o.turn.content.contains("tool_call"));
    }

    #[test]
    fn parses_json_inside_xml() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "<tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}</tool_call>"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert_eq!(o.turn.tool_calls[0].name, "bash");
        assert_eq!(o.turn.tool_calls[0].arguments["command"], "ls");
    }

    #[test]
    fn merges_openai_and_xml() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "note\n<tool_call>{\"id\":\"c2\",\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}</tool_call>",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}
                    }]
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert_eq!(o.turn.tool_calls.len(), 2);
        assert_eq!(o.turn.tool_calls[0].id, "c1");
        assert_eq!(o.turn.tool_calls[0].name, "read");
        assert_eq!(o.turn.tool_calls[1].id, "c2");
        assert_eq!(o.turn.tool_calls[1].name, "bash");
        assert!(!o.turn.content.contains("tool_call"));
        assert!(o.turn.content.contains("note"));
    }

    #[test]
    fn openai_wins_on_id() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "<tool_call>{\"id\":\"c1\",\"name\":\"bash\",\"arguments\":{\"command\":\"pwd\"}}</tool_call>",
                    "tool_calls": [{
                        "id": "c1",
                        "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}
                    }]
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.tool_calls[0].name, "read");
    }

    #[test]
    fn malformed_xml_is_parse_fail() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "<tool_call>this is not a tool</tool_call>"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(o.parse_fail);
        assert!(o.turn.parse_fail);
        assert!(o.turn.tool_calls.is_empty());
        assert!(o.turn.content.contains("tool_call"));
    }

    #[test]
    fn truncated_incomplete_json_not_parse_fail() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "function": {"name": "read", "arguments": "{\"path\":"}
                    }]
                }
            }]
        });
        let o = parse_turn_opts(&v, true).unwrap();
        assert!(!o.parse_fail);
        assert!(o.turn.tool_calls.is_empty());
    }

    #[test]
    fn truncated_unclosed_xml_not_parse_fail() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "<tool_call>\n<function=read>\n<parameter=path>\nno"
                }
            }]
        });
        let o = parse_turn_opts(&v, true).unwrap();
        assert!(!o.parse_fail);
        assert!(o.turn.tool_calls.is_empty());
    }

    #[test]
    fn splits_think_block() {
        let (think, rest) = split_think("pre<think>\nabc\n</think>\npost");
        assert_eq!(think, "abc");
        assert_eq!(rest.trim(), "pre\npost");
    }

    #[test]
    fn unclosed_think_is_all_thought() {
        let (think, rest) = split_think("pre<think>\nplan\n<tool_call>nope</tool_call>");
        assert!(think.contains("plan"));
        assert!(think.contains("tool_call"));
        assert_eq!(rest, "pre");
    }

    #[test]
    fn recovers_tool_calls_from_think_and_content() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "<think>no\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"secret.rs\"}}</tool_call>\n</think>\nI'll read it\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"note.txt\"}}</tool_call>"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert_eq!(o.turn.tool_calls.len(), 2);
        assert_eq!(o.turn.tool_calls[0].arguments["path"], "secret.rs");
        assert_eq!(o.turn.tool_calls[1].arguments["path"], "note.txt");
        assert!(o.turn.reasoning.contains("no"));
        assert!(!o.turn.reasoning.contains("tool_call"));
        assert!(o.turn.content.contains("I'll read it"));
        assert!(!o.turn.content.contains("tool_call"));
    }

    #[test]
    fn recovers_complete_tool_call_from_unclosed_think() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "<think>\nplan\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}</tool_call>"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.tool_calls[0].arguments["path"], "a.rs");
        assert!(o.turn.reasoning.contains("plan"));
        assert!(!o.turn.reasoning.contains("tool_call"));
    }

    #[test]
    fn recovers_from_reasoning_content_channel() {
        let v = json!({
            "choices": [{
                "message": {
                    "reasoning_content": "plan\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}</tool_call>",
                    "content": "I'll read it"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.tool_calls[0].arguments["path"], "a.rs");
        assert_eq!(o.turn.reasoning, "plan");
        assert_eq!(o.turn.content, "I'll read it");
    }

    #[test]
    fn native_tool_calls_skip_think_recovery() {
        let v = json!({
            "choices": [{
                "message": {
                    "reasoning_content": "maybe\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"secret.rs\"}}</tool_call>",
                    "content": "",
                    "tool_calls": [{
                        "id": "c1",
                        "function": {"name": "read", "arguments": "{\"path\":\"note.txt\"}"}
                    }]
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.tool_calls[0].arguments["path"], "note.txt");
        assert!(o.turn.reasoning.contains("secret.rs"));
    }

    #[test]
    fn preamble_before_tool_call_stays_in_content() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "I'll read the note first.\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"note.txt\"}}</tool_call>"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert!(o.turn.content.contains("I'll read the note first."));
        assert!(!o.turn.content.contains("tool_call"));
    }

    #[test]
    fn drops_text_after_tool_call() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "I'll read it.\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"note.txt\"}}</tool_call>\noops leftover"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.content, "I'll read it.");
        assert!(!o.turn.content.contains("oops"));
    }

    #[test]
    fn empty_plural_tool_calls_is_parse_fail() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "先确认上传文件是否存在。\n\n<tool_calls>\n</tool_calls>\n\n<tool_result>\n</tool_result>\n\n文件存在，读取内容。"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(o.parse_fail);
        assert!(o.turn.tool_calls.is_empty());
        assert_eq!(o.turn.content, "先确认上传文件是否存在。\n\n<tool_calls>\n</tool_calls>\n\n<tool_result>\n</tool_result>\n\n文件存在，读取内容。");
    }

    #[test]
    fn plural_tool_calls_with_function_extracts() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "先读。\n<tool_calls>\n<function=read>\n<parameter=path>\na.pdf\n</parameter>\n</function>\n</tool_calls>"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert_eq!(o.turn.tool_calls[0].name, "read");
        assert_eq!(o.turn.content, "先读。");
    }

    #[test]
    fn native_bash_args_with_think_close_are_parse_fail() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "9epb8FKpEkKV3RHlKZLhXbu4JOJOVFjw",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"cd /Users/william/q-harness\\n</think>\\n\\n<tool_call>\\n<function=bash>\\n<parameter=command>\\ncd /Users/william/q-harness && grep -n \\\"extract_xml_tools\\\" crates/q38-loop/src -r\"}"
                        }
                    }]
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(o.parse_fail);
        assert!(o.turn.tool_calls.is_empty());
    }

    #[test]
    fn native_grep_for_think_tag_still_runs() {
        let v = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"grep -n \\\"</think>\\\" crates/q38-loop/src/agent/delta.rs\"}"
                        }
                    }]
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert_eq!(o.turn.tool_calls.len(), 1);
        assert!(o.turn.tool_calls[0].arguments["command"]
            .as_str()
            .unwrap()
            .contains("</think>"));
    }

    #[test]
    fn unclosed_tool_call_in_think_is_not_executed() {
        let v = json!({
            "choices": [{
                "message": {
                    "reasoning_content": "plan\n<tool_call>\n<function=read>\n<parameter=path>\nnote",
                    "content": "still thinking"
                }
            }]
        });
        let o = parse_turn(&v).unwrap();
        assert!(!o.parse_fail);
        assert!(o.turn.tool_calls.is_empty());
        assert!(o.turn.reasoning.contains("plan"));
        assert_eq!(o.turn.content, "still thinking");
    }

    #[test]
    fn parses_openai_and_llamacpp_cached_tokens() {
        let openai = json!({
            "choices": [{"message": {"content": "ok"}}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 4,
                "prompt_tokens_details": {"cached_tokens": 80}
            }
        });
        assert_eq!(parse_cached_tokens(&openai), Some(80));
        assert_eq!(parse_turn(&openai).unwrap().turn.cached_tokens, Some(80));

        let llama = json!({
            "choices": [{"message": {"content": "ok"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 4},
            "timings": {"cache_n": 95, "prompt_n": 5}
        });
        assert_eq!(parse_cached_tokens(&llama), Some(95));
        assert_eq!(parse_turn(&llama).unwrap().turn.prompt_tokens, 100);
        assert_eq!(
            parse_cached_tokens(&json!({"usage": {"prompt_tokens": 10}})),
            None
        );

        // Stream last chunk: timings only (no usage) — llama.cpp prompt_n is uncached prefill.
        let timings_only = json!({
            "choices": [{"message": {"content": "ok"}}],
            "timings": {"cache_n": 56, "prompt_n": 4, "predicted_n": 8}
        });
        let t = parse_turn(&timings_only).unwrap().turn;
        assert_eq!(t.prompt_tokens, 60);
        assert_eq!(t.completion_tokens, 8);
        assert_eq!(t.cached_tokens, Some(56));
        assert_eq!(t.decode_tok_s, None);

        let with_rate = json!({
            "choices": [{"message": {"content": "ok"}}],
            "timings": {
                "predicted_n": 80,
                "predicted_ms": 2000,
                "predicted_per_second": 40.0
            }
        });
        assert_eq!(
            parse_turn(&with_rate).unwrap().turn.decode_tok_s,
            Some(40.0)
        );

        let from_ms = json!({
            "choices": [{"message": {"content": "ok"}}],
            "timings": {"predicted_n": 80, "predicted_ms": 2000.0}
        });
        let rate = parse_turn(&from_ms).unwrap().turn.decode_tok_s.unwrap();
        assert!((rate - 40.0).abs() < 1e-6, "{rate}");

        let mut acc = StreamAcc::new();
        acc.apply(&json!({"choices":[{"delta":{"content":"3"}}]}));
        acc.apply(&timings_only);
        let rebuilt = acc.into_message_json();
        assert_eq!(rebuilt["usage"]["prompt_tokens"], 60);
        assert_eq!(
            rebuilt["usage"]["prompt_tokens_details"]["cached_tokens"],
            56
        );
        let from_stream = parse_turn(&rebuilt).unwrap().turn;
        assert_eq!(from_stream.prompt_tokens, 60);
        assert_eq!(from_stream.cached_tokens, Some(56));
    }

    #[test]
    fn session_slot_reserves_last_when_parallel() {
        assert_eq!(session_slot("abc", 0), None);
        assert_eq!(session_slot("abc", 1), Some(0));
        let a = session_slot("soak-one", 4).unwrap();
        let b = session_slot("soak-one", 4).unwrap();
        assert_eq!(a, b);
        assert!(a < 3, "last slot reserved for unpinned probes, got {a}");
        assert_eq!(session_slot("other", 4).unwrap() < 3, true);
    }

    #[test]
    fn sse_parses_data_lines() {
        let mut buf = SseBuf::default();
        let events = buf.push("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "hi");
        assert!(buf.push("data: [DONE]\n\n").is_empty());
    }

    #[test]
    fn think_count_prefers_reasoning_content() {
        let n = count_think_tokens(Family::Qwen35, "one two three", "<think>ignored</think>");
        assert_eq!(n, 3);
    }

    #[test]
    fn configured_model_skips_models_list() {
        let mut cfg = Config::default();
        cfg.server.model = "qwen3.8".into();
        assert_eq!(configured_chat_model(&cfg).as_deref(), Some("qwen3.8"));
        cfg.server.model = "  ".into();
        assert!(configured_chat_model(&cfg).is_none());
        assert!(configured_chat_model(&Config::default()).is_none());
    }

    #[test]
    fn slot_probe_is_llamacpp_only() {
        assert!(should_probe_slots(EngineProfile::LlamaCpp));
        assert!(!should_probe_slots(EngineProfile::Generic));
        assert!(!should_probe_slots(EngineProfile::Auto));
        assert!(!should_probe_slots(EngineProfile::Vllm));
    }
}
