//! Prefix-cache accounting that does not assume llama.cpp slots.
//!
//! llama.cpp prompt cache, vLLM automatic prefix caching, and SGLang radix
//! all match **token ids** of the rendered prompt. A new user turn with
//! `preserve_thinking=false` drops historical `<think>` blocks, so the
//! longest reusable prefix ends at the first historical assistant.
//!
//! Cached counts are block-aligned on vLLM (16/32). Comparisons use slack
//! instead of a hard-coded token length.

use crate::family::{EndpointCaps, Family};
use crate::policy::{TemplateKwargs, ThinkPolicy};
use crate::template::{render, ChatMessage, RenderOpts};
use crate::tokenize::count_tokens;

/// Typical vLLM APC block size; llama.cpp is token-exact so this is only slack.
pub const BLOCK_SLACK: u64 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossTurnClass {
    /// Reused the completed previous turn (wrapper/think survived re-render).
    Warm,
    /// Reused only the prefix before the first assistant (think/wrapper dropped).
    Stripped,
    /// Essentially no reuse.
    Cold,
    /// Some reuse, not the two signatures above.
    Partial,
}

pub fn slack(expected: u64) -> u64 {
    BLOCK_SLACK.max(expected / 20)
}

pub fn near(cached: u64, expected: u64) -> bool {
    cached.abs_diff(expected) <= slack(expected.max(1))
}

/// First hop reused a short stable prefix while the prompt kept growing.
pub fn stuck_at_short_prefix(cached: u64, prompt: u64) -> bool {
    cached > 0 && prompt >= cached.saturating_mul(2).saturating_add(64)
}

pub fn clustered(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let med = v[v.len() / 2];
    if v.iter().all(|x| near(*x, med)) {
        Some(med)
    } else {
        None
    }
}

pub fn classify(cached: u64, prompt: u64, hist: u64, stripped: u64) -> CrossTurnClass {
    if prompt == 0 {
        return CrossTurnClass::Partial;
    }
    if cached < 8 {
        return CrossTurnClass::Cold;
    }
    let warm_floor = hist.min(prompt);
    if (warm_floor > 0 && near(cached, warm_floor)) || cached.saturating_mul(100) / prompt >= 85 {
        return CrossTurnClass::Warm;
    }
    let looks_stripped = stripped > 0 && near(cached, stripped);
    let tail_is_big = prompt > cached.saturating_add(slack(cached)).saturating_add(16);
    if looks_stripped && tail_is_big {
        return CrossTurnClass::Stripped;
    }
    if stuck_at_short_prefix(cached, prompt) {
        return CrossTurnClass::Stripped;
    }
    CrossTurnClass::Partial
}

pub fn hit_pct(cached: u64, prompt: u64) -> Option<f64> {
    if prompt == 0 {
        None
    } else {
        Some(((cached as f64 / prompt as f64) * 1000.0).round() / 10.0)
    }
}

pub fn system_tools_span(text: &str) -> &str {
    match text.find("<|im_start|>user") {
        Some(i) => &text[..i],
        None => text,
    }
}

pub fn pre_assistant_span(text: &str) -> &str {
    match text.find("<|im_start|>assistant") {
        Some(i) => &text[..i],
        None => text,
    }
}

pub fn token_len(family: Family, text: &str) -> Option<u32> {
    count_tokens(family, text).ok()
}

pub fn render_text(
    family: Family,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    kwargs: TemplateKwargs,
    add_generation_prompt: bool,
) -> Option<String> {
    render(&RenderOpts {
        family,
        messages,
        tools,
        add_generation_prompt,
        kwargs,
    })
    .ok()
    .map(|r| r.text)
}

/// Tokens of the completed previous turn (no new user, no generation suffix).
pub fn hist_tokens(
    family: Family,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    kwargs: TemplateKwargs,
) -> Option<u64> {
    let text = render_text(family, messages, tools, kwargs, false)?;
    token_len(family, &text).map(u64::from)
}

/// Tokens before the first assistant — longest prefix that survives think-strip.
pub fn stripped_tokens(
    family: Family,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    kwargs: TemplateKwargs,
) -> Option<u64> {
    let text = render_text(family, messages, tools, kwargs, true)?;
    token_len(family, pre_assistant_span(&text)).map(u64::from)
}

pub fn frozen_system_tools_tokens(
    family: Family,
    system: &str,
    tools: &[serde_json::Value],
    policy: &ThinkPolicy,
) -> Option<u64> {
    let caps = EndpointCaps::for_family(family, crate::family::EngineProfile::Generic);
    let kwargs = policy.template_kwargs(&caps);
    let msgs = [ChatMessage::system(system), ChatMessage::user("x")];
    let tools = if tools.is_empty() { None } else { Some(tools) };
    let text = render_text(family, &msgs, tools, kwargs, false)?;
    token_len(family, system_tools_span(&text)).map(u64::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::EndpointCaps;
    use crate::policy::ThinkPolicy;
    use crate::template::ChatMessage;
    use crate::tools_schema::{agent_tools, HARNESS_SYSTEM};

    #[test]
    fn slack_covers_vllm_blocks_not_aiga_643() {
        assert_eq!(slack(640), 32);
        assert_eq!(slack(2000), 100);
        assert!(near(640, 643));
        assert!(!near(640, 900));
    }

    #[test]
    fn classify_warm_high_ratio() {
        assert_eq!(classify(760, 820, 760, 200), CrossTurnClass::Warm);
        assert_eq!(classify(0, 670, 600, 200), CrossTurnClass::Cold);
    }

    #[test]
    fn classify_stripped_without_hardcoded_prefix() {
        assert_eq!(classify(643, 4000, 3800, 640), CrossTurnClass::Stripped);
        assert_eq!(classify(15, 80, 70, 15), CrossTurnClass::Stripped);
        assert_eq!(classify(2000, 2200, 2100, 640), CrossTurnClass::Warm);
    }

    #[test]
    fn cluster_median() {
        assert_eq!(clustered(&[640, 643, 641]), Some(641));
        assert_eq!(clustered(&[640, 2000]), None);
    }

    #[test]
    fn rendered_strip_is_before_first_assistant() {
        let caps = EndpointCaps::qwen38_llamacpp();
        let mut asst = ChatMessage::assistant("ok");
        asst.reasoning_content = Some("secret-plan".into());
        let msgs = vec![
            ChatMessage::system(HARNESS_SYSTEM),
            ChatMessage::user("ping"),
            asst,
            ChatMessage::user("pong"),
        ];
        let tools = agent_tools();
        let on = ThinkPolicy::agent_default().template_kwargs(&caps);
        let off = ThinkPolicy::off().template_kwargs(&caps);
        let hist = &msgs[..3];
        let stripped_on = stripped_tokens(Family::Qwen38, &msgs, Some(&tools), on.clone()).unwrap();
        let hist_on = hist_tokens(Family::Qwen38, hist, Some(&tools), on).unwrap();
        let stripped_off =
            stripped_tokens(Family::Qwen38, &msgs, Some(&tools), off.clone()).unwrap();
        let hist_off = hist_tokens(Family::Qwen38, hist, Some(&tools), off).unwrap();
        assert!(
            stripped_on < hist_on,
            "think-strip prefix {stripped_on} should be shorter than preserved hist {hist_on}"
        );
        assert!(
            hist_off > stripped_off,
            "thinking-off preserve hist {hist_off} vs stripped {stripped_off}"
        );
        let frozen = frozen_system_tools_tokens(
            Family::Qwen38,
            HARNESS_SYSTEM,
            &tools,
            &ThinkPolicy::agent_default(),
        )
        .unwrap();
        assert!(
            near(stripped_on, frozen) || stripped_on > frozen,
            "stripped {stripped_on} frozen {frozen}"
        );
        assert!(frozen > 32, "frozen system+tools was {frozen}");
    }
}
