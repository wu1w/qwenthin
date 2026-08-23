//! Naked completions: where to put MEMORY.md so this 27B applies prefs
//! without a think tax on simple questions, and without stealing the user.
//!
//! Not the agent loop. Frozen `agent_tools()` + short system stay byte-stable.
//! Only the tail (or a system splice we know is the anti-pattern) changes.

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::{json, Value};

    use crate::agent::{Completer, HttpCompleter};
    use crate::config::Config;
    use crate::family::{EndpointCaps, Family};
    use crate::policy::ThinkPolicy;
    use crate::session::SessionMode;
    use crate::template::{render, wrap_tool_response, ChatMessage, RenderOpts};
    use crate::tokenize::count_tokens;
    use crate::tools_schema::agent_tools;

    const REPORT: &str = "/tmp/q38-memory-recall.json";

    const SYSTEM: &str = "工作区助手。路径相对。";
    const SYSTEM_PTR: &str =
        "工作区助手。路径相对。\nPrefs in MEMORY.md. Do not read it for local code edits.";

    const HOT: &str = "\
MEMORY hot (do not restate, do not expand):
- 回复中文；代码注释最少
- commit: conventional commits, 中文标题
- 不要改 fmt / lock，除非用户点名";

    const FULL: &str = "\
# Prefs
- 回复中文；代码注释最少
- commit: conventional commits, 中文标题
- 不要改 fmt / lock，除非用户点名
# Hosts
- ssh dev = ops@192.0.2.8
# Decisions
- 测试用 cargo test -p q38-loop";

    const SNIPPET: &str = "\
MEMORY snippet:
path=MEMORY.md
commit: conv
ssh dev = ops@";

    const PROSE: &str = "\
# MEMORY.md
请先全面理解用户的工作哲学与仓库历史。一般情况下尽量用中文回复，但视情况也可英文。
提交信息通常采用 conventional commits，标题一般用中文，除非上下文更适合英文。
格式化与 lockfile 尽量不要动，但若看起来相关也可以权衡。
开发机 SSH 可能是某台内网主机，具体以当时环境为准。";

    #[derive(Clone, Copy)]
    enum Task {
        YesNo,
        Commit,
        English,
        Host,
    }

    impl Task {
        fn id(self) -> &'static str {
            match self {
                Self::YesNo => "yesno",
                Self::Commit => "commit",
                Self::English => "english",
                Self::Host => "host",
            }
        }

        fn prompt(self) -> &'static str {
            match self {
                Self::YesNo => "2+2 等于 4 吗？只答是或否。",
                Self::Commit => {
                    "给刚才修 off-by-one 的改动写一条 git commit 标题。只要一行，不要解释。"
                }
                Self::English => "这次用英文写 commit 标题。只要一行，不要解释。",
                Self::Host => "dev 机器 SSH 用户和地址是什么？只答 user@host。",
            }
        }
    }

    #[derive(Clone, Copy)]
    enum Inject {
        None,
        SystemFull,
        SystemPtr,
        HiddenHot,
        HiddenFull,
        HiddenSnippet,
        HiddenProse,
        UserSuffix,
    }

    impl Inject {
        fn id(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::SystemFull => "system_full",
                Self::SystemPtr => "system_ptr",
                Self::HiddenHot => "hidden_hot",
                Self::HiddenFull => "hidden_full",
                Self::HiddenSnippet => "hidden_snippet",
                Self::HiddenProse => "hidden_prose",
                Self::UserSuffix => "user_suffix",
            }
        }
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

    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    fn conventional(s: &str) -> bool {
        let t = s.trim().to_ascii_lowercase();
        ["fix", "feat", "chore", "docs", "refactor", "test", "perf"]
            .iter()
            .any(|p| t.starts_with(p))
    }

    fn yesno_ok(s: &str) -> bool {
        let t = s.trim().trim_end_matches(['。', '.', '！', '!', '\n']);
        t == "是" || t == "否" || t == "Yes" || t == "No"
    }

    fn live_cfg() -> Config {
        let (mut cfg, _) = Config::load_or_init().unwrap();
        cfg.apply_env();
        cfg
    }

    fn system_for(inj: Inject) -> String {
        match inj {
            Inject::SystemFull => format!("{SYSTEM}\n\n{FULL}"),
            Inject::SystemPtr => SYSTEM_PTR.into(),
            _ => SYSTEM.into(),
        }
    }

    fn messages(inj: Inject, task: Task) -> Vec<ChatMessage> {
        let sys = ChatMessage::system(system_for(inj));
        let user = ChatMessage::user(task.prompt());
        match inj {
            Inject::None | Inject::SystemFull | Inject::SystemPtr => vec![sys, user],
            Inject::HiddenHot => vec![sys, user, ChatMessage::hidden_user(HOT)],
            Inject::HiddenFull => vec![sys, user, ChatMessage::hidden_user(FULL)],
            Inject::HiddenSnippet => vec![sys, user, ChatMessage::hidden_user(SNIPPET)],
            Inject::HiddenProse => vec![sys, user, ChatMessage::hidden_user(PROSE)],
            Inject::UserSuffix => vec![
                sys,
                ChatMessage::user(format!("{}\n\n{HOT}", task.prompt())),
            ],
        }
    }

    fn prefix_tokens(inj: Inject, task: Task, policy: &ThinkPolicy) -> u32 {
        let msgs = messages(inj, task);
        let tools = agent_tools();
        let rendered = render(&RenderOpts {
            family: Family::Qwen38,
            messages: &msgs,
            tools: Some(&tools),
            add_generation_prompt: true,
            kwargs: policy.template_kwargs(&EndpointCaps::qwen38_llamacpp()),
        })
        .unwrap();
        toks(&rendered.text)
    }

    fn score(task: Task, reply: &str) -> Value {
        match task {
            Task::YesNo => json!({
                "yesno_ok": yesno_ok(reply),
                "chars": reply.trim().chars().count(),
            }),
            Task::Commit => json!({
                "conventional": conventional(reply),
                "cjk": has_cjk(reply),
                "ok": conventional(reply) && has_cjk(reply),
            }),
            Task::English => json!({
                "conventional": conventional(reply),
                "cjk": has_cjk(reply),
                "ok": conventional(reply) && !has_cjk(reply),
            }),
            Task::Host => json!({
                "hit": reply.contains("192.0.2.8") && reply.contains("ops"),
                "partial": reply.contains("192.0.2.8") || reply.contains("ops@"),
            }),
        }
    }

    #[test]
    fn memory_blob_token_table() {
        for (name, text) in [
            ("hot", HOT),
            ("full", FULL),
            ("snippet", SNIPPET),
            ("prose", PROSE),
            ("ptr", SYSTEM_PTR),
            ("wrap_hot", &wrap_tool_response(HOT)),
        ] {
            eprintln!(
                "memory {name}: {} tok / {} chars",
                toks(text),
                text.chars().count()
            );
        }
        assert!(toks(HOT) < 80, "hot card should stay tiny");
        assert!(toks(FULL) < 160);
        assert!(toks(PROSE) > toks(HOT));
    }

    async fn run_matrix(with_tools: bool, report: &str) {
        let cfg = live_cfg();
        let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
        let completer = HttpCompleter::connect(&cfg, policy.clone()).await.unwrap();
        let tools = agent_tools();
        let tools_ref = if with_tools {
            Some(tools.as_slice())
        } else {
            None
        };
        let injects = [
            Inject::None,
            Inject::SystemFull,
            Inject::SystemPtr,
            Inject::HiddenHot,
            Inject::HiddenFull,
            Inject::HiddenSnippet,
            Inject::HiddenProse,
            Inject::UserSuffix,
        ];
        let tasks = [Task::YesNo, Task::Commit, Task::English, Task::Host];
        let mut rows = Vec::new();
        for inj in injects {
            for task in tasks {
                let msgs = messages(inj, task);
                let prefix = if with_tools {
                    prefix_tokens(inj, task, &policy)
                } else {
                    let rendered = render(&RenderOpts {
                        family: Family::Qwen38,
                        messages: &msgs,
                        tools: None,
                        add_generation_prompt: true,
                        kwargs: policy.template_kwargs(&EndpointCaps::qwen38_llamacpp()),
                    })
                    .unwrap();
                    toks(&rendered.text)
                };
                let t0 = Instant::now();
                let turn = completer.complete(&msgs, tools_ref).await.unwrap();
                let wall_ms = t0.elapsed().as_millis() as u64;
                let tools_called: Vec<String> =
                    turn.tool_calls.iter().map(|c| c.name.clone()).collect();
                let row = json!({
                    "inject": inj.id(),
                    "task": task.id(),
                    "prefix_tokens": prefix,
                    "prompt_tokens": turn.prompt_tokens,
                    "think_tokens": toks(&turn.reasoning),
                    "think_chars": turn.reasoning.chars().count(),
                    "reply_tokens": toks(&turn.content),
                    "wall_ms": wall_ms,
                    "watchdog": turn.watchdog_hit,
                    "tools": tools_called,
                    "score": score(task, &turn.content),
                    "reply": clip(turn.content.trim(), 160),
                    "think_head": clip(turn.reasoning.trim(), 180),
                });
                eprintln!(
                    "{} {}/{} prefix={} think={} wall={}ms tools={:?} reply={}",
                    if with_tools { "tools" } else { "notools" },
                    inj.id(),
                    task.id(),
                    prefix,
                    row["think_tokens"],
                    wall_ms,
                    tools_called,
                    row["reply"]
                );
                rows.push(row);
            }
        }
        let body = json!({
            "model": cfg.server.model,
            "with_tools": with_tools,
            "hot_tokens": toks(HOT),
            "full_tokens": toks(FULL),
            "rows": rows,
        });
        std::fs::write(report, serde_json::to_string_pretty(&body).unwrap()).unwrap();
        eprintln!("wrote {report}");
        assert_eq!(rows.len(), 32);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live llama.cpp reference box"]
    async fn live_memory_recall_naked() {
        run_matrix(true, REPORT).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live llama.cpp reference box"]
    async fn live_memory_recall_naked_notools() {
        run_matrix(false, "/tmp/q38-memory-recall-notools.json").await;
    }
}
