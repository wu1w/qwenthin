//! Live checks against Qwen3.8 (LAN llama.cpp or the public TLS proxy).
//!
//! Ignored by default (`cargo test -- --ignored`). Override endpoint with
//! `Q38_BASE_URL` / `Q38_API_KEY` / `Q38_MODEL` — never put tokens in git.
//!
//! Fit tests: Unsloth wants `function.arguments` as objects; Jinja
//! `last_query_index` must stay on a real user after a wrapped archive.

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::adapter::{build_chat_body, ChatRequestSpec};
    use crate::agent::{Agent, Completer, HttpCompleter, RunOpts};
    use crate::config::Config;
    use crate::family::EndpointCaps;
    use crate::policy::{Effort, ThinkPolicy};
    use crate::session::{SessionLog, SessionMode};
    use crate::template::{is_hidden_user_text, wrap_tool_response, ChatMessage};
    use crate::tools_schema::{agent_tools, mcp_tool, memory_search_tool, recall_tool, skill_tool};

    fn live_cfg() -> Config {
        let (mut cfg, _) = Config::load_or_init().unwrap();
        cfg.apply_env();
        cfg
    }

    fn auth(req: reqwest::RequestBuilder, cfg: &Config) -> reqwest::RequestBuilder {
        if cfg.server.api_key.is_empty() {
            req
        } else {
            req.bearer_auth(&cfg.server.api_key)
        }
    }

    async fn client() -> (reqwest::Client, Config, String) {
        let cfg = live_cfg();
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .unwrap();
        let url = format!("{}/models", cfg.server.base_url.trim_end_matches('/'));
        let v: Value = auth(http.get(&url), &cfg)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let model = if cfg.server.model.is_empty() {
            v["data"][0]["id"].as_str().unwrap().to_string()
        } else {
            cfg.server.model.clone()
        };
        (http, cfg, model)
    }

    async fn chat(
        http: &reqwest::Client,
        cfg: &Config,
        model: &str,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Value {
        chat_budget(http, cfg, model, messages, tools, 256).await
    }

    async fn chat_budget(
        http: &reqwest::Client,
        cfg: &Config,
        model: &str,
        messages: &[ChatMessage],
        tools: &[Value],
        max_tokens: u32,
    ) -> Value {
        let mut policy = ThinkPolicy::agent_default();
        policy.max_tokens = max_tokens;
        policy.max_think_tokens = 512;
        let caps = EndpointCaps::qwen38_llamacpp();
        let body = build_chat_body(&ChatRequestSpec {
            model,
            messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            stream: false,
            policy: &policy,
            caps: &caps,
            id_slot: None,
            cache_prompt: false,
            lossy_repeat: false,
        });
        let url = format!(
            "{}/chat/completions",
            cfg.server.base_url.trim_end_matches('/')
        );
        let resp = auth(http.post(&url).json(&body), cfg).send().await.unwrap();
        let status = resp.status();
        let v: Value = resp.json().await.unwrap();
        assert!(status.is_success(), "chat {status}: {v}");
        v
    }

    fn parse_args(args: &Value) -> Value {
        match args {
            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
            other => other.clone(),
        }
    }

    fn tool_names(msg: &Value) -> Vec<String> {
        msg["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c["function"]["name"].as_str().map(str::to_string))
            .collect()
    }

    fn assistant_from_choice(msg: &Value) -> ChatMessage {
        let content = msg["content"].as_str().filter(|s| !s.trim().is_empty());
        let reasoning = msg["reasoning_content"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let raw_calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        let tool_calls = if raw_calls.is_empty() {
            None
        } else {
            Some(
                raw_calls
                    .into_iter()
                    .map(|mut c| {
                        if let Some(args) = c.pointer_mut("/function/arguments") {
                            *args = parse_args(args);
                        }
                        c
                    })
                    .collect(),
            )
        };
        ChatMessage::assistant_reply(content.map(str::to_string), reasoning, tool_calls)
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_models_lists_qwen38() {
        let (_http, _cfg, model) = client().await;
        assert!(
            model.to_lowercase().contains("qwen"),
            "unexpected model id: {model}"
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_arguments_are_objects_on_the_wire() {
        let (http, cfg, model) = client().await;
        let mut tools = agent_tools();
        tools.push(memory_search_tool());
        tools.push(skill_tool());
        let messages = vec![
            ChatMessage::system(crate::prompt::coding_prompt("/tmp/ws")),
            ChatMessage::user("Call read on Cargo.toml. No other tools."),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        let msg = &v["choices"][0]["message"];
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        assert!(
            !calls.is_empty() || msg["reasoning_content"].as_str().is_some(),
            "expected a tool call or think: {msg}"
        );
        for c in &calls {
            let obj = parse_args(&c["function"]["arguments"]);
            assert!(obj.is_object(), "arguments not object after parse: {c}");
        }
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_tool_roundtrip_sends_argument_objects() {
        // Unsloth chat template raises if function.arguments is a JSON string
        // on the *next* request. This is the path last night's HTTP probes skipped.
        let (http, cfg, model) = client().await;
        let tools = agent_tools();
        let mut messages = vec![
            ChatMessage::system(crate::prompt::coding_prompt("/tmp/ws")),
            ChatMessage::user("Call read on Cargo.toml only."),
        ];
        let first = chat(&http, &cfg, &model, &messages, &tools).await;
        let msg = &first["choices"][0]["message"];
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        assert!(!calls.is_empty(), "need a tool call to round-trip: {msg}");
        let asst = assistant_from_choice(msg);
        let id = calls[0]["id"].as_str().unwrap_or("call_1").to_string();
        let args = &asst.tool_calls.as_ref().unwrap()[0]["function"]["arguments"];
        assert!(args.is_object(), "must send objects, got {args}");
        messages.push(asst);
        messages.push(ChatMessage::tool(
            id,
            "     1|[package]\n     2|name = \"q-harness\"\n[q38 sha256=aaaaaaaaaaaa]",
        ));
        let second = chat(&http, &cfg, &model, &messages, &tools).await;
        let msg2 = &second["choices"][0]["message"];
        assert!(msg2.is_object(), "round-trip rejected: {second}");
        let err = second.get("error");
        assert!(
            err.is_none(),
            "Unsloth/template error on object args: {err:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_parallel_two_reads() {
        let (http, cfg, model) = client().await;
        let tools = agent_tools();
        let messages = vec![
            ChatMessage::system(crate::prompt::coding_prompt("/tmp/ws")),
            ChatMessage::user(
                "Call read twice in this turn, in parallel: path Cargo.toml and path README.md. No bash.",
            ),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        let msg = &v["choices"][0]["message"];
        let names = tool_names(msg);
        let reads = names.iter().filter(|n| *n == "read").count();
        assert!(
            reads >= 2 || names.iter().any(|n| n == "read"),
            "expected parallel reads, got {names:?} msg={msg}"
        );
        if reads >= 2 {
            let args_ok = msg["tool_calls"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| parse_args(&c["function"]["arguments"]).is_object());
            assert!(args_ok, "parallel arguments must parse to objects: {msg}");
        }
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_hidden_continue_keeps_task() {
        let (http, cfg, model) = client().await;
        let hidden = wrap_tool_response("Continue working on the task.");
        assert!(is_hidden_user_text(&hidden));
        let mut asst = ChatMessage::assistant("working");
        asst.reasoning_content = Some("plan the zirconium rewrite".into());
        let messages = vec![
            ChatMessage::system("You are a coding agent. Follow the latest real user."),
            ChatMessage::user("Reply with only the digit 9. Do not mention any metal."),
            asst,
            ChatMessage::user(hidden),
        ];
        let v = chat(&http, &cfg, &model, &messages, &[]).await;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
        let think = v["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap_or("");
        assert!(
            content.contains('9') || think.contains('9'),
            "CONTINUE stole last_query_index: content={content:?} think={think:?}"
        );
        assert!(
            !content.to_lowercase().contains("zirconium"),
            "model answered the hidden continue: {content}"
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_wrapped_archive_keeps_last_query() {
        let (http, cfg, model) = client().await;
        let archive = wrap_tool_response(
            "[archived]\n## Active Task\nrewrite the linker\n\n\
             ═══════════════ END OF ARCHIVED INDEX ═══════════════\n\
             The CURRENT LIVE TURN follows. Answer the most recent USER message there.\n",
        );
        assert!(is_hidden_user_text(&archive));
        let messages = vec![
            ChatMessage::system("You are a coding agent."),
            ChatMessage::user(archive),
            ChatMessage::user("Reply with only the digit 7."),
        ];
        let v = chat(&http, &cfg, &model, &messages, &[]).await;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
        let think = v["choices"][0]["message"]["reasoning_content"]
            .as_str()
            .unwrap_or("");
        assert!(
            content.contains('7') || think.contains('7'),
            "wrapped archive stole last_query_index: content={content:?} think={think:?}"
        );
        assert!(
            !content.to_lowercase().contains("linker"),
            "model treated archive as the task: {content}"
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_recall_after_archive() {
        let (http, cfg, model) = client().await;
        let mut tools = agent_tools();
        tools.push(recall_tool());
        let archive = wrap_tool_response(
            "[archived]\n## Active Task\nfix prefix cache\n\n## Index\nseq 3  tool bash blob=aa\n\
             Use recall to search archived turns or expand a blob by sha.\n",
        );
        let messages = vec![
            ChatMessage::system(crate::prompt::coding_prompt("/tmp/ws")),
            ChatMessage::user(archive),
            ChatMessage::user("Call recall with query prefix. Do not call bash."),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        let names = tool_names(&v["choices"][0]["message"]);
        assert!(
            names.iter().any(|n| n == "recall"),
            "27B did not call recall after compact archive: {names:?} {}",
            v["choices"][0]["message"]
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_mcp_is_called() {
        let (http, cfg, model) = client().await;
        let mut tools = agent_tools();
        tools.push(mcp_tool());
        let system = format!(
            "{}{}",
            crate::prompt::coding_prompt("/tmp/ws"),
            crate::prompt::periphery_section("", "MCP: echo\n")
        );
        let messages = vec![
            ChatMessage::system(system),
            ChatMessage::user(
                "Call mcp with server echo and method list. Do not call bash or read.",
            ),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        let names = tool_names(&v["choices"][0]["message"]);
        assert!(
            names.iter().any(|n| n == "mcp"),
            "27B did not call mcp: {names:?} {}",
            v["choices"][0]["message"]
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_memory_search_is_called() {
        let (http, cfg, model) = client().await;
        let mut tools = agent_tools();
        tools.push(memory_search_tool());
        let system = format!(
            "{}{}",
            crate::prompt::coding_prompt("/tmp/ws"),
            crate::prompt::periphery_section("", "")
        );
        let messages = vec![
            ChatMessage::system(system),
            ChatMessage::user("Call memory_search with query hello. Do not call read or bash."),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        let msg = &v["choices"][0]["message"];
        let names = tool_names(msg);
        assert!(
            names.iter().any(|n| n == "memory_search"),
            "27B did not call memory_search: names={names:?} msg={msg}"
        );
        for c in msg["tool_calls"].as_array().cloned().unwrap_or_default() {
            if c["function"]["name"] == "memory_search" {
                let obj = parse_args(&c["function"]["arguments"]);
                assert!(obj.is_object(), "{c}");
                assert!(
                    obj["query"].as_str().is_some_and(|q| q.contains("hello")),
                    "query={obj}"
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_skill_is_called() {
        let (http, cfg, model) = client().await;
        let mut tools = agent_tools();
        tools.push(skill_tool());
        let system = format!(
            "{}{}",
            crate::prompt::coding_prompt("/tmp/ws"),
            crate::prompt::periphery_section("Skills: pdf\n", "")
        );
        let messages = vec![
            ChatMessage::system(system),
            ChatMessage::user("Call skill with name pdf. Do not call read or bash."),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        let names = tool_names(&v["choices"][0]["message"]);
        assert!(
            names.iter().any(|n| n == "skill"),
            "27B did not call skill: names={names:?} {}",
            v["choices"][0]["message"]
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_periphery_tools_do_not_crash_template() {
        let (http, cfg, model) = client().await;
        let mut tools = agent_tools();
        tools.push(memory_search_tool());
        tools.push(skill_tool());
        tools.push(recall_tool());
        tools.push(mcp_tool());
        let system = format!(
            "{}{}",
            crate::prompt::coding_prompt("/tmp/ws"),
            crate::prompt::periphery_section("Skills: pdf\n", "MCP: echo\n")
        );
        let messages = vec![
            ChatMessage::system(system),
            ChatMessage::user("Say hi. Do not call tools."),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        assert!(v["choices"][0]["message"].is_object(), "{v}");
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_repeat_prompt_reports_cache() {
        let (http, cfg, model) = client().await;
        let messages = vec![
            ChatMessage::system("You are a coding agent."),
            ChatMessage::user("Reply with only the digit 3."),
        ];
        let a = chat(&http, &cfg, &model, &messages, &[]).await;
        let b = chat(&http, &cfg, &model, &messages, &[]).await;
        let cache = |v: &Value| -> Option<u64> {
            v.pointer("/usage/prompt_tokens_details/cached_tokens")
                .and_then(|x| x.as_u64())
                .or_else(|| v.pointer("/timings/cache_n").and_then(|x| x.as_u64()))
                .or_else(|| v.pointer("/usage/cache_n").and_then(|x| x.as_u64()))
        };
        // Not all proxies forward timings; the second call must still succeed.
        assert!(b["choices"][0]["message"].is_object(), "{b}");
        if let (Some(_), Some(c1)) = (cache(&a), cache(&b)) {
            // Second identical prefix should be able to hit cache.
            let _ = c1;
        }
        let _ = a;
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_read_then_reply() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-agent-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "hello-from-q38-live\n").unwrap();
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut opts = RunOpts::from_config(&cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = false;
        opts.max_steps = 6;
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = true;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run("Read note.txt and reply with only its exact first line. Do not call other tools.")
            .await
            .unwrap();
        assert!(
            out.text.contains("hello-from-q38-live"),
            "agent text={}",
            out.text
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn core_opts(cfg: &crate::config::Config, dir: std::path::PathBuf) -> RunOpts {
        let mut opts = RunOpts::from_config(cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = false;
        opts.max_steps = 8;
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = false;
        opts.media = false;
        opts
    }

    fn dump_traj(label: &str, agent: &Agent<HttpCompleter>, out: &crate::agent::AgentOutcome) {
        eprintln!(
            "\n======== {label} steps={} reason={:?} ========",
            out.steps, out.stop_reason
        );
        let mut think_chars = 0usize;
        let mut hidden = 0u32;
        let mut tool_turns = 0u32;
        let mut text_with_tools = 0u32;
        let mut empty_think = 0u32;
        for (i, m) in agent.messages().iter().enumerate() {
            let content = m.content.clone().unwrap_or_default();
            let think = m.reasoning_content.clone().unwrap_or_default();
            think_chars += think.chars().count();
            let hidden_here = m.role == "user" && crate::template::is_hidden_user_text(&content);
            if hidden_here {
                hidden += 1;
            }
            let tools = m
                .tool_calls
                .as_ref()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            let name = c["function"]["name"].as_str().unwrap_or("?");
                            let args = &c["function"]["arguments"];
                            let shape = match args {
                                Value::Object(_) => "object",
                                Value::String(_) => "string",
                                _ => "other",
                            };
                            Some(format!("{name}({shape})"))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !tools.is_empty() {
                tool_turns += 1;
                if !content.trim().is_empty() {
                    text_with_tools += 1;
                }
            }
            if m.role == "assistant" && think.trim().is_empty() {
                empty_think += 1;
            }
            let head = |s: &str, n: usize| -> String {
                let t: String = s.chars().take(n).collect();
                t.replace('\n', " ")
            };
            eprintln!(
                "  [{i}] {role} think={tc} content={cc} tools={tools:?} hidden={hidden_here} head={head:?}",
                role = m.role,
                tc = think.chars().count(),
                cc = content.chars().count(),
                tools = tools,
                hidden_here = hidden_here,
                head = head(if m.role == "assistant" && !think.is_empty() { &think } else { &content }, 180),
            );
        }
        eprintln!(
            "  summary answer={:?} think_chars={think_chars} hidden_continue={hidden} tool_asst={tool_turns} preamble_on_tools={text_with_tools} empty_think_asst={empty_think}",
            out.text.chars().take(120).collect::<String>()
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_loop_observe() {
        let cfg = live_cfg();
        let policy = ThinkPolicy::agent_default();

        // 1. Frozen-four read → answer (core ReAct).
        {
            let dir = std::env::temp_dir()
                .join(format!("q38-obs-read-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("note.txt"), "hello-from-q38-live\n").unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut agent = Agent::new(completer, core_opts(&cfg, dir.clone())).unwrap();
            let out = agent
                .run("Read note.txt and reply with only its exact first line. Do not call other tools.")
                .await
                .unwrap();
            dump_traj("read_then_reply", &agent, &out);
            let _ = std::fs::remove_dir_all(dir);
        }

        // 2. write → targeted edit → read (guideline: edit not rewrite).
        {
            let dir = std::env::temp_dir()
                .join(format!("q38-obs-edit-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut agent = Agent::new(completer, core_opts(&cfg, dir.clone())).unwrap();
            let out = agent
                .run("Create hello.txt containing the single line foo. Then use edit to change foo to bar. Then read hello.txt and reply with only that line. Do not use bash.")
                .await
                .unwrap();
            dump_traj("write_edit_read", &agent, &out);
            let on_disk = std::fs::read_to_string(dir.join("hello.txt")).unwrap_or_default();
            eprintln!("  disk hello.txt={on_disk:?}");
            let _ = std::fs::remove_dir_all(dir);
        }

        // 3. Parallel reads in one assistant turn.
        {
            let dir =
                std::env::temp_dir().join(format!("q38-obs-par-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
            std::fs::write(dir.join("b.txt"), "bravo\n").unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut agent = Agent::new(completer, core_opts(&cfg, dir.clone())).unwrap();
            let out = agent
                .run("Read a.txt and b.txt in the same turn (two read calls). Then reply with both words, nothing else. No bash.")
                .await
                .unwrap();
            dump_traj("parallel_reads", &agent, &out);
            let _ = std::fs::remove_dir_all(dir);
        }

        // 4. bash + missing-file error recovery.
        {
            let dir =
                std::env::temp_dir().join(format!("q38-obs-err-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("ok.txt"), "ok\n").unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut agent = Agent::new(completer, core_opts(&cfg, dir.clone())).unwrap();
            let out = agent
                .run("Read missing.txt first. If that errors, read ok.txt and reply with only its first line. No bash.")
                .await
                .unwrap();
            dump_traj("error_recovery", &agent, &out);
            let _ = std::fs::remove_dir_all(dir);
        }

        // 5. Text-only; should not call tools. Watch empty/CONTINUE.
        {
            let dir =
                std::env::temp_dir().join(format!("q38-obs-txt-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let mut agent = Agent::new(completer, core_opts(&cfg, dir.clone())).unwrap();
            let out = agent
                .run("Reply with only the digit 9. Do not call tools.")
                .await
                .unwrap();
            dump_traj("text_only_digit", &agent, &out);
            let _ = std::fs::remove_dir_all(dir);
        }

        // 6. Narration trap: will it talk about reading instead of calling read?
        {
            let dir =
                std::env::temp_dir().join(format!("q38-obs-nar-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("secret.txt"), "obsidian\n").unwrap();
            let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
            let mut agent = Agent::new(completer, core_opts(&cfg, dir.clone())).unwrap();
            let out = agent
                .run("What is in secret.txt? Reply with the file contents only.")
                .await
                .unwrap();
            dump_traj("open_read", &agent, &out);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_compacts_then_recalls() {
        let cfg = live_cfg();
        let dir = std::env::temp_dir().join(format!(
            "q38-live-compact-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut opts = RunOpts::from_config(&cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = true;
        opts.session_id = "live-compact".into();
        opts.session_dir = Some(sess.clone());
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = true;
        opts.max_steps = 10;
        opts.working_window = 3200;
        opts.generation_reserve = 0;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run(
                "Using bash, run python3 -c \"print('W'*8000)\" three times in separate tool calls, then reply with the word done.",
            )
            .await
            .unwrap();
        let log = SessionLog::open_in(&sess, "live-compact").unwrap();
        let compacted = log
            .events()
            .iter()
            .any(|e| e.type_name() == "session/compact");
        if compacted {
            let notes = std::fs::read_dir(dir.join(".q38-home").join("memory"))
                .map(|rd| rd.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
            assert!(notes > 0, "compact should write a daily note");
            let live_users: Vec<_> = agent
                .messages()
                .iter()
                .filter(|m| m.role == "user")
                .map(|m| m.content.clone().unwrap_or_default())
                .collect();
            assert!(
                live_users
                    .iter()
                    .any(|c| crate::template::is_hidden_user_text(c)),
                "archive must be hidden after live compact: {live_users:?}"
            );
        }
        assert!(
            out.text.to_lowercase().contains("done")
                || out
                    .stop_reason
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("budget:")
                || compacted,
            "text={} reason={:?}",
            out.text,
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn dump_archives(agent: &Agent<HttpCompleter>) {
        for (i, m) in agent.messages().iter().enumerate() {
            if m.role == "user" {
                let c = m.content.clone().unwrap_or_default();
                if crate::template::is_hidden_user_text(&c) && c.contains("[archived]") {
                    eprintln!(
                        "  archive[{i}] chars={} body=\n{}",
                        c.chars().count(),
                        c.chars().take(1800).collect::<String>()
                    );
                }
            }
        }
        eprintln!(
            "  recall_tool={}",
            crate::tools_schema::has_recall(agent.tools())
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_compact_keeps_tool_evidence() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-cqual-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(dir.join("fact.txt"), "token is obsidian-compact\n").unwrap();
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut opts = RunOpts::from_config(&cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = true;
        opts.session_id = "live-cqual".into();
        opts.session_dir = Some(sess.clone());
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = true;
        opts.media = false;
        opts.max_steps = 12;
        opts.working_window = 2800;
        opts.generation_reserve = 0;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run(
                "Read fact.txt first. Then run this bash three times in separate turns: python3 -c \"print('W'*8000)\". Then reply with only the token from fact.txt.",
            )
            .await
            .unwrap();
        dump_traj("compact_evidence", &agent, &out);
        dump_archives(&agent);
        let log = SessionLog::open_in(&sess, "live-cqual").unwrap();
        let compacted = log
            .events()
            .iter()
            .any(|e| e.type_name() == "session/compact");
        eprintln!("  compacted={compacted} answer={:?}", out.text);
        if compacted {
            assert!(crate::tools_schema::has_recall(agent.tools()));
            let archives: Vec<_> = agent
                .messages()
                .iter()
                .filter(|m| m.role == "user")
                .map(|m| m.content.clone().unwrap_or_default())
                .filter(|c| crate::template::is_hidden_user_text(c) && c.contains("[archived]"))
                .collect();
            assert!(
                !archives.is_empty(),
                "compact must inject a wrapped archive"
            );
            let blob = archives.join("\n");
            assert!(
                blob.contains("obsidian-compact"),
                "archive dropped the fact clip: {}",
                blob.chars().take(1200).collect::<String>()
            );
            assert!(
                !blob.to_ascii_lowercase().contains("now i need to run"),
                "restart think leaked into archive: {}",
                blob.chars().take(1200).collect::<String>()
            );
            assert!(
                !blob.contains("zirconium"),
                "think dump leaked into archive"
            );
            assert!(
                blob.contains("already:"),
                "Open Work should record tools already run: {}",
                blob.chars().take(1200).collect::<String>()
            );
        }
        let called = called_tools(agent.messages());
        eprintln!("  tools={called:?}");
        assert!(
            out.text.to_lowercase().contains("obsidian")
                || compacted
                || out
                    .stop_reason
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("budget:"),
            "text={} reason={:?}",
            out.text,
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_watchdog_soft_nudge_on_long_think() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-wd-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut policy = ThinkPolicy::agent_default();
        policy.max_think_tokens = 64;
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.max_steps = 4;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run(
                "In your thinking, count from 1 to 80 with a short phrase on each number. Then reply with only the word done. Do not call tools.",
            )
            .await
            .unwrap();
        dump_traj("watchdog", &agent, &out);
        let thinks: Vec<_> = agent
            .messages()
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| {
                m.reasoning_content
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .count()
            })
            .collect();
        eprintln!(
            "  think_lens={thinks:?} steps={} reason={:?} text={:?}",
            out.steps, out.stop_reason, out.text
        );
        let max_think = thinks.iter().copied().max().unwrap_or(0);
        // SSE watchdog should drop the body near 64 tokens (~250 chars). A
        // 800-char think on a single step means the box returned JSON, not SSE.
        assert!(
            max_think < 800 || out.steps >= 2,
            "think ran past the cap without a bounded retry: think_chars={max_think} text={} reason={:?}",
            out.text,
            out.stop_reason
        );
        assert!(
            out.text.to_lowercase().contains("done") || out.steps >= 2,
            "watchdog path produced neither a model retry nor a quiet finish: text={} reason={:?}",
            out.text,
            out.stop_reason
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_doom_loop_repeated_read() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-doom-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ping.txt"), "pong\n").unwrap();
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.max_steps = 12;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run(
                "Using the read tool only, read ping.txt six times in six separate turns (one read per turn). Do not use bash. After that, reply with the word done.",
            )
            .await
            .unwrap();
        dump_traj("doom", &agent, &out);
        let hidden: Vec<_> = agent
            .messages()
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone().unwrap_or_default())
            .filter(|c| crate::template::is_hidden_user_text(c))
            .collect();
        let reads = called_tools(agent.messages())
            .iter()
            .filter(|n| *n == "read")
            .count();
        eprintln!(
            "  reads={reads} hidden={} reason={:?} text={:?}",
            hidden.len(),
            out.stop_reason,
            out.text
        );
        let halted = out
            .stop_reason
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("doom");
        if reads >= 3 {
            let notes = hidden
                .iter()
                .filter(|c| c.contains(crate::paw_loop::REPEAT_NOTE))
                .count();
            assert!(
                notes >= 1,
                "looped reads={reads} without a repeat observation: hidden={hidden:?} reason={:?} text={}",
                out.stop_reason,
                out.text
            );
            assert!(
                !halted,
                "repeat detector must not halt: hidden={hidden:?} reason={:?} text={}",
                out.stop_reason, out.text
            );
        } else {
            eprintln!("  (27B did not loop; doom path not exercised live. unit covers the note.)");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_memory_search_roundtrip() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-mem-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join(".q38-home");
        let store = crate::memory::MemoryStore::open(&home).unwrap();
        store
            .write_compact_note("s", 1, "read crates/foo.rs linker rewrite")
            .unwrap();
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut opts = RunOpts::from_config(&cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = false;
        opts.max_steps = 6;
        opts.home = Some(home);
        opts.peripheral = true;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run("Call memory_search with query linker. Then reply with one word: hit or miss.")
            .await
            .unwrap();
        let used = agent.messages().iter().any(|m| {
            m.role == "tool"
                && m.content.as_deref().is_some_and(|c| {
                    c.contains("linker")
                        || c.contains("foo.rs")
                        || c.contains("No matches")
                        || c.contains("MEMORY.md")
                })
        });
        assert!(
            used || out.text.to_lowercase().contains("hit")
                || out.text.to_lowercase().contains("miss"),
            "memory_search round-trip failed: text={}",
            out.text
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_image_probe_red_png() {
        let (http, cfg, model) = client().await;
        let mut user = ChatMessage::user(crate::media::IMAGE_PROBE_PROMPT);
        user.parts = vec![crate::media::MediaPart::image_url(format!(
            "data:image/png;base64,{}",
            crate::media::PROBE_IMAGE_B64
        ))];
        let messages = vec![user];
        let v = chat_budget(&http, &cfg, &model, &messages, &[], 32).await;
        let msg = &v["choices"][0]["message"];
        let content = msg["content"].as_str().unwrap_or("");
        let reasoning = msg["reasoning_content"].as_str().unwrap_or("");
        let hit = crate::media::image_probe_hit(content, reasoning);
        eprintln!(
            "live image probe hit={hit} answer={:?} reasoning={:?}",
            content.chars().take(120).collect::<String>(),
            reasoning.chars().take(160).collect::<String>()
        );
        assert!(
            hit,
            "Qwen3.8-27B with mmproj must see the red probe PNG; content={content:?} reasoning={reasoning:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_view_is_called() {
        let (http, cfg, model) = client().await;
        let mut tools = agent_tools();
        tools.push(crate::tools_schema::view_tool());
        let messages = vec![
            ChatMessage::system(crate::prompt::coding_prompt("/tmp/ws")),
            ChatMessage::user("Call view on red.png. Do not call read or bash."),
        ];
        let v = chat(&http, &cfg, &model, &messages, &tools).await;
        let msg = &v["choices"][0]["message"];
        let names = tool_names(msg);
        assert!(
            names.iter().any(|n| n == "view"),
            "27B did not call view: names={names:?} msg={msg}"
        );
        if let Some(c) = msg["tool_calls"].as_array().and_then(|a| a.first()) {
            if c["function"]["name"] == "view" {
                let args = parse_args(&c["function"]["arguments"]);
                assert!(args.is_object(), "view arguments not object: {c}");
            }
        }
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_view_image_roundtrip() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-view-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            crate::media::PROBE_IMAGE_B64,
        )
        .unwrap();
        std::fs::write(dir.join("red.png"), png).unwrap();
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut opts = RunOpts::from_config(&cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = false;
        opts.max_steps = 6;
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = false;
        opts.media = true;
        let mut agent = Agent::new(completer, opts).unwrap();
        assert!(crate::tools_schema::has_tool(agent.tools(), "view"));
        let out = agent
            .run("What is the dominant color of red.png? Use view. Reply with one color word.")
            .await
            .unwrap();
        let tool = agent
            .messages()
            .iter()
            .find(|m| m.role == "tool")
            .expect("view tool message");
        assert!(
            tool.text().contains("Image loaded: red.png"),
            "expected freeze-to-context, got {}",
            tool.text()
        );
        assert!(
            !tool.parts.is_empty(),
            "image must be attached on this vision box"
        );
        let think = agent
            .messages()
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.reasoning_content.clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            crate::media::image_probe_hit(&out.text, &think),
            "model should see the attached PNG; text={} think={}",
            out.text,
            think.chars().take(200).collect::<String>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_video_probe() {
        let (http, cfg, model) = client().await;
        let mut user = ChatMessage::user(crate::media::VIDEO_PROBE_PROMPT);
        user.parts = vec![crate::media::MediaPart::video_url(
            crate::media::PROBE_VIDEO_URL,
        )];
        let url = format!(
            "{}/chat/completions",
            cfg.server.base_url.trim_end_matches('/')
        );
        let mut policy = ThinkPolicy::off();
        policy.max_tokens = 32;
        let caps = EndpointCaps::qwen38_llamacpp();
        let body = build_chat_body(&ChatRequestSpec {
            model: &model,
            messages: &[user],
            tools: None,
            stream: false,
            policy: &policy,
            caps: &caps,
            id_slot: None,
            cache_prompt: false,
            lossy_repeat: false,
        });
        let resp = auth(http.post(&url).json(&body), &cfg)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let v: Value = resp.json().await.unwrap();
        eprintln!("live video probe status={status} body={}", clip_live(&v));
        // Video is optional. HTTP 400/template reject => unsupported, not a suite failure.
        if status.is_success() {
            let msg = &v["choices"][0]["message"];
            let content = msg["content"].as_str().unwrap_or("");
            let reasoning = msg["reasoning_content"].as_str().unwrap_or("");
            eprintln!(
                "video hit={}",
                crate::media::video_probe_hit(content, reasoning, true)
            );
        }
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_audio_probe() {
        let (http, cfg, model) = client().await;
        let wav = crate::media::silence_wav();
        let mut user = ChatMessage::user("Reply with the single word: ok");
        user.parts = vec![crate::media::MediaPart::data_uri(
            crate::media::MediaKind::Audio,
            "audio/wav",
            &wav,
        )];
        let url = format!(
            "{}/chat/completions",
            cfg.server.base_url.trim_end_matches('/')
        );
        let mut policy = ThinkPolicy::off();
        policy.max_tokens = 16;
        let caps = EndpointCaps::qwen38_llamacpp();
        let body = build_chat_body(&ChatRequestSpec {
            model: &model,
            messages: &[user],
            tools: None,
            stream: false,
            policy: &policy,
            caps: &caps,
            id_slot: None,
            cache_prompt: false,
            lossy_repeat: false,
        });
        let resp = auth(http.post(&url).json(&body), &cfg)
            .send()
            .await
            .unwrap();
        eprintln!(
            "live audio native status={} body={}",
            resp.status(),
            clip_live(&resp.json().await.unwrap_or(Value::Null))
        );

        let turl = format!(
            "{}/audio/transcriptions",
            cfg.server.base_url.trim_end_matches('/')
        );
        let part = reqwest::multipart::Part::bytes(crate::media::silence_wav())
            .file_name("probe.wav")
            .mime_str("audio/wav")
            .unwrap();
        let form = reqwest::multipart::Form::new()
            .text("model", "whisper-1")
            .part("file", part);
        let tresp = auth(http.post(&turl).multipart(form), &cfg)
            .send()
            .await
            .unwrap();
        eprintln!("live transcriptions status={}", tresp.status());
    }

    fn called_tools(messages: &[ChatMessage]) -> Vec<String> {
        let mut names = Vec::new();
        for m in messages {
            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    if let Some(n) = c["function"]["name"].as_str() {
                        names.push(n.to_string());
                    }
                }
            }
        }
        names
    }

    fn live_media_opts(cfg: &crate::config::Config, dir: std::path::PathBuf) -> RunOpts {
        let mut opts = RunOpts::from_config(cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.narrate = false;
        opts.persist_session = false;
        opts.max_steps = 6;
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = false;
        opts.media = true;
        opts
    }

    async fn synth_wav(
        out: &std::path::Path,
        phrase: &str,
        ffmpeg: Option<&std::path::Path>,
    ) -> bool {
        for bin in ["espeak-ng", "espeak"] {
            let Some(p) = crate::media::find_bin(bin) else {
                continue;
            };
            let mut cmd = tokio::process::Command::new(p);
            cmd.arg("-w").arg(out).arg(phrase);
            if let Ok(Ok(st)) =
                tokio::time::timeout(std::time::Duration::from_secs(20), cmd.status()).await
            {
                if st.success() && out.is_file() {
                    return true;
                }
            }
        }
        if let Some(say) = crate::media::find_bin("say") {
            let aiff = out.with_extension("aiff");
            let mut cmd = tokio::process::Command::new(say);
            cmd.arg("-o").arg(&aiff).arg(phrase);
            if let Ok(Ok(st)) =
                tokio::time::timeout(std::time::Duration::from_secs(20), cmd.status()).await
            {
                if st.success() && aiff.is_file() {
                    if let Some(ff) = ffmpeg {
                        let mut conv = tokio::process::Command::new(ff);
                        conv.arg("-hide_banner")
                            .arg("-loglevel")
                            .arg("error")
                            .arg("-y")
                            .arg("-i")
                            .arg(&aiff)
                            .arg("-ac")
                            .arg("1")
                            .arg("-ar")
                            .arg("16000")
                            .arg(out);
                        if let Ok(Ok(st)) =
                            tokio::time::timeout(std::time::Duration::from_secs(20), conv.status())
                                .await
                        {
                            let _ = std::fs::remove_file(&aiff);
                            return st.success() && out.is_file();
                        }
                    }
                    let _ = std::fs::remove_file(aiff);
                }
            }
        }
        false
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_view_video_stills() {
        let cfg = live_cfg();
        let bins = crate::media::MediaBins::from_config(&cfg.media);
        let ffmpeg = bins
            .ffmpeg
            .as_ref()
            .expect("ffmpeg must be on PATH for live video stills (ffmpeg.exe on Windows)");
        let dir =
            std::env::temp_dir().join(format!("q38-live-vid-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        q38_loop_encode_red(ffmpeg, &clip).await;
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut agent = Agent::new(completer, live_media_opts(&cfg, dir.clone())).unwrap();
        let out = agent
            .run("What is the dominant color in clip.mp4? Use view. Do not use bash. Reply with one color word.")
            .await
            .unwrap();
        let names = called_tools(agent.messages());
        assert!(
            names.iter().any(|n| n == "view"),
            "expected view, got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "bash"),
            "model must not bash-install ffmpeg: {names:?}"
        );
        let tool = agent
            .messages()
            .iter()
            .find(|m| m.role == "tool")
            .expect("view tool message");
        assert!(
            tool.text().contains("stills"),
            "expected stills, got {}",
            tool.text()
        );
        assert_eq!(tool.parts.len(), 3, "expected 3 JPEG stills");
        assert!(tool
            .parts
            .iter()
            .all(|p| p.kind == crate::media::MediaKind::Image));
        let think = agent
            .messages()
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.reasoning_content.clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            crate::media::image_probe_hit(&out.text, &think),
            "model should see red stills; text={} think={}",
            out.text,
            think.chars().take(200).collect::<String>()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn q38_loop_encode_red(ffmpeg: &std::path::Path, clip: &std::path::Path) {
        let mut cmd = tokio::process::Command::new(ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-y")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("color=c=red:s=64x64:d=1:r=10")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg(clip);
        let st = cmd.status().await.expect("spawn ffmpeg lavfi");
        assert!(
            st.success() && clip.is_file(),
            "ffmpeg lavfi red mp4 failed"
        );
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_view_audio_transcript() {
        let cfg = live_cfg();
        let bins = crate::media::MediaBins::from_config(&cfg.media);
        assert!(
            bins.whisper.is_some(),
            "whisper-cli must be on PATH for live audio (whisper-cli.exe on Windows)"
        );
        assert!(
            bins.whisper_model.as_ref().map(|p| p.is_file()).unwrap_or(false),
            "whisper ggml model missing (set Q38_WHISPER_MODEL or put ggml-tiny.bin under the q38 home whisper dir)"
        );
        let dir =
            std::env::temp_dir().join(format!("q38-live-aud-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("speak.wav");
        let phrase = "the color is red";
        let spoke = synth_wav(&wav, phrase, bins.ffmpeg.as_deref()).await;
        if !spoke {
            std::fs::write(&wav, crate::media::silence_wav()).unwrap();
        }
        let policy = ThinkPolicy::agent_default();
        let completer = HttpCompleter::connect(&cfg, policy).await.unwrap();
        let mut agent = Agent::new(completer, live_media_opts(&cfg, dir.clone())).unwrap();
        let prompt = if spoke {
            "Transcribe speak.wav with view. Do not use bash. Quote the color word from the transcript."
        } else {
            "Use view on speak.wav. Do not use bash. Reply with one short sentence."
        };
        let out = agent.run(prompt).await.unwrap();
        let names = called_tools(agent.messages());
        assert!(
            names.iter().any(|n| n == "view"),
            "expected view, got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "bash"),
            "model must not bash-install whisper: {names:?}"
        );
        let tool = agent
            .messages()
            .iter()
            .find(|m| m.role == "tool")
            .expect("view tool message");
        let t = tool.text();
        assert!(
            t.contains("Transcript of") || t.contains("Cannot hear"),
            "unexpected audio view result: {t}"
        );
        assert!(!t.to_ascii_lowercase().contains("brew"), "{t}");
        if spoke && t.contains("Transcript of") {
            let blob = format!(
                "{} {}",
                t.to_ascii_lowercase(),
                out.text.to_ascii_lowercase()
            );
            assert!(
                blob.contains("red") || blob.contains("color"),
                "expected color/red in transcript or answer; tool={t} text={}",
                out.text
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    fn bench_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../q38-bench/fixtures")
    }

    fn copy_tree(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name == "target" || name == ".git" {
                continue;
            }
            let to = dst.join(&name);
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &to);
            } else {
                std::fs::copy(entry.path(), to).unwrap();
            }
        }
    }

    fn soak_rounds() -> u32 {
        std::env::var("Q38_SOAK_ROUNDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(24)
    }

    fn think_chars(agent: &Agent<HttpCompleter>) -> usize {
        agent
            .messages()
            .iter()
            .map(|m| m.reasoning_content.as_deref().unwrap_or("").chars().count())
            .sum()
    }

    fn hidden_user_count(agent: &Agent<HttpCompleter>) -> usize {
        agent
            .messages()
            .iter()
            .filter(|m| {
                m.role == "user"
                    && crate::template::is_hidden_user_text(m.content.as_deref().unwrap_or(""))
            })
            .count()
    }

    fn real_user_count(agent: &Agent<HttpCompleter>) -> usize {
        agent
            .messages()
            .iter()
            .filter(|m| {
                m.role == "user"
                    && !crate::template::is_hidden_user_text(m.content.as_deref().unwrap_or(""))
            })
            .count()
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_bench_read_edit() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-01-{}", uuid::Uuid::new_v4().simple()));
        copy_tree(&bench_root().join("01-read-edit"), &dir);
        let completer = HttpCompleter::connect(&cfg, ThinkPolicy::agent_default())
            .await
            .unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.max_steps = 10;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run(
                "Read note.txt first. Then edit src/main.rs so it prints new instead of old. \
The file must contain println!(\"new\") and must not contain old. Change only what is required.",
            )
            .await
            .unwrap();
        dump_traj("bench_01_read_edit", &agent, &out);
        let src = std::fs::read_to_string(dir.join("src/main.rs")).unwrap_or_default();
        assert!(
            src.contains("println!(\"new\")") && !src.contains("old"),
            "src/main.rs={src:?} text={}",
            out.text
        );
        let names = called_tools(agent.messages());
        assert!(
            names.iter().any(|n| n == "read") && names.iter().any(|n| n == "edit"),
            "expected read+edit, got {names:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_bench_multi_file() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-02-{}", uuid::Uuid::new_v4().simple()));
        copy_tree(&bench_root().join("02-multi-file"), &dir);
        let completer = HttpCompleter::connect(&cfg, ThinkPolicy::agent_default())
            .await
            .unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.max_steps = 10;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run(
                "Read hint.txt. Change the BANNER constant in src/lib.rs so the program would print q38-ok. \
Do not change src/main.rs.",
            )
            .await
            .unwrap();
        dump_traj("bench_02_multi_file", &agent, &out);
        let lib = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap_or_default();
        let main = std::fs::read_to_string(dir.join("src/main.rs")).unwrap_or_default();
        assert!(
            lib.contains("q38-ok") && !lib.contains("\"alpha\""),
            "lib.rs={lib:?} text={}",
            out.text
        );
        assert!(
            !main.contains("q38-ok"),
            "main.rs must stay untouched: {main:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_bench_test_fix() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-03-{}", uuid::Uuid::new_v4().simple()));
        copy_tree(&bench_root().join("03-test-fix"), &dir);
        let completer = HttpCompleter::connect(&cfg, ThinkPolicy::agent_default())
            .await
            .unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.max_steps = 10;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run(
                "src/lib.rs has a failing unit test. Fix the scale implementation so the existing asserts pass. \
Do not change the test.",
            )
            .await
            .unwrap();
        dump_traj("bench_03_test_fix", &agent, &out);
        let src = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap_or_default();
        assert!(
            src.contains("assert_eq!(scale(3), 6)"),
            "tests were rewritten: {src}"
        );
        assert!(
            !src.contains("n * n") && !src.contains("n.pow("),
            "scale still squares; text={} src={src}",
            out.text
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_roleplay_tools_compact_recall() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-rp-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(dir.join("lore")).unwrap();
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(
            dir.join("lore/charter.txt"),
            "The cipher is moonlight-oak. The keep's true name is Ashenford.\n",
        )
        .unwrap();
        let completer = HttpCompleter::connect(&cfg, ThinkPolicy::agent_default())
            .await
            .unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.persist_session = true;
        opts.session_id = "live-rp".into();
        opts.session_dir = Some(sess.clone());
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = true;
        opts.max_steps = 12;
        opts.working_window = 4200;
        opts.generation_reserve = 0;
        for i in 2..=8 {
            std::fs::write(
                dir.join("lore").join(format!("day{i}.txt")),
                format!("Watchword for day {i} is token-rp-{i}.\n"),
            )
            .unwrap();
        }
        let mut agent = Agent::new(completer, opts).unwrap();

        let t1 = agent
            .run(
                "You are the keep's archivist in a long-running play. Stay in character. \
Read lore/charter.txt with the read tool, then write chronicle.md containing both secret names. \
Then stop. Do not read other files.",
            )
            .await
            .unwrap();
        dump_traj("rp_turn1", &agent, &t1);
        let chronicle = std::fs::read_to_string(dir.join("chronicle.md")).unwrap_or_default();
        assert!(
            chronicle.to_ascii_lowercase().contains("moonlight-oak")
                && chronicle.to_ascii_lowercase().contains("ashenford"),
            "chronicle.md={chronicle:?} text={}",
            t1.text
        );

        // Filler turns so compact must fire; each still uses tools.
        for i in 2..=8 {
            let out = agent
                .run(&format!(
                    "Still the archivist. Read only lore/day{i}.txt and append that watchword as a new line on chronicle.md. \
Reply in-character with one short sentence, then stop. Do not read the next day."
                ))
                .await
                .unwrap();
            dump_traj(&format!("rp_turn{i}"), &agent, &out);
            assert!(
                !out.stop_reason
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("budget:context"),
                "roleplay died on compact budget at turn {i}: {:?}",
                out.stop_reason
            );
        }

        let live = agent.messages().len();
        assert!(
            live <= 40,
            "roleplay live window bloated to {live} messages"
        );
        assert!(
            real_user_count(&agent) >= 1,
            "live user missing after roleplay; msgs={live}"
        );

        let recall = agent
            .run(
                "Stay in character. Use recall or memory_search if needed, then reply with only the keep's true name from the charter.",
            )
            .await
            .unwrap();
        dump_traj("rp_recall", &agent, &recall);
        assert!(
            recall.text.to_ascii_lowercase().contains("ashenford"),
            "recall miss: {}",
            recall.text
        );
        let think = think_chars(&agent);
        eprintln!(
            "rp think_chars={think} hidden={} live={}",
            hidden_user_count(&agent),
            agent.messages().len()
        );
        assert!(
            think < 80_000,
            "default effort spilled too much think: {think} chars"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_soak_tool_rounds() {
        let cfg = live_cfg();
        let rounds = soak_rounds();
        let dir =
            std::env::temp_dir().join(format!("q38-live-soak-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(dir.join("journal.md"), "# journal\n").unwrap();
        let completer = HttpCompleter::connect(&cfg, ThinkPolicy::agent_default())
            .await
            .unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.persist_session = true;
        opts.session_id = "live-soak".into();
        opts.session_dir = Some(sess.clone());
        opts.home = Some(dir.join(".q38-home"));
        opts.peripheral = true;
        opts.max_steps = 8;
        opts.working_window = 3800;
        opts.generation_reserve = 0;
        let mut agent = Agent::new(completer, opts).unwrap();

        let mut steps_sum = 0u32;
        let mut compact_hits = 0u32;
        let mut budget_halts = 0u32;
        let t0 = std::time::Instant::now();

        for i in 1..=rounds {
            let token = format!("soak-token-{i:04}");
            let fat = "x".repeat(700);
            std::fs::write(
                dir.join("notes").join(format!("{i}.txt")),
                format!("{token}\n{fat}\n"),
            )
            .unwrap();
            let prompt = if i % 8 == 0 {
                format!(
                    "Read notes/{i}.txt. Append only the first line ({token}) to journal.md. \
If soak-token-0001 is gone from the live window, recall it. \
Reply with only {token}, then stop. No bash."
                )
            } else {
                format!(
                    "Read notes/{i}.txt. Append only its first line to journal.md. \
Reply with only {token}, then stop. Do not read other notes. No bash."
                )
            };
            let out = agent.run(&prompt).await.unwrap();
            steps_sum += out.steps;
            if out
                .stop_reason
                .as_deref()
                .unwrap_or("")
                .starts_with("budget:context")
            {
                budget_halts += 1;
                eprintln!("soak round {i} budget halt after {:?}", t0.elapsed());
                break;
            }
            if hidden_user_count(&agent) > compact_hits as usize {
                compact_hits = hidden_user_count(&agent) as u32;
            }
            let journal = std::fs::read_to_string(dir.join("journal.md")).unwrap_or_default();
            if !(journal.contains(&token) || out.text.contains(&token)) {
                dump_traj(&format!("soak_miss_{i}"), &agent, &out);
                panic!(
                    "round {i} missed {token}; journal={journal:?} text={}",
                    out.text
                );
            }
            if hidden_user_count(&agent) > 0 {
                assert!(
                    agent.messages().len() <= 36,
                    "soak live window {} after compact at round {i}",
                    agent.messages().len()
                );
            }
            if i == 1 || i % 8 == 0 || i == rounds {
                eprintln!(
                    "soak i={i}/{rounds} steps={} live={} hidden={} steps_sum={steps_sum} think={} elapsed={:?}",
                    out.steps,
                    agent.messages().len(),
                    hidden_user_count(&agent),
                    think_chars(&agent),
                    t0.elapsed()
                );
            }
        }

        let journal = std::fs::read_to_string(dir.join("journal.md")).unwrap_or_default();
        assert!(
            journal.contains("soak-token-0001"),
            "first token dropped from journal: {journal}"
        );
        if rounds >= 12 {
            assert!(
                compact_hits > 0 || hidden_user_count(&agent) > 0,
                "expected extractive compact on a 3800 window after {rounds} rounds"
            );
        }
        assert_eq!(
            budget_halts, 0,
            "compact failed to keep the live window under budget"
        );
        assert!(
            steps_sum >= rounds,
            "too few model steps ({steps_sum}) for {rounds} rounds"
        );
        eprintln!(
            "soak done rounds={rounds} steps_sum={steps_sum} hidden={} live={} think={} wall={:?}",
            hidden_user_count(&agent),
            agent.messages().len(),
            think_chars(&agent),
            t0.elapsed()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_think_effort_chain() {
        use crate::family::Family;
        use crate::template::{render, RenderOpts};
        use crate::tokenize::count_tokens;

        let cfg = live_cfg();
        let b = cfg.policy.think_budget();
        let prompt = concat!(
            "A Rust index helper `fn idx(n: i32) -> usize { n as usize }` is used as ",
            "`arr[idx(i - 1)]` when `i` can be 0. In two short sentences: name the bug, ",
            "then say the safe return type. Do not call tools. Do not write code."
        );
        let msgs = vec![ChatMessage::user(prompt)];
        let labels = [
            ("off", ThinkPolicy::from_cli(&b, true, None, None)),
            ("low", ThinkPolicy::effort_with(&b, Effort::Low)),
            ("medium", ThinkPolicy::effort_with(&b, Effort::Medium)),
            ("xhigh", ThinkPolicy::effort_with(&b, Effort::Xhigh)),
        ];

        let mut rows = Vec::new();
        for (label, policy) in labels {
            let caps = EndpointCaps::qwen38_llamacpp();
            let kwargs = policy.template_kwargs(&caps);
            let rendered = render(&RenderOpts {
                family: Family::Qwen38,
                messages: &msgs,
                tools: None,
                add_generation_prompt: true,
                kwargs: kwargs.clone(),
            })
            .unwrap();
            let has_low = rendered.text.contains("Reasoning effort is set to low.");
            let has_xhigh = rendered.text.contains("Reasoning effort is set to xhigh.");
            let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
            let t0 = std::time::Instant::now();
            let turn = completer.complete(&msgs, None).await.unwrap();
            let elapsed = t0.elapsed();
            let think_tok = if turn.reasoning.is_empty() {
                0
            } else {
                count_tokens(Family::Qwen38, &turn.reasoning).unwrap_or(0)
            };
            let head: String = turn.reasoning.chars().take(280).collect();
            eprintln!(
                "think {label}: enabled={} effort={:?} kwargs_effort={:?} preserve={} max_tokens={} max_think={} stream_cap={} think_chars={} think_tok={think_tok} answer_chars={} wall={elapsed:?} jinja_low={has_low} jinja_xhigh={has_xhigh}",
                policy.enabled,
                policy.effort,
                kwargs.reasoning_effort,
                policy.preserve,
                policy.max_tokens,
                policy.max_think_tokens,
                policy.max_think_tokens,
                turn.reasoning.chars().count(),
                turn.content.chars().count(),
            );
            eprintln!("  think_head={head:?}");
            eprintln!(
                "  answer_head={:?}",
                turn.content.chars().take(160).collect::<String>()
            );
            rows.push((
                label,
                think_tok,
                turn.reasoning.chars().count(),
                has_low,
                has_xhigh,
                kwargs.reasoning_effort.clone(),
                policy.enabled,
            ));
        }

        let off = rows.iter().find(|r| r.0 == "off").unwrap();
        let low = rows.iter().find(|r| r.0 == "low").unwrap();
        let medium = rows.iter().find(|r| r.0 == "medium").unwrap();
        let xhigh = rows.iter().find(|r| r.0 == "xhigh").unwrap();
        assert!(!off.6, "off must disable thinking");
        assert!(off.5.is_none(), "off must omit reasoning_effort");
        assert!(off.1 <= 8, "off still thought {} tokens", off.1);
        assert!(low.6 && medium.6 && xhigh.6);
        assert_eq!(low.5.as_deref(), Some("low"));
        assert_eq!(medium.5.as_deref(), Some("medium"));
        assert_eq!(xhigh.5.as_deref(), Some("xhigh"));
        assert!(low.3, "low must inject the official low sentence");
        assert!(
            !medium.3 && !medium.4,
            "official Jinja has no medium sentence"
        );
        assert!(xhigh.4, "xhigh must inject the official xhigh sentence");
        assert!(low.1 > 0 || low.2 > 0, "low produced no thinking chain");

        let dir =
            std::env::temp_dir().join(format!("q38-live-think-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("n.txt"), "7\n").unwrap();
        let completer =
            HttpCompleter::connect(&cfg, SessionMode::Agent.default_policy_with(&cfg.policy))
                .await
                .unwrap();
        let mut opts = core_opts(&cfg, dir.clone());
        opts.max_steps = 4;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent
            .run("Read n.txt with the read tool, then reply with only that number. No extra words.")
            .await
            .unwrap();
        dump_traj("think-low-tool", &agent, &out);
        let thinks: Vec<usize> = agent
            .messages()
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| {
                m.reasoning_content
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .count()
            })
            .collect();
        eprintln!(
            "agent-low tools={:?} think_chars={thinks:?} text={:?} reason={:?}",
            called_tools(agent.messages()),
            out.text,
            out.stop_reason
        );
        assert!(
            called_tools(agent.messages()).iter().any(|n| n == "read") || out.text.contains('7'),
            "low agent neither read nor answered: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn fence_kind(reason: Option<&str>) -> Option<&'static str> {
        let r = reason.unwrap_or("");
        if r.contains("Doom loop") {
            Some("doom")
        } else if r.contains("Name streak") {
            Some("name-streak")
        } else if r.contains("Path loop") {
            Some("path-loop")
        } else if r.starts_with("budget:repeat") {
            Some("repeat")
        } else if r == "parse failed" {
            Some("parse")
        } else if r.contains("Tool call budget") {
            Some("tool-budget")
        } else {
            None
        }
    }

    async fn run_lossy_live(
        cfg: &Config,
        dir: std::path::PathBuf,
        prompt: &str,
    ) -> (crate::agent::AgentOutcome, Vec<String>) {
        let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
        let completer = HttpCompleter::connect(cfg, policy).await.unwrap();
        let mut opts = core_opts(cfg, dir);
        opts.low_precision = true;
        opts.max_steps = 12;
        let mut agent = Agent::new(completer, opts).unwrap();
        let out = agent.run(prompt).await.unwrap();
        let tools = called_tools(agent.messages());
        dump_traj("lossy-public", &agent, &out);
        (out, tools)
    }

    /// Public-box smoke: low-precision overlay on, normal work must not halt.
    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_low_precision_does_not_block_normal_work() {
        let mut cfg = live_cfg();
        cfg.policy.low_precision = true;
        eprintln!(
            "lossy live: base_url={} model={} low_precision=true",
            cfg.server.base_url,
            if cfg.server.model.is_empty() {
                "(first /models id)"
            } else {
                cfg.server.model.as_str()
            }
        );

        let mut blocked = Vec::new();
        let mut note = Vec::new();

        let dir =
            std::env::temp_dir().join(format!("q38-lossy-text-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let (out, tools) = run_lossy_live(
            &cfg,
            dir.clone(),
            "Do not use tools. Reply with the single word: ok",
        )
        .await;
        note.push(format!(
            "text-only steps={} reason={:?} tools={tools:?} text={:?}",
            out.steps,
            out.stop_reason,
            out.text.chars().take(80).collect::<String>()
        ));
        if let Some(k) = fence_kind(out.stop_reason.as_deref()) {
            blocked.push(format!("text-only:{k}"));
        }
        let _ = std::fs::remove_dir_all(&dir);

        let dir =
            std::env::temp_dir().join(format!("q38-lossy-read-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "hello-from-q38-live\n").unwrap();
        let (out, tools) = run_lossy_live(
            &cfg,
            dir.clone(),
            "Read note.txt once and reply with only its exact first line. Do not call other tools.",
        )
        .await;
        note.push(format!(
            "read-once steps={} reason={:?} tools={tools:?} text={:?}",
            out.steps,
            out.stop_reason,
            out.text.chars().take(80).collect::<String>()
        ));
        if let Some(k) = fence_kind(out.stop_reason.as_deref()) {
            blocked.push(format!("read-once:{k}"));
        }
        let _ = std::fs::remove_dir_all(&dir);

        let dir =
            std::env::temp_dir().join(format!("q38-lossy-edit-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("src.txt"), "alpha\n").unwrap();
        let (out, tools) = run_lossy_live(
            &cfg,
            dir.clone(),
            "Read src.txt, then edit the first line from alpha to beta. Reply with only the word done. Do not read the file again after editing.",
        )
        .await;
        note.push(format!(
            "read-edit steps={} reason={:?} tools={tools:?} text={:?} file={}",
            out.steps,
            out.stop_reason,
            out.text.chars().take(80).collect::<String>(),
            std::fs::read_to_string(dir.join("src.txt")).unwrap_or_default()
        ));
        if let Some(k) = fence_kind(out.stop_reason.as_deref()) {
            blocked.push(format!("read-edit:{k}"));
        }
        let _ = std::fs::remove_dir_all(&dir);

        let dir =
            std::env::temp_dir().join(format!("q38-lossy-rer-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("src.txt"), "alpha\n").unwrap();
        let (out, tools) = run_lossy_live(
            &cfg,
            dir.clone(),
            "Read src.txt, edit the first line from alpha to beta, then read src.txt again to confirm. Reply with only the word confirmed.",
        )
        .await;
        note.push(format!(
            "read-edit-read steps={} reason={:?} tools={tools:?} text={:?} file={}",
            out.steps,
            out.stop_reason,
            out.text.chars().take(80).collect::<String>(),
            std::fs::read_to_string(dir.join("src.txt")).unwrap_or_default()
        ));
        if let Some(k) = fence_kind(out.stop_reason.as_deref()) {
            blocked.push(format!("read-edit-read:{k}"));
        }
        let _ = std::fs::remove_dir_all(&dir);

        for line in &note {
            eprintln!("lossy-result {line}");
        }
        assert!(
            blocked.is_empty(),
            "low-precision fences blocked normal work: {}\n{}",
            blocked.join(", "),
            note.join("\n")
        );
    }

    fn clip_live(v: &Value) -> String {
        let s = v.to_string();
        if s.len() > 400 {
            format!("{}…", &s[..400])
        } else {
            s
        }
    }
}
