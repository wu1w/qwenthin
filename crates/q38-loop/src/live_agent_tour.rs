//! One live Agent session covering the harness surface we just landed:
//! frozen tools, MEMORY hot card, skills, MCP mount/call, follow-ups,
//! cancel, steer, queue, and extractive compact with tool results in the window.
//!
//! Gated (`cargo test -- --ignored --exact live_agent_usage_tour`).

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::agent::{Agent, AgentOutcome, HttpCompleter, RunOpts};
    use crate::channel::{push_steer, BusyDecision, BusyPolicy, Mailbox};
    use crate::config::Config;
    use crate::media::PROBE_IMAGE_B64;
    use crate::memory::MemoryStore;
    use crate::session::{SessionEvent, SessionLog, SessionMode};
    use crate::template::is_hidden_user_text;
    use crate::tool_calls::CancelFlag;

    const MCP_PY: &str = r#"
import json, sys
CORPUS = [
    "Lantern is a 262k-context coding agent. Project id lantern-core-9f3a.",
    "UI status chip in mocks is solid red. Palette token is ember-red.",
]
def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise EOFError
        if line in (b"\r\n", b"\n"):
            break
        k, _, v = line.decode().partition(":")
        headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    buf = b""
    while len(buf) < n:
        chunk = sys.stdin.buffer.read(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return json.loads(buf)
def write_msg(obj):
    raw = json.dumps(obj).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(raw)}\r\n\r\n".encode() + raw)
    sys.stdout.buffer.flush()
def search(q):
    q = (q or "").lower()
    hits = [c for c in CORPUS if q in c.lower() or not q]
    return hits[:4] or ["no hits"]
while True:
    try:
        msg = read_msg()
    except EOFError:
        break
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"docs","version":"0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"search","description":"Keyword search"},{"name":"lookup","description":"Lookup"}]}})
    elif method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        q = str(args.get("query") or args.get("q") or "")
        text = "\n".join(search(q)) if name in ("search","lookup") else "unknown tool"
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":text}]}})
"#;

    const EMIT_PY: &str = r#"
import argparse, pathlib
p = argparse.ArgumentParser()
p.add_argument("n", type=int)
p.add_argument("count", type=int)
args = p.parse_args()
root = pathlib.Path("src/gen")
root.mkdir(parents=True, exist_ok=True)
out = root / f"pack_{args.n}.rs"
lines = [f"//! generated pack {args.n}", ""]
for i in range(args.count):
    lines.append(f"pub fn lantern_p{args.n}_f{i}() -> u32 {{ {args.n} * 10000 + {i} }}")
out.write_text("\n".join(lines) + "\n")
mod_rs = root / "mod.rs"
body = mod_rs.read_text() if mod_rs.exists() else ""
decl = f"pub mod pack_{args.n};"
if decl not in body:
    mod_rs.write_text(body + decl + "\n")
print(f"wrote {out} lines={len(lines)}")
"#;

    #[derive(Default)]
    struct Report {
        steps: Vec<serde_json::Value>,
        tools: Vec<String>,
        compact: usize,
        aborted: usize,
        steer_notes: usize,
        skill_modgen: bool,
        skill_testhook: bool,
        skill_commit: bool,
        memory_on_commit: bool,
        memory_on_yesno: bool,
        mcp_card: bool,
        mcp_called: bool,
        queued_ran: bool,
        followup_ok: bool,
        view_called: bool,
    }

    fn live_cfg() -> Config {
        let (mut cfg, _) = Config::load_or_init().unwrap();
        cfg.apply_env();
        cfg
    }

    fn hidden_blob(agent: &Agent<HttpCompleter>) -> String {
        agent
            .messages()
            .iter()
            .filter(|m| m.role == "user" && is_hidden_user_text(m.content.as_deref().unwrap_or("")))
            .map(|m| m.content.clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_names(events: &[SessionEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Tool(t) => Some(t.name.clone()),
                _ => None,
            })
            .collect()
    }

    fn compact_n(events: &[SessionEvent]) -> usize {
        events
            .iter()
            .filter(|e| e.type_name() == "session/compact")
            .count()
    }

    fn setup(dir: &Path) -> PathBuf {
        let home = dir.join(".q38-home");
        for p in [
            home.join("skills/modgen"),
            home.join("skills/testhook"),
            home.join("skills/commit"),
            dir.join("scripts"),
            dir.join("src/gen"),
            dir.join("docs"),
            dir.join("assets"),
            dir.join(".q38"),
        ] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(
            home.join("MEMORY.md"),
            "# Prefs\n- 回复中文\n- commit: conv\n\n# Hosts\n- ssh = ops@192.0.2.8\n",
        )
        .unwrap();
        std::fs::write(
            home.join("skills/modgen/SKILL.md"),
            "---\nname: modgen\ndescription: Emit a numbered Rust pack under src/gen\n---\n1. Run `python3 scripts/emit.py N COUNT` (COUNT default 40).\n2. Stop after the files are on disk.\n",
        )
        .unwrap();
        std::fs::write(
            home.join("skills/testhook/SKILL.md"),
            "---\nname: testhook\n---\nRerun the failing file only. Do not cargo test the whole crate.\n",
        )
        .unwrap();
        std::fs::write(
            home.join("skills/commit/SKILL.md"),
            "---\nname: commit\n---\nConventional commit. One line. Language from MEMORY prefs if present.\n",
        )
        .unwrap();
        std::fs::write(dir.join("scripts/emit.py"), EMIT_PY).unwrap();
        std::fs::write(dir.join("scripts/docs_mcp.py"), MCP_PY).unwrap();
        std::fs::write(
            dir.join(".q38").join("mcp.toml"),
            format!(
                "[[servers]]\nname=\"docs\"\ncommand=\"python3\"\nargs=[\"{}\"]\ncwd=\"{}\"\ndescription=\"Lantern project docs\"\nmethods=[\"search\",\"lookup\"]\n",
                dir.join("scripts/docs_mcp.py").display(),
                dir.display()
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"lantern\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub mod gen;\n").unwrap();
        std::fs::write(dir.join("src/gen/mod.rs"), "// generated\n").unwrap();
        std::fs::write(dir.join("NOTE.md"), "hello\n").unwrap();
        let fat = format!("TOUR-FAT\n{}\n", "block ".repeat(2500));
        std::fs::write(dir.join("fat.txt"), fat).unwrap();
        let png =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, PROBE_IMAGE_B64)
                .unwrap();
        std::fs::write(dir.join("assets/red.png"), png).unwrap();
        let store = MemoryStore::open(&home).unwrap();
        let _ = store.write_compact_note("tour", 1, "lantern-core-9f3a linker rewrite");
        home
    }

    fn log_step(rep: &mut Report, name: &str, out: &AgentOutcome, extra: serde_json::Value) {
        eprintln!(
            "tour {name} steps={} stop={:?} text={}",
            out.steps,
            out.stop_reason,
            out.text.chars().take(160).collect::<String>()
        );
        rep.steps.push(json!({
            "name": name,
            "steps": out.steps,
            "stop": out.stop_reason,
            "text": out.text.chars().take(240).collect::<String>(),
            "extra": extra,
        }));
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_agent_usage_tour() {
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-tour-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        let home = setup(&dir);

        let completer =
            HttpCompleter::connect(&cfg, SessionMode::Agent.default_policy_with(&cfg.policy))
                .await
                .unwrap();
        eprintln!("tour model={}", completer.model());

        let mut opts = RunOpts::from_config(&cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.persist_session = true;
        opts.session_id = "live-tour".into();
        opts.session_dir = Some(sess.clone());
        opts.home = Some(home);
        opts.peripheral = true;
        opts.skills_auto_catalog = false;
        opts.mcp_auto_catalog = false;
        opts.media = true;
        opts.max_steps = 8;
        opts.working_window = 3_200;
        opts.generation_reserve = 0;
        let mut agent = Agent::new(completer, opts).unwrap();
        assert!(
            crate::tools_schema::has_tool(agent.tools(), "mcp"),
            "mcp should mount from .q38/mcp.toml"
        );
        let sys = agent.messages()[0].content.clone().unwrap_or_default();
        assert!(
            !sys.contains("mcp:"),
            "catalog must stay out of system: {sys}"
        );
        assert!(!sys.contains("modgen"), "{sys}");

        let mut rep = Report::default();
        let t0 = Instant::now();
        let mut mailbox = Mailbox::default();

        // 1–2 frozen write/edit
        let out = agent
            .run("Write TOUR.md with exactly one line: tour-write-ok. Then stop. No bash.")
            .await
            .unwrap();
        log_step(&mut rep, "write", &out, json!({}));

        let out = agent
            .run("Edit TOUR.md so the line is tour-edit-ok instead. Then stop. No bash.")
            .await
            .unwrap();
        log_step(&mut rep, "edit", &out, json!({}));

        // 3 yes/no: must not attach MEMORY
        let out = agent
            .run("这个函数 off-by-one 吗？只答是或否。")
            .await
            .unwrap();
        let blob = hidden_blob(&agent);
        rep.memory_on_yesno = blob.contains("MEMORY");
        let leaked = rep.memory_on_yesno;
        log_step(&mut rep, "yesno", &out, json!({"memory_leaked": leaked}));

        // 4 testhook via FAILED — no other skill is live yet
        let out = agent
            .run("Run bash: python3 -c \"print('FAILED in foo.rs')\". Then stop. Do not fix anything.")
            .await
            .unwrap();
        let blob = hidden_blob(&agent);
        rep.skill_testhook = blob.contains("Rerun the failing file");
        let inj = rep.skill_testhook;
        log_step(&mut rep, "testhook", &out, json!({"injected": inj}));

        // 5 bash (1st later user after testhook)
        let out = agent
            .run("Run `echo tour-bash-ok` via bash. Then stop. Do not write files.")
            .await
            .unwrap();
        log_step(&mut rep, "bash", &out, json!({}));

        // 6 MCP (2nd later user — testhook stubs here)
        let out = agent
            .run(
                "Use mcp with server docs and method search, args query lantern-core. \
Quote the project id token in the final sentence. Then stop. Do not bash.",
            )
            .await
            .unwrap();
        let blob = hidden_blob(&agent);
        rep.mcp_card = blob.contains("[mcp: docs]") || blob.contains("[mcp:docs]");
        let card = rep.mcp_card;
        log_step(
            &mut rep,
            "mcp",
            &out,
            json!({"card": card, "text": out.text}),
        );

        // 7 追问
        let out = agent
            .run("只要项目 id 那个 token，别的一句都不要。")
            .await
            .unwrap();
        rep.followup_ok = out.text.to_ascii_lowercase().contains("lantern-core");
        let ok = rep.followup_ok;
        log_step(&mut rep, "followup", &out, json!({"ok": ok}));

        // 8 commit: MEMORY hot + commit skill (testhook already stubbed)
        let out = agent.run("写一条 commit 标题，只要一行。").await.unwrap();
        let blob = hidden_blob(&agent);
        rep.memory_on_commit = blob.contains("MEMORY hot") && blob.contains("回复中文");
        rep.skill_commit = blob.contains("[skill: commit]") || blob.contains("[skill:commit]");
        let mem = rep.memory_on_commit;
        let sk = rep.skill_commit;
        log_step(
            &mut rep,
            "commit",
            &out,
            json!({"memory_hot": mem, "commit_skill": sk}),
        );

        // two later users so commit skill stubs before modgen
        let out = agent.run("只回 ok。不要工具。").await.unwrap();
        log_step(&mut rep, "stub1", &out, json!({}));
        let out = agent.run("只回 ok2。不要工具。").await.unwrap();
        log_step(&mut rep, "stub2", &out, json!({}));

        // 9 modgen skill
        let out = agent
            .run("[skill:modgen]\nrun python3 scripts/emit.py 7 40 so pack_7 exists. Then stop.")
            .await
            .unwrap();
        let blob = hidden_blob(&agent);
        rep.skill_modgen = blob.contains("[skill: modgen]") || blob.contains("emit.py");
        let pack = dir.join("src/gen/pack_7.rs");
        let inj = rep.skill_modgen;
        log_step(
            &mut rep,
            "modgen",
            &out,
            json!({"injected": inj, "pack_exists": pack.is_file()}),
        );

        // 10 steer during a sleep+write turn
        let steer = mailbox.steer_slot();
        agent.set_steer(steer.clone());
        let steer_h = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            push_steer(
                &steer,
                "write STEER.txt containing only STEEROK, then stop".into(),
            );
        });
        let out = agent
            .run(
                "Run `sleep 6` via bash. After it finishes, write SLEEP.txt with slept. Then stop.",
            )
            .await
            .unwrap();
        let _ = steer_h.await;
        let blob = hidden_blob(&agent);
        let steer_hit = blob.contains("Steer:") || dir.join("STEER.txt").is_file();
        if steer_hit {
            rep.steer_notes += 1;
        }
        agent.set_steer(Arc::new(std::sync::Mutex::new(Vec::new())));
        log_step(&mut rep, "steer", &out, json!({"steer": steer_hit}));

        // 11–12 repeated stop (flag must stick even while HTTP has no waiter)
        for i in 1..=2 {
            let cancel = CancelFlag::new();
            agent.set_cancel(cancel.clone());
            let h = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1200)).await;
                cancel.cancel();
            });
            let t_cancel = Instant::now();
            let out = agent
                .run("Run `sleep 25` via bash and wait until it finishes. Do not stop early.")
                .await
                .unwrap();
            let _ = h.await;
            let aborted = out.stop_reason.as_deref() == Some("aborted")
                || t_cancel.elapsed() < Duration::from_secs(12);
            if aborted {
                rep.aborted += 1;
            }
            log_step(
                &mut rep,
                &format!("cancel{i}"),
                &out,
                json!({
                    "aborted": out.stop_reason.as_deref() == Some("aborted"),
                    "elapsed_ms": t_cancel.elapsed().as_millis() as u64,
                }),
            );
            agent.set_cancel(CancelFlag::new());
        }

        // queue: hold two prompts, run a short turn, drain
        mailbox.busy = BusyPolicy::Queue;
        assert_eq!(
            mailbox.offer_while_busy("Write QUEUE.txt containing only QUEUEOK. Then stop.".into()),
            BusyDecision::Queued
        );
        assert_eq!(
            mailbox.offer_while_busy("Append QUEUE2 to QUEUE.txt. Then stop.".into()),
            BusyDecision::Queued
        );
        let out = agent
            .run("Reply with only queued-idle. No tools.")
            .await
            .unwrap();
        log_step(&mut rep, "queue-idle", &out, json!({"n": mailbox.queued()}));
        let mut n = 0u32;
        while let Some(q) = mailbox.pop_queue() {
            n += 1;
            let out = agent.run(&q).await.unwrap();
            log_step(&mut rep, &format!("queue-{n}"), &out, json!({}));
        }
        let qbody = std::fs::read_to_string(dir.join("QUEUE.txt")).unwrap_or_default();
        rep.queued_ran = qbody.contains("QUEUEOK") || qbody.contains("QUEUE2");
        if !rep.queued_ran {
            // model may have written a different path; accept tool write of the prompt text
            let log = SessionLog::open_in(&sess, "live-tour").unwrap();
            let dump: String = log
                .events()
                .iter()
                .filter_map(|e| match e {
                    SessionEvent::Tool(t) => Some(t.output.as_str()),
                    SessionEvent::Assistant(a) => Some(a.content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            rep.queued_ran = dump.contains("QUEUEOK");
        }

        // interrupt redirect after a cancelled sleep (already cancelled; just run redirect)
        mailbox.busy = BusyPolicy::Interrupt;
        assert_eq!(
            mailbox.offer_while_busy("Write REDIR.txt containing only REDIROK. Then stop.".into()),
            BusyDecision::AbortThenRedirect
        );
        if let Some(p) = mailbox.take_redirect() {
            let out = agent.run(&p).await.unwrap();
            log_step(&mut rep, "interrupt-redirect", &out, json!({}));
        }

        // view
        let out = agent
            .run("What color is assets/red.png? Use the view tool. Reply with one word, then stop.")
            .await
            .unwrap();
        log_step(&mut rep, "view", &out, json!({}));

        // fat read + oversized bash so extractive compact must fire with tool results live
        let out = agent
            .run(
                "Read fat.txt (the whole file). Then via bash run python3 -c \"print('W'*8000)\" \
twice in separate tool calls. Append only the first line of fat.txt to journal.md. Then stop.",
            )
            .await
            .unwrap();
        log_step(&mut rep, "fat-read", &out, json!({}));
        let out = agent
            .run(
                "Read fat.txt again. Then read TOUR.md. Reply with the first line of each, then stop.",
            )
            .await
            .unwrap();
        log_step(&mut rep, "fat-read-2", &out, json!({}));

        // memory_search after compact/daily note
        let out = agent
            .run("Call memory_search with query lantern-core. Quote any hit, then stop. Do not bash.")
            .await
            .unwrap();
        log_step(&mut rep, "memory_search", &out, json!({}));

        let log = SessionLog::open_in(&sess, "live-tour").unwrap();
        rep.tools = tool_names(log.events());
        rep.compact = compact_n(log.events());
        if rep.compact == 0 {
            let archived = agent.messages().iter().any(|m| {
                m.role == "user" && m.content.as_deref().unwrap_or("").contains("[archived]")
            });
            if archived {
                rep.compact = 1;
            }
        }
        rep.view_called = rep.tools.iter().any(|n| n == "view");
        rep.mcp_called = rep.tools.iter().any(|n| n == "mcp");

        let tour = json!({
            "elapsed_s": t0.elapsed().as_secs_f64(),
            "live": agent.messages().len(),
            "compact": rep.compact,
            "aborted": rep.aborted,
            "steer": rep.steer_notes,
            "tools": rep.tools,
            "skill_modgen": rep.skill_modgen,
            "skill_testhook": rep.skill_testhook,
            "skill_commit": rep.skill_commit,
            "memory_on_commit": rep.memory_on_commit,
            "memory_on_yesno": rep.memory_on_yesno,
            "mcp_card": rep.mcp_card,
            "mcp_called": rep.mcp_called,
            "queued_ran": rep.queued_ran,
            "followup_ok": rep.followup_ok,
            "view_called": rep.view_called,
            "tour_md": std::fs::read_to_string(dir.join("TOUR.md")).unwrap_or_default(),
            "pack7": pack.is_file(),
            "steps": rep.steps,
        });
        let report_path = PathBuf::from("/tmp/q38-agent-tour.json");
        std::fs::write(&report_path, serde_json::to_string_pretty(&tour).unwrap()).unwrap();
        eprintln!(
            "tour report {} elapsed={:?}",
            report_path.display(),
            t0.elapsed()
        );
        eprintln!("{}", serde_json::to_string_pretty(&tour).unwrap());

        let mut gaps = Vec::new();
        if !rep.tools.iter().any(|n| n == "write") {
            gaps.push("write");
        }
        if !rep.tools.iter().any(|n| n == "edit") {
            gaps.push("edit");
        }
        if !rep.tools.iter().any(|n| n == "bash") {
            gaps.push("bash");
        }
        if !rep.tools.iter().any(|n| n == "read") && !rep.tools.iter().any(|n| n == "view") {
            gaps.push("read/view");
        }
        if !rep.mcp_called {
            gaps.push("mcp-call");
        }
        if !rep.mcp_card {
            gaps.push("mcp-card");
        }
        if !rep.skill_modgen {
            gaps.push("skill-modgen");
        }
        if !rep.skill_testhook {
            gaps.push("skill-testhook");
        }
        if !rep.memory_on_commit {
            gaps.push("memory-hot");
        }
        if rep.memory_on_yesno {
            gaps.push("memory-leaked-on-yesno");
        }
        if rep.aborted < 2 {
            gaps.push("cancel-x2");
        }
        if rep.steer_notes == 0 {
            gaps.push("steer");
        }
        if !rep.queued_ran {
            gaps.push("queue");
        }
        if rep.compact == 0 {
            gaps.push("compact");
        }

        assert!(
            gaps.is_empty(),
            "tour gaps {gaps:?} report={}",
            report_path.display()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
