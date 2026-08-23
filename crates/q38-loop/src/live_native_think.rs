//! Live check: does Qwen3.8 pick think depth from the task when Jinja
//! does not inject a low/xhigh lecture? Official template: `medium` has no
//! effort sentence; omit/`xhigh` injects xhigh; `low` injects the brief sentence.

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;

    use crate::agent::{Completer, HttpCompleter};
    use crate::config::Config;
    use crate::family::{EndpointCaps, Family};
    use crate::policy::{Effort, ThinkPolicy};
    use crate::template::{render, ChatMessage, RenderOpts};
    use crate::tokenize::count_tokens;

    fn live_cfg() -> Config {
        let (mut cfg, _) = Config::load_or_init().unwrap();
        cfg.apply_env();
        cfg
    }

    fn toks(s: &str) -> u32 {
        count_tokens(Family::Qwen38, s).unwrap_or(0)
    }

    fn jinja_sentence(policy: &ThinkPolicy) -> (&'static str, bool, bool) {
        const LOW: &str = "Reasoning effort is set to low.";
        const XHIGH: &str = "Reasoning effort is set to xhigh.";
        let text = render(&RenderOpts {
            family: Family::Qwen38,
            messages: &[ChatMessage::user("x")],
            tools: None,
            add_generation_prompt: true,
            kwargs: policy.template_kwargs(&EndpointCaps::qwen38_llamacpp()),
        })
        .unwrap()
        .text;
        let has_low = text.contains(LOW);
        let has_xhigh = text.contains(XHIGH);
        let label = if has_low {
            "low-sentence"
        } else if has_xhigh {
            "xhigh-sentence"
        } else if policy.enabled {
            "no-sentence"
        } else {
            "thinking-off"
        };
        (label, has_low, has_xhigh)
    }

    async fn once(cfg: &Config, policy: ThinkPolicy, prompt: &str) -> serde_json::Value {
        let (sentence, has_low, has_xhigh) = jinja_sentence(&policy);
        let completer = HttpCompleter::connect(cfg, policy.clone()).await.unwrap();
        let t0 = Instant::now();
        let turn = completer
            .complete(&[ChatMessage::user(prompt)], None)
            .await
            .unwrap();
        json!({
            "prompt": prompt,
            "enabled": policy.enabled,
            "effort": policy.effort.map(|e| e.as_str()),
            "jinja": sentence,
            "jinja_low": has_low,
            "jinja_xhigh": has_xhigh,
            "think_tokens": toks(&turn.reasoning),
            "think_chars": turn.reasoning.chars().count(),
            "reply_tokens": toks(&turn.content),
            "reply": turn.content,
            "think_head": turn.reasoning.chars().take(180).collect::<String>(),
            "wall_ms": t0.elapsed().as_millis() as u64,
            "watchdog": turn.watchdog_hit,
        })
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_native_think_depth() {
        let cfg = live_cfg();
        let b = cfg.policy.think_budget();
        let hi = "你好";
        let hard = concat!(
            "A Rust index helper `fn idx(n: i32) -> usize { n as usize }` is used as ",
            "`arr[idx(i - 1)]` when `i` can be 0. In two short sentences: name the bug, ",
            "then say the safe return type. Do not call tools. Do not write code."
        );
        let policies = [
            ("off", ThinkPolicy::off_with(&b)),
            ("low", ThinkPolicy::effort_with(&b, Effort::Low)),
            ("medium", ThinkPolicy::effort_with(&b, Effort::Medium)),
            ("xhigh", ThinkPolicy::effort_with(&b, Effort::Xhigh)),
        ];
        let mut rows = Vec::new();
        for (name, policy) in policies {
            eprintln!("\n===== {name} / 你好 =====");
            let a = once(&cfg, policy.clone(), hi).await;
            eprintln!(
                "hi think_tok={} jinja={} wall={}ms reply={:?}",
                a["think_tokens"], a["jinja"], a["wall_ms"], a["reply"]
            );
            eprintln!("===== {name} / hard =====");
            let b_row = once(&cfg, policy, hard).await;
            eprintln!(
                "hard think_tok={} jinja={} wall={}ms",
                b_row["think_tokens"], b_row["jinja"], b_row["wall_ms"]
            );
            rows.push(json!({ "policy": name, "hi": a, "hard": b_row }));
        }
        let report = json!({ "model": cfg.server.model, "rows": rows });
        let path = std::env::temp_dir().join("q38-native-think.json");
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        eprintln!("wrote {}", path.display());
        assert_eq!(rows.len(), 4);
    }
}
