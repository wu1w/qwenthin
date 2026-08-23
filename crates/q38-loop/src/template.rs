//! Official Qwen chat templates via minijinja (same engine llama.cpp uses).
//!
//! HuggingFace extras: `raise_exception()`, pycompat methods (`.startswith`).
//! MiniJinja's `namespace()` does not take Jinja2 kwargs, so the two Qwen
//! `namespace(...)` call sites are expanded to `{% set ns.field = ... %}`.

use std::collections::BTreeMap;

use minijinja::value::Value;
use minijinja::{Environment, ErrorKind};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::family::Family;
use crate::media::MediaPart;
use crate::policy::TemplateKwargs;
use crate::vendor;

const LOW_SENTENCE: &str = "Reasoning effort is set to low. Keep your thinking brief and focused, moving directly to the conclusion without unnecessary elaboration.";
const XHIGH_SENTENCE: &str = "Reasoning effort is set to xhigh. Please think carefully through the task, validate key assumptions, consider plausible alternatives, and prioritize correctness, consistency, and clarity in the final answer.";

#[derive(Clone, Debug, Default, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Not a wire field. Folded into `content` parts by [`Self::to_api_value`].
    #[serde(skip)]
    pub parts: Vec<MediaPart>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            ..Self::default()
        }
    }

    /// Gate CONTINUE / parse-repair. Official Qwen Jinja ignores user turns
    /// whose trimmed content is a `<tool_response>` wrap when computing
    /// `last_query_index`, so current-turn `<think>` blocks stay in the prefix.
    pub fn hidden_user(content: impl AsRef<str>) -> Self {
        Self::user(wrap_tool_response(content.as_ref()))
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn assistant_tools(content: Option<String>, tool_calls: Vec<serde_json::Value>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_calls: Some(tool_calls),
            ..Self::default()
        }
    }

    pub fn assistant_reply(
        content: Option<String>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<serde_json::Value>>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content,
            reasoning_content: reasoning,
            tool_calls,
            ..Self::default()
        }
    }

    pub fn tool(call_id: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            tool_call_id: Some(call_id.into()),
            content: Some(body.into()),
            ..Self::default()
        }
    }

    pub fn tool_media(
        call_id: impl Into<String>,
        body: impl Into<String>,
        parts: Vec<MediaPart>,
    ) -> Self {
        let mut msg = Self::tool(call_id, body);
        msg.parts = parts;
        msg
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }

    /// OpenAI-compat message object. Media becomes a `content` array.
    pub fn to_api_value(&self) -> serde_json::Value {
        self.to_wire(false)
    }

    /// Official Qwen3.8 Jinja message object (video key + audio as text).
    pub fn to_jinja_value(&self) -> serde_json::Value {
        self.to_wire(true)
    }

    fn to_wire(&self, jinja: bool) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}));
        if self.parts.is_empty() {
            return v;
        }
        let mut arr = Vec::new();
        for p in &self.parts {
            arr.push(if jinja {
                p.to_jinja_value()
            } else {
                p.to_api_value()
            });
        }
        if let Some(t) = &self.content {
            if !t.is_empty() {
                arr.push(serde_json::json!({"type": "text", "text": t}));
            }
        }
        if let Some(obj) = v.as_object_mut() {
            obj.insert("content".into(), serde_json::Value::Array(arr));
        }
        v
    }
}

/// Official Qwen3.8 Jinja: `last_query_index` is the last user whose trimmed
/// content is **not** a `<tool_response>` wrap. CONTINUE / compact archive /
/// parse-repair must pass this so historical think after that user stays.
pub fn is_hidden_user_text(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("<tool_response>") && t.ends_with("</tool_response>")
}

/// Wrap harness-injected user text so Jinja does not treat it as a new query.
pub fn wrap_tool_response(text: &str) -> String {
    let t = text.trim();
    if is_hidden_user_text(t) {
        t.to_string()
    } else {
        format!("<tool_response>\n{t}\n</tool_response>")
    }
}

#[derive(Clone, Debug)]
pub struct RenderOpts<'a> {
    pub family: Family,
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [serde_json::Value]>,
    pub add_generation_prompt: bool,
    pub kwargs: TemplateKwargs,
}

#[derive(Clone, Debug)]
pub struct RenderedPrompt {
    pub text: String,
    pub family: Family,
}

impl RenderedPrompt {
    pub fn prefix_hash(&self) -> String {
        crate::vendor::sha256_hex(self.text.as_bytes())
    }
}

pub fn render(opts: &RenderOpts<'_>) -> Result<RenderedPrompt> {
    let family = match opts.family {
        Family::Auto => Family::Qwen38,
        other => other,
    };
    let src = load_template_source(family)?;
    let src = preprocess_qwen_jinja(&src);
    let env = hf_env();
    let tmpl = env
        .template_from_str(&src)
        .map_err(|e| Error::Template(e.to_string()))?;

    let mut ctx: BTreeMap<String, Value> = BTreeMap::new();
    let msgs: Vec<serde_json::Value> = opts
        .messages
        .iter()
        .map(ChatMessage::to_jinja_value)
        .collect();
    ctx.insert("messages".into(), Value::from_serialize(&msgs));
    ctx.insert(
        "add_generation_prompt".into(),
        Value::from(opts.add_generation_prompt),
    );
    if let Some(tools) = &opts.tools {
        if !tools.is_empty() {
            ctx.insert("tools".into(), Value::from_serialize(tools));
        }
    }
    if let Some(v) = opts.kwargs.enable_thinking {
        ctx.insert("enable_thinking".into(), Value::from(v));
    }
    if let Some(ref v) = opts.kwargs.reasoning_effort {
        ctx.insert("reasoning_effort".into(), Value::from(v.clone()));
    }
    if let Some(v) = opts.kwargs.preserve_thinking {
        ctx.insert("preserve_thinking".into(), Value::from(v));
    }

    let text = tmpl
        .render(ctx)
        .map_err(|e| Error::Template(format!("{e:#}")))?;
    Ok(RenderedPrompt { text, family })
}

pub fn load_template_source(family: Family) -> Result<String> {
    let path = vendor::chat_template_path(family);
    std::fs::read_to_string(&path)
        .map_err(|e| Error::Template(format!("read {}: {e}", path.display())))
}

/// MiniJinja's `namespace()` ignores Jinja2 kwargs. Expand the official Qwen call sites.
fn preprocess_qwen_jinja(src: &str) -> String {
    src.replace(
        "{%- set image_count = namespace(value=0) %}",
        "{%- set image_count = namespace() %}{%- set image_count.value = 0 %}",
    )
    .replace(
        "{%- set video_count = namespace(value=0) %}",
        "{%- set video_count = namespace() %}{%- set video_count.value = 0 %}",
    )
    .replace(
        "{%- set ns = namespace(multi_step_tool=true, last_query_index=messages|length - 1) %}",
        "{%- set ns = namespace() %}{%- set ns.multi_step_tool = true %}{%- set ns.last_query_index = messages|length - 1 %}",
    )
}

fn hf_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_keep_trailing_newline(true);
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    env.add_function("raise_exception", raise_exception);
    env
}

fn raise_exception(msg: String) -> std::result::Result<String, minijinja::Error> {
    Err(minijinja::Error::new(ErrorKind::InvalidOperation, msg))
}

pub fn low_effort_sentence() -> &'static str {
    LOW_SENTENCE
}

pub fn xhigh_effort_sentence() -> &'static str {
    XHIGH_SENTENCE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::EndpointCaps;
    use crate::media::MediaPart;
    use crate::policy::ThinkPolicy;
    use crate::tokenize::count_tokens;
    use crate::tools_schema::{agent_tools, memory_search_tool, view_tool, HARNESS_SYSTEM};

    fn kw(policy: &ThinkPolicy) -> TemplateKwargs {
        policy.template_kwargs(&EndpointCaps::qwen38_llamacpp())
    }

    fn qwen38(
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
        policy: ThinkPolicy,
    ) -> String {
        render(&RenderOpts {
            family: Family::Qwen38,
            messages,
            tools,
            add_generation_prompt: true,
            kwargs: kw(&policy),
        })
        .unwrap()
        .text
    }

    fn without_gen(s: &str) -> &str {
        match s.rfind("<|im_start|>assistant\n") {
            Some(i) => &s[..i],
            None => s,
        }
    }

    #[test]
    fn low_vs_xhigh_fork_at_effort_word() {
        let msgs = vec![ChatMessage::user("hi")];
        let low = qwen38(&msgs, None, ThinkPolicy::agent_default());
        let mut xhigh = ThinkPolicy::agent_default();
        xhigh.effort = Some(crate::policy::Effort::Xhigh);
        xhigh.max_think_tokens = 4096;
        let high = qwen38(&msgs, None, xhigh);

        assert!(low.contains(LOW_SENTENCE), "{low}");
        assert!(high.contains(XHIGH_SENTENCE), "{high}");
        let common = low
            .chars()
            .zip(high.chars())
            .take_while(|(a, b)| a == b)
            .count();
        // Shared: "<|im_start|>system\nReasoning effort is set to "
        assert!(
            common < 64,
            "effort must fork near the start, common={common}"
        );
        assert!(low.starts_with("<|im_start|>system\n"));
        assert_eq!(&low[..common], &high[..common]);
    }

    #[test]
    fn medium_does_not_inject_effort_sentence() {
        let mut mid = ThinkPolicy::agent_default();
        mid.effort = Some(crate::policy::Effort::Medium);
        let msgs = [ChatMessage::user("hi")];
        let text = qwen38(&msgs, None, mid);
        assert!(!text.contains("Reasoning effort is set"), "{text}");
        assert!(text.starts_with("<|im_start|>user\n"));
    }

    #[test]
    fn native_policy_has_no_effort_sentence() {
        let p = ThinkPolicy::native_with(&crate::policy::ThinkBudget::default());
        let text = qwen38(&[ChatMessage::user("hi")], None, p);
        assert!(!text.contains("Reasoning effort is set"), "{text}");
        assert!(text.starts_with("<|im_start|>user\n"), "{text}");
    }

    #[test]
    fn thinking_off_empty_think_block() {
        let msgs = [ChatMessage::user("hi")];
        let text = qwen38(&msgs, None, ThinkPolicy::off());
        assert!(text.contains("<think>\n\n</think>\n\n"), "{text}");
        assert!(!text.contains("Reasoning effort is set"), "{text}");
    }

    #[test]
    fn tools_rebuild_system_and_keep_client_system() {
        let msgs = vec![ChatMessage::system(HARNESS_SYSTEM), ChatMessage::user("hi")];
        let tools = agent_tools();
        let text = qwen38(&msgs, Some(&tools), ThinkPolicy::agent_default());
        assert!(text.contains("# Tools"));
        assert!(text.contains("<tools>"));
        assert!(text.contains(HARNESS_SYSTEM));
        assert!(text.contains("\"name\":\"read\""));
        assert!(text.contains(LOW_SENTENCE));
        // effort paragraph is before # Tools
        let effort_at = text.find(LOW_SENTENCE).unwrap();
        let tools_at = text.find("# Tools").unwrap();
        assert!(effort_at < tools_at);
    }

    #[test]
    fn agent_prefix_under_800_tokens() {
        let msgs = vec![ChatMessage::system(HARNESS_SYSTEM), ChatMessage::user("x")];
        let tools = agent_tools();
        let text = qwen38(&msgs, Some(&tools), ThinkPolicy::agent_default());
        let n = count_tokens(Family::Qwen38, &text).unwrap();
        assert!(
            n <= 800,
            "agent rendered prefix {n} tokens exceeds 800 gate:\n{text}"
        );
    }

    #[test]
    fn coding_prompt_prefix_under_800_tokens() {
        let system = crate::prompt::coding_prompt("/tmp/ws");
        let msgs = vec![ChatMessage::system(system), ChatMessage::user("x")];
        let tools = agent_tools();
        let text = qwen38(&msgs, Some(&tools), ThinkPolicy::agent_default());
        let n = count_tokens(Family::Qwen38, &text).unwrap();
        assert!(
            n <= 800,
            "coding prompt rendered prefix {n} tokens exceeds 800 gate:\n{text}"
        );
    }

    #[test]
    fn periphery_prefix_under_800_tokens() {
        let system = crate::prompt::coding_prompt("/tmp/ws");
        let mut tools = agent_tools();
        tools.push(crate::tools_schema::search_tool());
        tools.push(memory_search_tool());
        tools.push(view_tool());
        let msgs = vec![ChatMessage::system(system), ChatMessage::user("x")];
        let text = qwen38(&msgs, Some(&tools), ThinkPolicy::agent_default());
        let n = count_tokens(Family::Qwen38, &text).unwrap();
        assert!(
            n <= 800,
            "periphery rendered prefix {n} tokens exceeds 800 gate:\n{text}"
        );
        let frozen = qwen38(
            &[
                ChatMessage::system(crate::prompt::coding_prompt("/tmp/ws")),
                ChatMessage::user("x"),
            ],
            Some(&agent_tools()),
            ThinkPolicy::agent_default(),
        );
        let frozen_n = count_tokens(Family::Qwen38, &frozen).unwrap();
        assert!(frozen_n <= 800, "frozen four still {frozen_n}");
    }

    #[test]
    fn no_user_query_errors() {
        let msgs = vec![ChatMessage::system("only system")];
        let err = render(&RenderOpts {
            family: Family::Qwen38,
            messages: &msgs,
            tools: None,
            add_generation_prompt: true,
            kwargs: kw(&ThinkPolicy::agent_default()),
        })
        .unwrap_err();
        assert!(err.to_string().contains("No user query"), "{err}");
    }

    #[test]
    fn qwen36_template_parses_without_effort() {
        let msgs = vec![ChatMessage::user("hi")];
        let text = render(&RenderOpts {
            family: Family::Qwen36,
            messages: &msgs,
            tools: None,
            add_generation_prompt: true,
            kwargs: TemplateKwargs {
                enable_thinking: Some(true),
                reasoning_effort: None,
                preserve_thinking: Some(false),
            },
        })
        .unwrap()
        .text;
        assert!(text.contains("<|im_start|>user\n"));
        assert!(!text.contains("Reasoning effort is set"), "{text}");
    }

    #[test]
    fn hidden_continue_keeps_current_turn_think() {
        let policy = ThinkPolicy::agent_default();
        let mut asst = ChatMessage::assistant("working");
        asst.reasoning_content = Some("plan".into());
        let before = vec![ChatMessage::user("task"), asst.clone()];
        let mut hidden = before.clone();
        hidden.push(ChatMessage::hidden_user("Continue working on the task."));
        let mut plain = before.clone();
        plain.push(ChatMessage::user("Continue working on the task."));

        let before_text = qwen38(&before, None, policy.clone());
        let hidden_text = qwen38(&hidden, None, policy.clone());
        let plain_text = qwen38(&plain, None, policy);

        assert!(
            hidden_text.contains("<think>\nplan\n</think>"),
            "{hidden_text}"
        );
        assert!(
            !plain_text.contains("<think>\nplan\n</think>"),
            "{plain_text}"
        );
        let prefix = without_gen(&before_text);
        assert!(
            hidden_text.starts_with(prefix),
            "CONTINUE wrap must extend the suffix only"
        );
        assert!(
            !plain_text.starts_with(prefix),
            "a real user CONTINUE must rewrite earlier assistants"
        );
    }

    #[test]
    fn thinking_off_keeps_empty_wrapper_on_historical() {
        let asst = ChatMessage::assistant("ok");
        let t2 = vec![ChatMessage::user("ping"), asst, ChatMessage::user("pong")];
        let text = qwen38(&t2, None, ThinkPolicy::off());
        assert!(
            text.contains("<|im_start|>assistant\n<think>\n\n</think>\n\nok"),
            "{text}"
        );
        let t1 = qwen38(&[ChatMessage::user("ping")], None, ThinkPolicy::off());
        assert!(t1.contains("<think>\n\n</think>\n\n"), "{t1}");
    }

    #[test]
    fn thinking_on_new_user_drops_historical_think() {
        let mut asst = ChatMessage::assistant("ok");
        asst.reasoning_content = Some("secret-plan".into());
        let t2 = vec![ChatMessage::user("ping"), asst, ChatMessage::user("pong")];
        let stripped = qwen38(&t2, None, ThinkPolicy::agent_default());
        assert!(!stripped.contains("secret-plan"), "{stripped}");
        let kept = qwen38(&t2, None, ThinkPolicy::think_mode());
        assert!(kept.contains("secret-plan"), "{kept}");
    }

    #[test]
    fn hidden_archive_keeps_current_turn_think() {
        let policy = ThinkPolicy::agent_default();
        let mut asst = ChatMessage::assistant("working");
        asst.reasoning_content = Some("plan-zirconium".into());
        let live = vec![
            ChatMessage::hidden_user("[archived]\nseq 1 user old task"),
            ChatMessage::user("task"),
            asst.clone(),
        ];
        let text = qwen38(&live, None, policy);
        assert!(text.contains("<think>\nplan-zirconium\n</think>"), "{text}");
        assert!(text.contains("<tool_response>"));
        assert!(text.contains("<|im_start|>user\ntask"));
    }

    #[test]
    fn tool_call_object_args_render_parameters() {
        let calls = vec![serde_json::json!({
            "id": "c1",
            "type": "function",
            "function": {
                "name": "read",
                "arguments": {"path": "a.rs"}
            }
        })];
        let msgs = vec![
            ChatMessage::user("read it"),
            ChatMessage::assistant_tools(None, calls),
            ChatMessage::tool("c1", "ok"),
        ];
        let tools = agent_tools();
        let text = qwen38(&msgs, Some(&tools), ThinkPolicy::agent_default());
        assert!(text.contains("<parameter=path>"), "{text}");
        assert!(text.contains("a.rs"), "{text}");
        assert!(
            text.contains("<tool_response>\nok\n</tool_response>"),
            "{text}"
        );
    }

    #[test]
    fn tool_step_extends_suffix_only() {
        let policy = ThinkPolicy::agent_default();
        let tools = agent_tools();
        let base = vec![
            ChatMessage::system(HARNESS_SYSTEM),
            ChatMessage::user("read a.rs"),
        ];
        let mut after = base.clone();
        after.push(ChatMessage::assistant_tools(
            None,
            vec![serde_json::json!({
                "id": "c1",
                "type": "function",
                "function": { "name": "read", "arguments": {"path": "a.rs"} }
            })],
        ));
        after.push(ChatMessage::tool("c1", "fn main() {}"));
        let a = qwen38(&base, Some(&tools), policy.clone());
        let b = qwen38(&after, Some(&tools), policy);
        assert!(
            b.starts_with(without_gen(&a)),
            "tool results must not rewrite the frozen prefix"
        );
    }

    #[test]
    fn tool_image_renders_vision_tokens_inside_tool_response() {
        let mut img = ChatMessage::tool("c1", "Image loaded: red.png");
        img.parts = vec![MediaPart::image_url("data:image/png;base64,xx")];
        let msgs = vec![ChatMessage::user("what color"), img];
        let text = qwen38(&msgs, None, ThinkPolicy::agent_default());
        assert!(
            text.contains("<|vision_start|><|image_pad|><|vision_end|>"),
            "{text}"
        );
        assert!(text.contains("<tool_response>"), "{text}");
        assert!(text.contains("Image loaded: red.png"), "{text}");
        assert!(
            text.contains("<|im_start|>user\nwhat color"),
            "real user must remain the query: {text}"
        );
    }

    #[test]
    fn tool_video_renders_video_pad() {
        let mut vid = ChatMessage::tool("c1", "Video loaded: clip.mp4");
        vid.parts = vec![MediaPart::video_url("https://example.com/c.mp4")];
        let msgs = vec![ChatMessage::user("watch"), vid];
        let text = qwen38(&msgs, None, ThinkPolicy::agent_default());
        assert!(
            text.contains("<|vision_start|><|video_pad|><|vision_end|>"),
            "{text}"
        );
    }

    #[test]
    fn tool_audio_does_not_raise_in_jinja() {
        let mut aud = ChatMessage::tool("c1", "Audio loaded: a.wav");
        aud.parts = vec![MediaPart::audio_url(
            "data:audio/wav;base64,AA==",
            "audio/wav",
        )];
        let msgs = vec![ChatMessage::user("listen"), aud];
        let text = qwen38(&msgs, None, ThinkPolicy::agent_default());
        assert!(text.contains("[audio]"), "{text}");
        assert!(text.contains("Audio loaded: a.wav"), "{text}");
    }

    #[test]
    fn system_rejects_images() {
        let mut sys = ChatMessage::system("sys");
        sys.parts = vec![MediaPart::image_url("data:image/png;base64,xx")];
        let err = render(&RenderOpts {
            family: Family::Qwen38,
            messages: &[sys, ChatMessage::user("hi")],
            tools: None,
            add_generation_prompt: true,
            kwargs: kw(&ThinkPolicy::agent_default()),
        })
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("System message cannot contain images"),
            "{err}"
        );
    }
}
