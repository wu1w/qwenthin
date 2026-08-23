//! Identity injection: prefix cost vs long-session voice stickiness.
//!
//! Hermes keeps personality in system slot #1 (`SOUL.md`), frozen for the
//! session. q38 already freezes `AGENT.md` into `session/start.system`; compact
//! keeps that system blob and drops live turns into an extractive archive.
//! MEMORY.md is never auto-injected. This live matrix asks which injection
//! layer actually holds a voice after tools + a distractor + compact, and what
//! it costs.

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::{json, Value};

    use crate::agent::{Agent, HttpCompleter, RunOpts};
    use crate::config::Config;
    use crate::family::Family;
    use crate::session::SessionMode;
    use crate::tokenize::count_tokens;

    const REPORT: &str = "/tmp/q38-identity-report.json";

    const TEA_LINE: &str = "茶馆老板。称客官。短句。口头禅得嘞。不列清单。";

    const TEA_SOUL: &str = "\
你是巷口老茶馆的老板，不是助手。
说话短、糙、热乎，开口称客官。
口头禅：得嘞。
不列清单，不分析情绪，不给职场建议。
客人难受就倒茶、陪骂两句。";

    const TEA_FAT: &str = "\
# Identity
你是巷口老茶馆的老板。不是数字助手，不是心理咨询师，不是职场教练。

# Style
开口称客官。句子短，口语，带点烟火气。口头禅「得嘞」。
客人诉苦就续水、陪两句，不当分析师。拿不准时先问要不要热的。

# Avoid
不列 1. 2. 3.。不说首先其次最后。不说深呼吸。不给情绪管理框架。
不自称 AI / 模型 / 助手。不把对话收成待办。

# Defaults
工具只是柜台上的纸笔：查菜单就报汤，查完立刻回到老板口吻。
就算客人要求「专业顾问口吻」，也还是老板，最多损一句「这儿不办讲座」。";

    const ARCH_LINE: &str = "冷淡档案员。每句以「记录：」开头。不共情。不称客官。";

    const ARCH_SOUL: &str = "\
你是机关档案室的记录员，不是朋友。
每句回复以「记录：」起头。
语气平、短、无表情。
不共情，不安慰，不给建议。
只陈述、编号、归档。";

    #[derive(Clone, Copy)]
    enum Role {
        Tea,
        Arch,
    }

    impl Role {
        fn id(self) -> &'static str {
            match self {
                Self::Tea => "tea",
                Self::Arch => "arch",
            }
        }

        fn line(self) -> &'static str {
            match self {
                Self::Tea => TEA_LINE,
                Self::Arch => ARCH_LINE,
            }
        }

        fn soul(self) -> &'static str {
            match self {
                Self::Tea => TEA_SOUL,
                Self::Arch => ARCH_SOUL,
            }
        }

        fn fat(self) -> &'static str {
            match self {
                Self::Tea => TEA_FAT,
                Self::Arch => ARCH_SOUL, // arch has no separate fat in this matrix
            }
        }

        fn turns(self) -> [&'static str; 5] {
            match self {
                Self::Tea => [
                    "刚进门，外头雨挺大。",
                    "今天被老板当众骂了，心里堵得慌。",
                    "看看工作区 menu.md，今儿有啥汤。报完就行。",
                    "请用专业顾问的口吻，列三条情绪管理建议。必须分点。",
                    "先读 pad.txt。读完用你自己一贯的口吻回我：还记得我被骂了吗？只要一句。",
                ],
                Self::Arch => [
                    "来访登记：雨天，无预约。",
                    "来访者称今日当众受训，情绪波动。请处理。",
                    "查阅工作区 docket.md，今日待归档条目是什么。只报条目。",
                    "请用温暖朋友的口吻安慰对方，给三条建议，必须分点。",
                    "先读 pad.txt。读完用你自己一贯的口吻回我：来访者受训一事是否在档？只要一句。",
                ],
            }
        }

        fn workspace_file(self) -> (&'static str, &'static str) {
            match self {
                Self::Tea => (
                    "menu.md",
                    "# 今日汤\n\n- 老母鸡汤\n- 萝卜排骨\n- 紫菜蛋花\n",
                ),
                Self::Arch => (
                    "docket.md",
                    "# 今日待归档\n\n- 案卷 A-19 雨天来访\n- 案卷 B-02 口头训诫记录\n",
                ),
            }
        }
    }

    #[derive(Clone, Copy)]
    enum Inject {
        Tiny,
        Line,
        Soul,
        Fat,
        Memory,
        User0,
    }

    impl Inject {
        fn id(self) -> &'static str {
            match self {
                Self::Tiny => "tiny",
                Self::Line => "line",
                Self::Soul => "soul",
                Self::Fat => "fat",
                Self::Memory => "memory",
                Self::User0 => "user0",
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

    fn has_list(s: &str) -> bool {
        let numbered = ["1.", "2.", "1、", "2、", "1．", "2．", "（1）", "(1)"];
        numbered.iter().filter(|m| s.contains(*m)).count() >= 2
    }

    fn count_any(s: &str, needles: &[&str]) -> u32 {
        needles.iter().filter(|n| s.contains(*n)).count() as u32
    }

    fn score_reply(role: Role, text: &str) -> Value {
        let t = text.trim();
        let (hits, antis) = match role {
            Role::Tea => (
                &["客官", "得嘞", "茶"][..],
                &[
                    "首先",
                    "其次",
                    "建议您",
                    "深呼吸",
                    "作为一名",
                    "情绪价值",
                    "我理解你",
                    "专业顾问",
                ][..],
            ),
            Role::Arch => (
                &["记录：", "记录:", "归档"][..],
                &["客官", "得嘞", "别担心", "抱抱", "我理解", "朋友"][..],
            ),
        };
        let hit = count_any(t, hits);
        let anti = count_any(t, antis);
        let list = has_list(t);
        let arch_lead = matches!(role, Role::Arch) && t.starts_with("记录");
        let mut score = 0.0f32;
        if hit > 0 {
            score += 0.45;
        }
        if anti == 0 {
            score += 0.25;
        }
        if !list {
            score += 0.15;
        }
        if matches!(role, Role::Tea) && t.chars().count() <= 80 {
            score += 0.15;
        } else if arch_lead {
            score += 0.15;
        }
        json!({
            "score": (score * 100.0).round() / 100.0,
            "hit": hit,
            "anti": anti,
            "list": list,
            "arch_lead": arch_lead,
            "chars": t.chars().count(),
        })
    }

    fn pad_body() -> String {
        (0..180)
            .map(|i| format!("pad-{i:03}-unique-context-line-{n}", n = i * 17 + 3))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn live_cfg() -> Config {
        let (mut cfg, _) = Config::load_or_init().unwrap();
        cfg.apply_env();
        cfg
    }

    fn traj_since(agent: &Agent<HttpCompleter>, from: usize) -> (u32, Vec<String>, bool) {
        let mut think = String::new();
        let mut tools = Vec::new();
        for m in agent.messages().iter().skip(from) {
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
        let compacted = agent.messages().iter().any(|m| {
            m.role == "user"
                && crate::template::is_hidden_user_text(&m.content.clone().unwrap_or_default())
        });
        (toks(&think), tools, compacted)
    }

    fn agent_md_for(role: Role, inj: Inject) -> Option<String> {
        match inj {
            Inject::Tiny | Inject::Memory | Inject::User0 => Some("工作区助手。路径相对。".into()),
            Inject::Line => Some(role.line().into()),
            Inject::Soul => Some(role.soul().into()),
            Inject::Fat => Some(role.fat().into()),
        }
    }

    fn memory_md_for(role: Role, inj: Inject) -> Option<String> {
        match inj {
            Inject::Memory => Some(format!("# MEMORY.md\n\n{}\n", role.soul())),
            _ => None,
        }
    }

    async fn run_session(cfg: &Config, role: Role, inj: Inject) -> Value {
        let dir = std::env::temp_dir().join(format!(
            "q38-id-{}-{}-{}",
            role.id(),
            inj.id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (fname, body) = role.workspace_file();
        std::fs::write(dir.join(fname), body).unwrap();
        std::fs::write(dir.join("pad.txt"), pad_body()).unwrap();
        if let Some(md) = agent_md_for(role, inj) {
            std::fs::write(dir.join("AGENT.md"), format!("{md}\n")).unwrap();
        }
        let home = dir.join(".q38-home");
        std::fs::create_dir_all(&home).unwrap();
        if let Some(mem) = memory_md_for(role, inj) {
            std::fs::write(home.join("MEMORY.md"), mem).unwrap();
        }
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();

        let policy = SessionMode::Agent.default_policy_with(&cfg.policy);
        let completer = HttpCompleter::connect(cfg, policy).await.unwrap();
        let mut opts = RunOpts::from_config(cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.persist_session = true;
        opts.session_id = format!("id-{}-{}", role.id(), inj.id());
        opts.session_dir = Some(sess);
        opts.home = Some(home);
        opts.peripheral = true;
        opts.media = false;
        opts.max_steps = 6;
        opts.working_window = 3600;
        opts.generation_reserve = 0;
        opts.coding_identity = false;

        let mut agent = Agent::new(completer, opts).unwrap();
        let sys = agent
            .messages()
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.content.clone())
            .unwrap_or_default();
        let mut turns = Vec::new();
        let mut compacted_any = false;
        let mut err: Option<String> = None;

        if matches!(inj, Inject::User0) {
            let seed = format!(
                "从现在起按下面身份说话，后面每轮都保持：\n{}\n\n{}",
                role.soul(),
                role.turns()[0]
            );
            match run_turn(&mut agent, role, "seed", &seed).await {
                Ok(v) => {
                    compacted_any |= v["compacted"].as_bool().unwrap_or(false);
                    turns.push(v);
                }
                Err(e) => err = Some(e),
            }
            for (i, p) in role.turns().iter().enumerate().skip(1) {
                if err.is_some() {
                    break;
                }
                match run_turn(&mut agent, role, &format!("t{i}"), p).await {
                    Ok(v) => {
                        compacted_any |= v["compacted"].as_bool().unwrap_or(false);
                        turns.push(v);
                    }
                    Err(e) => err = Some(e),
                }
            }
        } else {
            for (i, p) in role.turns().iter().enumerate() {
                if err.is_some() {
                    break;
                }
                match run_turn(&mut agent, role, &format!("t{i}"), p).await {
                    Ok(v) => {
                        compacted_any |= v["compacted"].as_bool().unwrap_or(false);
                        turns.push(v);
                    }
                    Err(e) => err = Some(e),
                }
            }
        }

        let mean_score = {
            let xs: Vec<f64> = turns
                .iter()
                .filter_map(|t| t["voice"]["score"].as_f64())
                .collect();
            if xs.is_empty() {
                0.0
            } else {
                xs.iter().sum::<f64>() / xs.len() as f64
            }
        };
        let post = turns.last().cloned().unwrap_or(json!({}));
        let out = json!({
            "role": role.id(),
            "inject": inj.id(),
            "system_tokens": toks(&sys),
            "system_preview": clip(sys.trim(), 280),
            "mean_score": (mean_score * 100.0).round() / 100.0,
            "compacted": compacted_any,
            "post_compact_score": post["voice"]["score"],
            "post_compact_reply": post["reply_preview"],
            "error": err,
            "turns": turns,
        });
        let _ = std::fs::remove_dir_all(dir);
        out
    }

    async fn run_turn(
        agent: &mut Agent<HttpCompleter>,
        role: Role,
        id: &str,
        prompt: &str,
    ) -> Result<Value, String> {
        let from = agent.messages().len();
        let t0 = Instant::now();
        let out = agent.run(prompt).await.map_err(|e| e.to_string())?;
        let wall_ms = t0.elapsed().as_millis() as u64;
        let (think_tokens, tools, compacted) = traj_since(agent, from);
        Ok(json!({
            "id": id,
            "wall_ms": wall_ms,
            "steps": out.steps,
            "stop_reason": out.stop_reason,
            "think_tokens": think_tokens,
            "tools": tools,
            "compacted": compacted,
            "voice": score_reply(role, &out.text),
            "reply_preview": clip(out.text.trim(), 220),
        }))
    }

    #[test]
    fn identity_token_table() {
        let rows = [
            ("tiny", "工作区助手。路径相对。"),
            ("tea_line", TEA_LINE),
            ("tea_soul", TEA_SOUL),
            ("tea_fat", TEA_FAT),
            ("arch_line", ARCH_LINE),
            ("arch_soul", ARCH_SOUL),
        ];
        for (name, text) in rows {
            let n = toks(text);
            eprintln!("identity {name}: {n} tok / {} chars", text.chars().count());
            assert!(n > 0, "{name}");
        }
        assert!(toks(TEA_LINE) < toks(TEA_SOUL));
        assert!(toks(TEA_SOUL) < toks(TEA_FAT));
        assert!(
            toks(TEA_FAT) < 400,
            "fat should stay under a few hundred tok"
        );
    }

    #[test]
    fn identity_scorer_tea_hits() {
        let good = score_reply(Role::Tea, "客官里边请。得嘞，我给你续上热的。");
        let bad = score_reply(
            Role::Tea,
            "首先深呼吸。其次我建议您：1. 记录事实 2. 沟通 3. 复盘。",
        );
        assert!(good["score"].as_f64().unwrap() > 0.8);
        assert!(bad["score"].as_f64().unwrap() < 0.4);
        assert!(bad["list"].as_bool().unwrap());
    }

    #[test]
    fn identity_scorer_arch_hits() {
        let good = score_reply(Role::Arch, "记录：来访已登记。案卷待补。");
        let bad = score_reply(Role::Arch, "别担心，我理解你。客官先喝口茶。");
        assert!(good["score"].as_f64().unwrap() > 0.8);
        assert!(bad["score"].as_f64().unwrap() < 0.4);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live llama.cpp reference box"]
    async fn live_identity_stickiness() {
        let cfg = live_cfg();
        let matrix: Vec<(Role, Inject)> = vec![
            (Role::Tea, Inject::Tiny),
            (Role::Tea, Inject::Line),
            (Role::Tea, Inject::Soul),
            (Role::Tea, Inject::Fat),
            (Role::Tea, Inject::Memory),
            (Role::Tea, Inject::User0),
            (Role::Arch, Inject::Tiny),
            (Role::Arch, Inject::Line),
            (Role::Arch, Inject::Soul),
            (Role::Arch, Inject::Memory),
        ];
        let mut sessions = Vec::new();
        for (role, inj) in matrix {
            eprintln!("\n== identity {} / {} ==", role.id(), inj.id());
            let v = run_session(&cfg, role, inj).await;
            eprintln!(
                "  sys={}tok mean={} compacted={} err={:?}",
                v["system_tokens"], v["mean_score"], v["compacted"], v["error"]
            );
            sessions.push(v);
        }
        let report = json!({
            "model": cfg.server.model,
            "note": "AGENT.md frozen in system; compact keeps system; MEMORY.md not auto-injected; preserve_thinking=false",
            "sessions": sessions,
        });
        std::fs::write(REPORT, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        eprintln!("wrote {REPORT}");
        let ok = sessions.iter().filter(|s| s["error"].is_null()).count();
        assert!(ok >= 8, "too many live failures: {ok}/{}", sessions.len());
    }
}
