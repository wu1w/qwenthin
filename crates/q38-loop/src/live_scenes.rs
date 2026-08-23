//! One-shot live contrast: coding agent loop vs a naked chat completion.
//! Same model, same ThinkPolicy, same user prompt. Agent gets the coding
//! system prompt + frozen tools. Naked gets the user message only.

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::{json, Value};

    use crate::agent::{Agent, Completer, HttpCompleter, RunOpts};
    use crate::config::Config;
    use crate::family::Family;
    use crate::session::SessionMode;
    use crate::template::ChatMessage;
    use crate::tokenize::count_tokens;

    struct Scene {
        id: &'static str,
        title: &'static str,
        prompt: &'static str,
        seed_files: &'static [(&'static str, &'static str)],
    }

    const SCENES: &[Scene] = &[
        Scene {
            id: "roleplay",
            title: "角色扮演 / 情绪价值",
            prompt: "我今天被老板当众批评了，整个人都垮了。你现在就是我的好朋友，陪我聊聊，给我一点情绪价值。不要列任务清单，不要分析代码，就是陪我说话。",
            seed_files: &[],
        },
        Scene {
            id: "code",
            title: "代码 / 输出一段实现",
            prompt: "写一个 Python 函数 sliding_window_max(nums, k)：返回长度 n-k+1 的列表，第 i 项是 nums[i:i+k] 的最大值。请用单调队列做到 O(n)。只输出函数，不要解释。",
            seed_files: &[],
        },
        Scene {
            id: "office",
            title: "办公助手 / 查材料给建议",
            prompt: "根据工作区里的材料，帮我准备下周给 CEO 的 15 分钟 Q2 复盘：怎么讲业绩下滑但留存上升，优先建议做什么。直接给可讲的结构和建议。",
            seed_files: &[(
                "memo.md",
                "# Q2 内部备忘（未公开）\n\n\
                 - 营收环比 -8%\n\
                 - 付费留存 72% → 81%\n\
                 - 新客 CAC +20%\n\
                 - 原因草稿：两家大客户续约推迟到 Q3；自助订阅人数在涨\n\
                 - CEO 这周只关心：现金流、投放要不要砍\n\
                 - 销售想要加预算；财务建议先冻投放看 4 周\n",
            )],
        },
    ];

    fn live_cfg() -> Config {
        let (mut cfg, _) = Config::load_or_init().unwrap();
        cfg.apply_env();
        cfg
    }

    fn toks(s: &str) -> u32 {
        count_tokens(Family::Qwen38, s).unwrap_or(0)
    }

    fn clip(s: &str, n: usize) -> String {
        let t: String = s.chars().take(n).collect();
        if s.chars().count() > n {
            format!("{t}…")
        } else {
            t
        }
    }

    fn traj(agent: &Agent<HttpCompleter>) -> (u32, String, Vec<String>) {
        let mut think = String::new();
        let mut tools = Vec::new();
        for m in agent.messages() {
            if let Some(r) = &m.reasoning_content {
                think.push_str(r);
            }
            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    if let Some(name) = c["function"]["name"].as_str() {
                        tools.push(name.to_string());
                    }
                }
            }
        }
        (toks(&think), think, tools)
    }

    async fn run_agent(cfg: &Config, scene: &Scene) -> Value {
        let dir = std::env::temp_dir().join(format!(
            "q38-scene-agent-{}-{}",
            scene.id,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in scene.seed_files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
        let completer = HttpCompleter::connect(cfg, policy).await.unwrap();
        let mut opts = RunOpts::from_config(cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = false;
        opts.max_steps = 8;
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = false;
        opts.media = false;
        let mut agent = Agent::new(completer, opts).unwrap();
        let t0 = Instant::now();
        let out = agent.run(scene.prompt).await.unwrap();
        let wall_ms = t0.elapsed().as_millis() as u64;
        let (think_tokens, think, tools) = traj(&agent);
        let sys = agent
            .messages()
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let v = json!({
            "path": "agent",
            "scene": scene.id,
            "wall_ms": wall_ms,
            "steps": out.steps,
            "stop_reason": out.stop_reason,
            "tools": tools,
            "think_tokens": think_tokens,
            "think_chars": think.chars().count(),
            "think_preview": clip(think.trim(), 500),
            "reply": out.text,
            "reply_chars": out.text.chars().count(),
            "reply_tokens": toks(&out.text),
            "system_preview": clip(sys.trim(), 240),
        });
        let _ = std::fs::remove_dir_all(dir);
        v
    }

    async fn run_naked(cfg: &Config, scene: &Scene) -> Value {
        let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
        let completer = HttpCompleter::connect(cfg, policy).await.unwrap();
        let t0 = Instant::now();
        let turn = completer
            .complete(&[ChatMessage::user(scene.prompt)], None)
            .await
            .unwrap();
        let wall_ms = t0.elapsed().as_millis() as u64;
        json!({
            "path": "naked",
            "scene": scene.id,
            "wall_ms": wall_ms,
            "steps": 1,
            "stop_reason": if turn.watchdog_hit {
                Some("watchdog")
            } else if turn.parse_fail {
                Some("parse")
            } else {
                None
            },
            "tools": Vec::<String>::new(),
            "think_tokens": toks(&turn.reasoning),
            "think_chars": turn.reasoning.chars().count(),
            "think_preview": clip(turn.reasoning.trim(), 500),
            "reply": turn.content,
            "reply_chars": turn.content.chars().count(),
            "reply_tokens": toks(&turn.content),
            "system_preview": "",
        })
    }

    struct TraceCompleter {
        inner: HttpCompleter,
        log: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl Completer for TraceCompleter {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            tools: Option<&[Value]>,
        ) -> crate::error::Result<crate::agent::ModelTurn> {
            let policy = self.inner.policy();
            let t0 = Instant::now();
            let turn = self.inner.complete(messages, tools).await?;
            let tool_names: Vec<String> = turn.tool_calls.iter().map(|c| c.name.clone()).collect();
            self.log.lock().expect("trace").push(json!({
                "watchdog": turn.watchdog_hit,
                "parse_fail": turn.parse_fail,
                "thinking_on": policy.enabled,
                "max_tokens": policy.max_tokens,
                "max_think_tokens": policy.max_think_tokens,
                "tools": tool_names,
                "think_tokens": toks(&turn.reasoning),
                "reply_tokens": toks(&turn.content),
                "reply_chars": turn.content.chars().count(),
                "completion_tokens": turn.completion_tokens,
                "prompt_tokens": turn.prompt_tokens,
                "wall_ms": t0.elapsed().as_millis() as u64,
                "reply_tail": clip(turn.content.trim(), 80),
            }));
            Ok(turn)
        }

        fn prefix_meter(&self) -> Option<(Family, crate::policy::TemplateKwargs)> {
            self.inner.prefix_meter()
        }

        fn set_policy(&self, p: crate::policy::ThinkPolicy) {
            self.inner.set_policy(p);
        }

        fn policy(&self) -> Option<crate::policy::ThinkPolicy> {
            Some(self.inner.policy())
        }

        fn media_caps(&self) -> crate::media::MediaCaps {
            self.inner.media_caps()
        }

        fn set_token_sink(&self, sink: Option<crate::agent::TokenSink>) {
            self.inner.set_token_sink(sink);
        }
    }

    fn scene_opts(cfg: &Config, dir: std::path::PathBuf) -> RunOpts {
        let mut opts = RunOpts::from_config(cfg, dir);
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = false;
        opts.max_steps = 8;
        opts.peripheral = false;
        opts.media = false;
        opts
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_office_cutoff_trace() {
        let cfg = live_cfg();
        let scene = &SCENES[2];
        let dir = std::env::temp_dir().join(format!(
            "q38-office-trace-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in scene.seed_files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
        eprintln!(
            "policy max_tokens={} max_think={} enabled={}",
            policy.max_tokens, policy.max_think_tokens, policy.enabled
        );
        let inner = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let completer = TraceCompleter {
            inner,
            log: log.clone(),
        };
        let mut opts = scene_opts(&cfg, dir.clone());
        opts.home = Some(dir.join(".q38-home"));
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent.run(scene.prompt).await.unwrap();
        let steps = log.lock().expect("trace").clone();
        let report = json!({
            "outcome_steps": out.steps,
            "stop_reason": out.stop_reason,
            "reply_chars": out.text.chars().count(),
            "reply_tokens": toks(&out.text),
            "reply_ends_with": clip(
                out.text.chars().rev().take(40).collect::<String>().chars().rev().collect::<String>().as_str(),
                40
            ),
            "http_steps": steps,
        });
        let path = std::env::temp_dir().join("q38-office-cutoff-trace.json");
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
        eprintln!("wrote {}", path.display());
        let _ = std::fs::remove_dir_all(dir);
        assert!(!steps.is_empty());
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_roleplay_prefix_ablation() {
        let cfg = live_cfg();
        let prompt = SCENES[0].prompt;
        let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
        let mut variants = Vec::new();

        // current identity + tools
        {
            let dir = std::env::temp_dir().join(format!(
                "q38-rp-current-tools-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut opts = scene_opts(&cfg, dir.clone());
            opts.home = Some(dir.join(".q38-home"));
            let mut agent = Agent::new(completer, opts).unwrap();
            let t0 = Instant::now();
            let out = agent.run(prompt).await.unwrap();
            variants.push(json!({
                "id": "current_tools",
                "wall_ms": t0.elapsed().as_millis() as u64,
                "steps": out.steps,
                "tools": agent.tools().len(),
                "reply": out.text,
                "think_preview": agent.messages().iter().filter_map(|m| m.reasoning_content.clone()).collect::<Vec<_>>().join("\n"),
            }));
            let _ = std::fs::remove_dir_all(dir);
        }

        // force "coding agent" identity + tools
        {
            let dir = std::env::temp_dir().join(format!(
                "q38-rp-coding-tools-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut opts = scene_opts(&cfg, dir.clone());
            opts.home = Some(dir.join(".q38-home"));
            let mut agent = Agent::new(completer, opts).unwrap();
            let mut msgs = agent.messages().to_vec();
            if let Some(sys) = msgs.iter_mut().find(|m| m.role == "system") {
                if let Some(c) = sys.content.as_mut() {
                    *c = c.replacen("工作区助手。", "编程助手。", 1);
                }
            }
            agent.load_messages(msgs);
            let t0 = Instant::now();
            let out = agent.run(prompt).await.unwrap();
            variants.push(json!({
                "id": "coding_identity",
                "wall_ms": t0.elapsed().as_millis() as u64,
                "steps": out.steps,
                "tools": agent.tools().len(),
                "reply": out.text,
                "think_preview": agent.messages().iter().filter_map(|m| m.reasoning_content.clone()).collect::<Vec<_>>().join("\n"),
            }));
            let _ = std::fs::remove_dir_all(dir);
        }

        // coding identity, no tools (Jinja tools block absent)
        {
            let dir = std::env::temp_dir().join(format!(
                "q38-rp-coding-notools-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut opts = scene_opts(&cfg, dir.clone());
            opts.home = Some(dir.join(".q38-home"));
            opts.with_tools = false;
            let mut agent = Agent::new(completer, opts).unwrap();
            let t0 = Instant::now();
            let out = agent.run(prompt).await.unwrap();
            variants.push(json!({
                "id": "coding_notools",
                "wall_ms": t0.elapsed().as_millis() as u64,
                "steps": out.steps,
                "tools": agent.tools().len(),
                "reply": out.text,
                "think_preview": agent.messages().iter().filter_map(|m| m.reasoning_content.clone()).collect::<Vec<_>>().join("\n"),
            }));
            let _ = std::fs::remove_dir_all(dir);
        }

        // naked
        {
            let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
            let t0 = Instant::now();
            let turn = completer
                .complete(&[ChatMessage::user(prompt)], None)
                .await
                .unwrap();
            variants.push(json!({
                "id": "naked",
                "wall_ms": t0.elapsed().as_millis() as u64,
                "steps": 1,
                "tools": 0,
                "reply": turn.content,
                "think_preview": turn.reasoning,
            }));
        }

        let report = json!({ "prompt": prompt, "variants": variants });
        let path = std::env::temp_dir().join("q38-roleplay-ablation.json");
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        for v in &variants {
            eprintln!(
                "\n===== {} wall={} tools={} =====\n{}\n--- think ---\n{}\n",
                v["id"],
                v["wall_ms"],
                v["tools"],
                v["reply"].as_str().unwrap_or(""),
                clip(v["think_preview"].as_str().unwrap_or(""), 400)
            );
        }
        eprintln!("wrote {}", path.display());
        assert_eq!(variants.len(), 4);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_vs_naked_three_scenes() {
        let cfg = live_cfg();
        let mut runs = Vec::new();
        for scene in SCENES {
            eprintln!("\n===== {} / agent =====", scene.id);
            let agent = run_agent(&cfg, scene).await;
            eprintln!(
                "agent wall={}ms steps={} tools={:?} think_tok={} reply_chars={}",
                agent["wall_ms"],
                agent["steps"],
                agent["tools"],
                agent["think_tokens"],
                agent["reply_chars"]
            );
            eprintln!("\n===== {} / naked =====", scene.id);
            let naked = run_naked(&cfg, scene).await;
            eprintln!(
                "naked wall={}ms think_tok={} reply_chars={}",
                naked["wall_ms"], naked["think_tokens"], naked["reply_chars"]
            );
            runs.push(json!({
                "id": scene.id,
                "title": scene.title,
                "prompt": scene.prompt,
                "agent": agent,
                "naked": naked,
            }));
        }
        let report = json!({
            "model": cfg.server.model,
            "policy": "SessionMode::Agent default (thinking on, effort low)",
            "runs": runs,
        });
        let out = std::env::temp_dir().join("q38-scene-compare.json");
        std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        eprintln!("wrote {}", out.display());
        assert_eq!(runs.len(), 3);
    }
}
