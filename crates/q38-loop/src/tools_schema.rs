//! Frozen OpenAI tool JSON. Byte-stable: parsed with `preserve_order` and never rebuilt by hash maps.
//!
//! Descriptions name the tool. No working-style lectures — Jinja already owns
//! the call format.

use serde_json::Value;

/// Dummy system blob for template tests (not injected by the agent loop).
pub const HARNESS_SYSTEM: &str = "工作区助手。路径相对。";

const READ: &str = r#"{"type":"function","function":{"name":"read","description":"Read a file.","parameters":{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}}}"#;
const WRITE: &str = r#"{"type":"function","function":{"name":"write","description":"Write a file.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}"#;
const EDIT: &str = r#"{"type":"function","function":{"name":"edit","description":"Replace text in a file.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"}},"required":["path","old_string","new_string"]}}}"#;
const BASH: &str = r#"{"type":"function","function":{"name":"bash","description":"Run a shell command.","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}"#;
const RUN_CODE: &str = r#"{"type":"function","function":{"name":"run_code","description":"Run Python.","parameters":{"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}}}"#;
const RECALL: &str = r#"{"type":"function","function":{"name":"recall","description":"Search session archive.","parameters":{"type":"object","properties":{"query":{"type":"string"},"seq":{"type":"integer"},"blob":{"type":"string"}}}}}"#;
const MEMORY_SEARCH: &str = r#"{"type":"function","function":{"name":"memory_search","description":"Search notes.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}"#;
const SKILL: &str = r#"{"type":"function","function":{"name":"skill","description":"Load a skill by name.","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}"#;
const MCP: &str = r#"{"type":"function","function":{"name":"mcp","description":"Call an MCP server.","parameters":{"type":"object","properties":{"server":{"type":"string"},"method":{"type":"string"},"args":{"type":"object"}},"required":["method"]}}}"#;
const VIEW: &str = r#"{"type":"function","function":{"name":"view","description":"Load image, video stills, or audio.","parameters":{"type":"object","properties":{"path":{"type":"string"},"kind":{"type":"string","enum":["image","audio","video"]}},"required":["path"]}}}"#;
const SEARCH: &str = r#"{"type":"function","function":{"name":"search","description":"Find code.","parameters":{"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string"}},"required":["query"]}}}"#;
const WEB: &str = r#"{"type":"function","function":{"name":"web","description":"Web search (query) or fetch a page (url).","parameters":{"type":"object","properties":{"query":{"type":"string"},"url":{"type":"string"}}}}}"#;

fn parse(s: &'static str) -> Value {
    serde_json::from_str(s).expect("frozen tool JSON")
}

pub fn agent_tools() -> Vec<Value> {
    vec![parse(READ), parse(WRITE), parse(EDIT), parse(BASH)]
}

pub fn agent_tool_names() -> [&'static str; 4] {
    ["read", "write", "edit", "bash"]
}

pub fn code_tools() -> Vec<Value> {
    vec![parse(RUN_CODE), parse(READ), parse(BASH)]
}

pub fn code_tool_names() -> [&'static str; 3] {
    ["run_code", "read", "bash"]
}

/// Separate blob. Do not splice into [`agent_tools`] — that would change the
/// frozen four-tool JSON. Append only after compact (prefix already misses).
pub fn recall_tool() -> Value {
    parse(RECALL)
}

pub fn memory_search_tool() -> Value {
    parse(MEMORY_SEARCH)
}

pub fn skill_tool() -> Value {
    parse(SKILL)
}

pub fn mcp_tool() -> Value {
    parse(MCP)
}

pub fn view_tool() -> Value {
    parse(VIEW)
}

/// Append after [`agent_tools`] — do not splice into the frozen four-byte JSON.
pub fn search_tool() -> Value {
    parse(SEARCH)
}

/// Appended at session start when `[web]` is enabled (builtin engines need no
/// key, so this is on out of the box). Same freeze discipline as `mcp`.
pub fn web_tool() -> Value {
    parse(WEB)
}

pub fn has_tool(tools: &[Value], name: &str) -> bool {
    tools
        .iter()
        .any(|t| t["function"]["name"].as_str() == Some(name))
}

pub fn has_recall(tools: &[Value]) -> bool {
    has_tool(tools, "recall")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_order_and_names() {
        let tools = agent_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, agent_tool_names());
        assert_eq!(serde_json::to_string(&tools[0]).unwrap(), READ);
    }

    #[test]
    fn code_tools_order_and_frozen() {
        let tools = code_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, code_tool_names());
        assert_eq!(names.join(", "), "run_code, read, bash");
        assert_eq!(serde_json::to_string(&code_tools()[0]).unwrap(), RUN_CODE);
    }

    #[test]
    fn recall_is_not_in_frozen_agent_tools() {
        let tools = agent_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"recall"));
        assert_eq!(serde_json::to_string(&recall_tool()).unwrap(), RECALL);
        assert!(!has_recall(&tools));
        assert!(has_recall(&[recall_tool()]));
        assert_eq!(
            serde_json::to_string(&memory_search_tool()).unwrap(),
            MEMORY_SEARCH
        );
        assert_eq!(serde_json::to_string(&skill_tool()).unwrap(), SKILL);
        assert_eq!(serde_json::to_string(&mcp_tool()).unwrap(), MCP);
        assert_eq!(serde_json::to_string(&view_tool()).unwrap(), VIEW);
        assert_eq!(serde_json::to_string(&search_tool()).unwrap(), SEARCH);
        assert_eq!(serde_json::to_string(&web_tool()).unwrap(), WEB);
        assert!(!has_tool(&tools, "memory_search"));
        assert!(!has_tool(&tools, "skill"));
        assert!(!has_tool(&tools, "mcp"));
        assert!(!has_tool(&tools, "view"));
        assert!(!has_tool(&tools, "search"));
        assert!(!has_tool(&tools, "web"));
    }
}
