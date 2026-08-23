//! Qwen XML tool-call surface (`<tool_call><function=…>`), plus JSON inside
//! `<tool_call>`.
//!
//! Matches QwenPaw `tag_parser.py`: JSON first, then strict XML, then lenient
//! XML (missing `</parameter>` / `</function>`). This extractor is think-unaware;
//! `parse_turn` recovers complete blocks from the thinking channel when there
//! are no native OpenAI `tool_calls`.

use std::ops::Range;

use serde_json::{json, Map, Value};

use crate::tool_calls::ToolCall;

const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";
const FN_OPEN: &str = "<function=";
const FN_CLOSE: &str = "</function>";
const PARAM_OPEN: &str = "<parameter=";
const PARAM_CLOSE: &str = "</parameter>";

#[derive(Debug, Default)]
pub(super) struct XmlExtract {
    pub calls: Vec<ToolCall>,
    /// A `<tool_call>` was opened but never closed.
    pub unclosed: bool,
    /// A complete block could not be parsed as XML functions or JSON.
    pub malformed: bool,
    pub ranges: Vec<Range<usize>>,
}

impl XmlExtract {
    pub fn had_tag(&self) -> bool {
        !self.ranges.is_empty()
    }

    /// Incomplete XML from a cancelled stream is not a parse fail.
    pub fn parse_fail(&self, truncated: bool) -> bool {
        if truncated {
            return false;
        }
        self.unclosed || self.malformed
    }
}

pub(super) fn extract_xml_tools(content: &str) -> XmlExtract {
    let mut out = XmlExtract::default();
    let mut search = 0usize;
    while let Some(rel) = content[search..].find(TOOL_OPEN) {
        let start = search + rel;
        if in_markdown_code(content, start) {
            search = start + TOOL_OPEN.len();
            continue;
        }
        let inner_at = start + TOOL_OPEN.len();
        match content[inner_at..].find(TOOL_CLOSE) {
            Some(rel_end) => {
                let inner_end = inner_at + rel_end;
                let end = inner_end + TOOL_CLOSE.len();
                out.ranges.push(start..end);
                match parse_tool_inner(content[inner_at..inner_end].trim()) {
                    Ok(mut calls) => out.calls.append(&mut calls),
                    Err(()) => out.malformed = true,
                }
                search = end;
            }
            None => {
                out.unclosed = true;
                out.ranges.push(start..content.len());
                break;
            }
        }
    }
    out
}

/// Citations like `` `<tool_call>` `` in an analysis must not eat the rest of
/// the reply. Real Qwen XML is not wrapped in markdown code.
fn in_markdown_code(s: &str, byte_idx: usize) -> bool {
    let prefix = s.get(..byte_idx).unwrap_or("");
    let b = prefix.as_bytes();
    let mut i = 0;
    let mut fence = false;
    let mut inline = false;
    while i < b.len() {
        if !inline && i + 2 < b.len() && b[i] == b'`' && b[i + 1] == b'`' && b[i + 2] == b'`' {
            fence = !fence;
            i += 3;
            continue;
        }
        if !fence && b[i] == b'`' {
            inline = !inline;
        }
        i += 1;
    }
    fence || inline
}

/// QwenPaw keeps only `text_before` the first `<tool_call>` (official template:
/// optional natural language before a call, none after).
pub(super) fn text_before_first(content: &str, ranges: &[Range<usize>]) -> String {
    match ranges.first() {
        Some(r) if r.start <= content.len() => content[..r.start].trim().to_string(),
        _ => content.trim().to_string(),
    }
}

fn parse_tool_inner(inner: &str) -> Result<Vec<ToolCall>, ()> {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return Err(());
    }
    if let Ok(calls) = parse_json_tools(trimmed) {
        return Ok(calls);
    }
    if trimmed.contains(FN_OPEN) {
        return parse_function_tools(trimmed);
    }
    Err(())
}

fn parse_json_tools(raw: &str) -> Result<Vec<ToolCall>, ()> {
    let v: Value = serde_json::from_str(raw).map_err(|_| ())?;
    match v {
        Value::Array(items) => {
            let mut calls = Vec::with_capacity(items.len());
            for item in items {
                calls.push(json_to_call(&item)?);
            }
            if calls.is_empty() {
                Err(())
            } else {
                Ok(calls)
            }
        }
        obj => Ok(vec![json_to_call(&obj)?]),
    }
}

fn json_to_call(v: &Value) -> Result<ToolCall, ()> {
    let name = v
        .get("name")
        .or_else(|| v.pointer("/function/name"))
        .and_then(Value::as_str)
        .ok_or(())?
        .to_string();
    if name.is_empty() {
        return Err(());
    }
    let arguments = match v
        .get("arguments")
        .or_else(|| v.pointer("/function/arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}))
    {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(fresh_id);
    Ok(ToolCall {
        id,
        name,
        arguments,
    })
}

fn parse_function_tools(inner: &str) -> Result<Vec<ToolCall>, ()> {
    parse_function_tools_strict(inner).or_else(|_| parse_function_tools_lenient(inner))
}

fn parse_function_tools_strict(inner: &str) -> Result<Vec<ToolCall>, ()> {
    let mut calls = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = inner[search..].find(FN_OPEN) {
        let start = search + rel;
        let name_at = start + FN_OPEN.len();
        let gt = inner[name_at..].find('>').ok_or(())?;
        let name = inner[name_at..name_at + gt].trim();
        if name.is_empty() || name.contains('<') {
            return Err(());
        }
        let body_at = name_at + gt + 1;
        let close = inner[body_at..].find(FN_CLOSE).ok_or(())?;
        let body = &inner[body_at..body_at + close];
        let arguments = parse_parameters_strict(body)?;
        calls.push(ToolCall {
            id: fresh_id(),
            name: name.to_string(),
            arguments,
        });
        search = body_at + close + FN_CLOSE.len();
    }
    if calls.is_empty() {
        Err(())
    } else {
        Ok(calls)
    }
}

fn parse_function_tools_lenient(inner: &str) -> Result<Vec<ToolCall>, ()> {
    let mut calls = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = inner[search..].find(FN_OPEN) {
        let start = search + rel;
        let name_at = start + FN_OPEN.len();
        let Some(gt) = inner[name_at..].find('>') else {
            break;
        };
        let name = inner[name_at..name_at + gt].trim();
        if name.is_empty() || name.contains('<') {
            return Err(());
        }
        let body_at = name_at + gt + 1;
        let rest = &inner[body_at..];
        let body_end = [rest.find(FN_OPEN), rest.find(FN_CLOSE)]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
        let body = &rest[..body_end];
        let arguments = parse_parameters_lenient(body);
        let empty = arguments.as_object().map(|m| m.is_empty()).unwrap_or(true);
        search = body_at + body_end;
        if rest
            .get(body_end..)
            .is_some_and(|s| s.starts_with(FN_CLOSE))
        {
            search += FN_CLOSE.len();
        }
        if empty {
            continue;
        }
        calls.push(ToolCall {
            id: fresh_id(),
            name: name.to_string(),
            arguments,
        });
    }
    if calls.is_empty() {
        Err(())
    } else {
        Ok(calls)
    }
}

fn parse_parameters_strict(body: &str) -> Result<Value, ()> {
    let mut map = Map::new();
    let mut search = 0usize;
    while let Some(rel) = body[search..].find(PARAM_OPEN) {
        let start = search + rel;
        let name_at = start + PARAM_OPEN.len();
        let gt = body[name_at..].find('>').ok_or(())?;
        let key = body[name_at..name_at + gt].trim();
        if key.is_empty() {
            return Err(());
        }
        let val_at = name_at + gt + 1;
        let close = body[val_at..].find(PARAM_CLOSE).ok_or(())?;
        let raw = body[val_at..val_at + close].trim();
        map.insert(key.to_string(), coerce_param(raw));
        search = val_at + close + PARAM_CLOSE.len();
    }
    Ok(Value::Object(map))
}

fn parse_parameters_lenient(body: &str) -> Value {
    let mut map = Map::new();
    let mut search = 0usize;
    while let Some(rel) = body[search..].find(PARAM_OPEN) {
        let start = search + rel;
        let name_at = start + PARAM_OPEN.len();
        let Some(gt) = body[name_at..].find('>') else {
            break;
        };
        let key = body[name_at..name_at + gt].trim();
        if key.is_empty() {
            break;
        }
        let val_at = name_at + gt + 1;
        let rest = &body[val_at..];
        let end = [
            rest.find(PARAM_OPEN),
            rest.find(PARAM_CLOSE),
            rest.find(FN_OPEN),
            rest.find(FN_CLOSE),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
        let raw = rest[..end].trim();
        map.insert(key.to_string(), coerce_param(raw));
        search = val_at + end;
        if rest.get(end..).is_some_and(|s| s.starts_with(PARAM_CLOSE)) {
            search += PARAM_CLOSE.len();
        }
    }
    Value::Object(map)
}

fn coerce_param(raw: &str) -> Value {
    if (raw.starts_with('{') && raw.ends_with('}')) || (raw.starts_with('[') && raw.ends_with(']'))
    {
        if let Ok(v) = serde_json::from_str(raw) {
            return v;
        }
    }
    if raw == "true" {
        return Value::Bool(true);
    }
    if raw == "false" {
        return Value::Bool(false);
    }
    if let Ok(n) = raw.parse::<i64>() {
        return json!(n);
    }
    if let Ok(n) = raw.parse::<f64>() {
        return json!(n);
    }
    Value::String(raw.to_string())
}

fn fresh_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_xml_function() {
        let content = "\
<tool_call>
<function=read>
<parameter=path>
note.txt
</parameter>
</function>
</tool_call>";
        let x = extract_xml_tools(content);
        assert!(!x.parse_fail(false));
        assert_eq!(x.calls.len(), 1);
        assert_eq!(x.calls[0].name, "read");
        assert_eq!(x.calls[0].arguments["path"], "note.txt");
        assert!(text_before_first(content, &x.ranges).is_empty());
    }

    #[test]
    fn json_inside_tool_call() {
        let content = r#"<tool_call>
{"name":"read","arguments":{"path":"note.txt"}}
</tool_call>"#;
        let x = extract_xml_tools(content);
        assert!(!x.parse_fail(false));
        assert_eq!(x.calls.len(), 1);
        assert_eq!(x.calls[0].name, "read");
        assert_eq!(x.calls[0].arguments["path"], "note.txt");
    }

    #[test]
    fn json_with_id_preserved() {
        let content =
            r#"<tool_call>{"id":"c9","name":"bash","arguments":{"command":"ls"}}</tool_call>"#;
        let x = extract_xml_tools(content);
        assert_eq!(x.calls[0].id, "c9");
        assert_eq!(x.calls[0].name, "bash");
    }

    #[test]
    fn malformed_complete_block() {
        let content = "<tool_call>this is not a tool</tool_call>";
        let x = extract_xml_tools(content);
        assert!(x.malformed);
        assert!(x.parse_fail(false));
        assert!(x.calls.is_empty());
        assert!(!x.parse_fail(true));
    }

    #[test]
    fn unclosed_is_parse_fail_unless_truncated() {
        let content = "<tool_call>\n<function=read>\n<parameter=path>\nnote";
        let x = extract_xml_tools(content);
        assert!(x.unclosed);
        assert!(x.parse_fail(false));
        assert!(!x.parse_fail(true));
        assert!(x.calls.is_empty());
    }

    #[test]
    fn two_blocks() {
        let content = "\
<tool_call>
<function=read>
<parameter=path>
a.rs
</parameter>
</function>
</tool_call>
<tool_call>
<function=read>
<parameter=path>
b.rs
</parameter>
</function>
</tool_call>";
        let x = extract_xml_tools(content);
        assert_eq!(x.calls.len(), 2);
        assert_eq!(x.calls[0].arguments["path"], "a.rs");
        assert_eq!(x.calls[1].arguments["path"], "b.rs");
        assert!(text_before_first(content, &x.ranges).is_empty());
    }

    #[test]
    fn extractor_is_think_unaware() {
        let content = "\
<think>
I might call read
<tool_call>
{\"name\":\"read\",\"arguments\":{\"path\":\"secret.rs\"}}
</tool_call>
</think>
I'll read it first
<tool_call>
{\"name\":\"read\",\"arguments\":{\"path\":\"note.txt\"}}
</tool_call>";
        let x = extract_xml_tools(content);
        assert_eq!(x.calls.len(), 2);
        assert_eq!(x.calls[0].arguments["path"], "secret.rs");
        assert_eq!(x.calls[1].arguments["path"], "note.txt");
    }

    #[test]
    fn complete_tool_call_without_think_close() {
        let content =
            "<think>\nplan\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}</tool_call>";
        let x = extract_xml_tools(content);
        assert_eq!(x.calls.len(), 1);
        assert_eq!(x.calls[0].arguments["path"], "a.rs");
        assert!(!x.malformed);
        assert!(!x.unclosed);
    }

    #[test]
    fn lenient_xml_missing_closing_tags() {
        let content = "\
<tool_call>
<function=read>
<parameter=path>
note.txt
</tool_call>";
        let x = extract_xml_tools(content);
        assert!(!x.parse_fail(false));
        assert_eq!(x.calls.len(), 1);
        assert_eq!(x.calls[0].name, "read");
        assert_eq!(x.calls[0].arguments["path"], "note.txt");
    }

    #[test]
    fn lenient_xml_several_parameters() {
        let content = "\
<tool_call>
<function=edit>
<parameter=path>
a.rs
<parameter=old_string>
x
<parameter=new_string>
y
</tool_call>";
        let x = extract_xml_tools(content);
        assert_eq!(x.calls.len(), 1);
        assert_eq!(x.calls[0].name, "edit");
        assert_eq!(x.calls[0].arguments["path"], "a.rs");
        assert_eq!(x.calls[0].arguments["old_string"], "x");
        assert_eq!(x.calls[0].arguments["new_string"], "y");
    }

    #[test]
    fn cited_tag_in_backticks_is_not_a_call() {
        let content = "xml_tools.rs 解析 `<tool_call>`。然后继续把适配度写完整，直到句号。";
        let x = extract_xml_tools(content);
        assert!(x.calls.is_empty());
        assert!(!x.had_tag());
        assert!(!x.unclosed);
        assert_eq!(text_before_first(content, &x.ranges), content);
    }

    #[test]
    fn real_xml_after_prose_still_extracts() {
        let content = "先读文件。\n<tool_call>\n{\"name\":\"read\",\"arguments\":{\"path\":\"a.rs\"}}\n</tool_call>";
        let x = extract_xml_tools(content);
        assert_eq!(x.calls.len(), 1);
        assert_eq!(x.calls[0].name, "read");
        assert_eq!(text_before_first(content, &x.ranges), "先读文件。");
    }
}
