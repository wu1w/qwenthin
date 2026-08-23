//! Mixed 300-turn live smoke: chat, design, docs, ~30k-line codegen,
//! user-sent images, skill load, and MCP search.
//!
//! Gated (`cargo test -- --ignored`). `Q38_MIX_ROUNDS` overrides length
//! (default 300). Compacts extractively; does not fatten the system prompt.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use crate::agent::{Agent, HttpCompleter, RunOpts};
    use crate::config::Config;
    use crate::mcp::{McpConfig, McpServer};
    use crate::media::{MediaPart, PROBE_IMAGE_B64};
    use crate::session::{SessionEvent, SessionLog, SessionMode};
    use crate::template::ChatMessage;

    const MCP_PY: &str = r#"
import json, sys

CORPUS = [
    "Lantern is a 262k-context coding agent. Project id lantern-core-9f3a.",
    "UI status chip in mocks is solid red. Palette token is ember-red.",
    "modgen skill writes src/gen/batch_N.rs and registers the module.",
    "Search this corpus with method search and argument query.",
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
        write_msg({"jsonrpc":"2.0","id":mid,"result":{"tools":[
            {"name":"search","description":"Keyword search over Lantern project docs"},
            {"name":"lookup","description":"Lookup a single doc sentence"}
        ]}})
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

    const SKILL_MD: &str = r#"---
name: modgen
description: Add a numbered Rust batch module under src/gen
---
# modgen

When asked to add generated lantern modules:

1. Prefer `python3 scripts/emit.py N COUNT` (COUNT default 900).
2. Then `read src/lib.rs` and make sure `pub mod gen;` exists.
3. Put a `// skill:modgen` comment in any hand-written batch file.
4. Do not invent other skills. Stop after the files are on disk.
"#;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Chat,
        Design,
        Docs,
        Code,
        Burst,
        ImageView,
        ImageSend,
        Skill,
        Mcp,
        Recall,
    }

    fn mix_rounds() -> u32 {
        std::env::var("Q38_MIX_ROUNDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(300)
    }

    fn kind_for(i: u32) -> Kind {
        match i {
            1..=18 => Kind::Chat,
            19..=30 => Kind::Design,
            31..=50 => Kind::Docs,
            n if n % 17 == 0 => Kind::Skill,
            n if n % 19 == 0 => Kind::Mcp,
            n if n % 23 == 0 => Kind::ImageSend,
            n if n % 29 == 0 => Kind::ImageView,
            n if n % 31 == 0 => Kind::Recall,
            n if n > 50 && n % 13 == 0 => Kind::Burst,
            _ => Kind::Code,
        }
    }

    fn prompt_for(kind: Kind, i: u32) -> String {
        match kind {
            Kind::Chat => format!(
                "Idle chat, no tools. Round {i}/300. We are spinning up a toy crate named lantern. \
Reply in two short sentences as a colleague, nothing else."
            ),
            Kind::Design => format!(
                "Project design, round {i}. Append one short bullet to DESIGN.md about lantern \
(modules under src/gen, docs under docs/, UI notes). Then stop. Do not cargo."
            ),
            Kind::Docs => match i {
                31 => "Create README.md for crate lantern: what it is, how to generate packs with scripts/emit.py, and the project id lantern-core-9f3a. Then stop.".into(),
                32 => "Write docs/ARCHITECTURE.md: src/lib.rs re-exports gen, generated packs are throwaway volume. Then stop.".into(),
                33 => "Write docs/API.md with a section listing the emit.py CLI. Then stop.".into(),
                _ => format!(
                    "Append one paragraph to docs/changelog.md noting mix round {i}. Then stop."
                ),
            },
            Kind::Code => format!(
                "Hand-write src/gen/batch_{i}.rs with 12 pub fn lantern_b{i}_f0..f11, each returning {i}*100+index, \
and a `// skill:modgen` comment at the top. Register `pub mod batch_{i};` in src/gen/mod.rs and `pub mod gen;` in src/lib.rs if missing. Then stop."
            ),
            Kind::Burst => format!(
                "Run `python3 scripts/emit.py {i} 1200` to emit src/gen/pack_{i}.rs (about 1200 one-line fns). \
If it errors, read scripts/emit.py and fix. Then stop. No cargo test."
            ),
            Kind::ImageView => {
                "What color is assets/red.png? Use the view tool. Append one line `chip=<color>` to docs/ui-notes.md. Then stop.".into()
            }
            Kind::ImageSend => {
                "I attached a mock PNG. Record its dominant color as a new line in docs/ui-notes.md (format `sent=<color>`). Then stop.".into()
            }
            Kind::Skill => format!(
                "[skill:modgen]\nrun python3 scripts/emit.py {i} 400 so pack_{i} exists. Then stop."
            ),
            Kind::Mcp => {
                "Use mcp with server docs and method search, args query lantern-core. Quote the project id token in your final sentence. Then stop. Do not bash.".into()
            }
            Kind::Recall => {
                "Using recall or memory_search if the live window lost it, reply with only the project id token from README/DESIGN (lantern-core-…). Then stop.".into()
            }
        }
    }

    fn live_cfg() -> Config {
        let (mut cfg, _) = Config::load_or_init().unwrap();
        cfg.apply_env();
        cfg
    }

    fn count_rs_lines(root: &Path) -> usize {
        let mut n = 0usize;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name();
                if name == "target" || name == ".git" || name == ".q38-home" {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        n += s.lines().count();
                    }
                }
            }
        }
        n
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

    fn setup_ws(dir: &Path) -> PathBuf {
        let home = dir.join(".q38-home");
        std::fs::create_dir_all(home.join("skills/modgen")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::create_dir_all(dir.join("src/gen")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(home.join("skills/modgen/SKILL.md"), SKILL_MD).unwrap();
        std::fs::write(dir.join("scripts/emit.py"), EMIT_PY).unwrap();
        std::fs::write(dir.join("scripts/docs_mcp.py"), MCP_PY).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"lantern\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub mod gen;\n").unwrap();
        std::fs::write(dir.join("src/gen/mod.rs"), "// lantern generated modules\n").unwrap();
        let png =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, PROBE_IMAGE_B64)
                .unwrap();
        std::fs::write(dir.join("assets/red.png"), png).unwrap();
        home
    }

    #[tokio::test]
    #[ignore = "live llama.cpp reference box"]
    async fn live_mix_300_chat_docs_code_image_skill_mcp() {
        let rounds = mix_rounds();
        let cfg = live_cfg();
        let dir =
            std::env::temp_dir().join(format!("q38-live-mix-{}", uuid::Uuid::new_v4().simple()));
        let sess = dir.join("sessions");
        std::fs::create_dir_all(&sess).unwrap();
        let home = setup_ws(&dir);

        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        for i in 1..=rounds {
            *counts.entry(format!("{:?}", kind_for(i))).or_default() += 1;
        }
        eprintln!("mix schedule rounds={rounds} kinds={counts:?}");

        let mcp = McpConfig {
            servers: vec![McpServer {
                name: "docs".into(),
                command: "python3".into(),
                args: vec![dir.join("scripts/docs_mcp.py").display().to_string()],
                cwd: dir.display().to_string(),
                description: "Lantern project docs".into(),
                methods: vec!["search".into(), "lookup".into()],
                ..McpServer::default()
            }],
            timeout_s: 15,
        };
        let completer =
            HttpCompleter::connect(&cfg, SessionMode::Agent.default_policy_with(&cfg.policy))
                .await
                .unwrap();
        let mut opts = RunOpts::from_config(&cfg, dir.clone());
        opts.print = false;
        opts.agents_md = false;
        opts.persist_session = true;
        opts.session_id = "live-mix".into();
        opts.session_dir = Some(sess.clone());
        opts.home = Some(home);
        opts.peripheral = true;
        opts.skills_auto_catalog = false;
        opts.media = true;
        opts.mcp = mcp;
        opts.max_steps = 10;
        opts.working_window = 6_000;
        opts.generation_reserve = 0;
        let mut agent = Agent::new(completer, opts).unwrap();

        let t0 = Instant::now();
        let mut budget_halts = 0u32;
        let mut steps_sum = 0u32;
        let mut errors = 0u32;

        for i in 1..=rounds {
            let kind = kind_for(i);
            let prompt = prompt_for(kind, i);
            let result = if kind == Kind::ImageSend {
                let mut msg = ChatMessage::user(prompt);
                msg.parts = vec![MediaPart::image_url(format!(
                    "data:image/png;base64,{PROBE_IMAGE_B64}"
                ))];
                agent.run_message(msg).await
            } else {
                agent.run(&prompt).await
            };
            let out = match result {
                Ok(o) => o,
                Err(e) => {
                    errors += 1;
                    eprintln!("mix round {i} ({kind:?}) err={e}");
                    if errors >= 8 {
                        panic!("too many completer errors ({errors}) at round {i}");
                    }
                    continue;
                }
            };
            steps_sum += out.steps;
            if out
                .stop_reason
                .as_deref()
                .unwrap_or("")
                .starts_with("budget:context")
            {
                budget_halts += 1;
                eprintln!("mix budget halt at {i} after {:?}", t0.elapsed());
                break;
            }
            if i == 1 || i % 25 == 0 || i == rounds {
                let loc = count_rs_lines(&dir);
                let hidden = agent
                    .messages()
                    .iter()
                    .filter(|m| {
                        m.role == "user"
                            && crate::template::is_hidden_user_text(
                                m.content.as_deref().unwrap_or(""),
                            )
                    })
                    .count();
                eprintln!(
                    "mix i={i}/{rounds} kind={kind:?} steps={} live={} hidden={hidden} loc={loc} steps_sum={steps_sum} elapsed={:?}",
                    out.steps,
                    agent.messages().len(),
                    t0.elapsed()
                );
            }
        }

        if rounds >= 300 {
            let mut extra = 0u32;
            while count_rs_lines(&dir) < 30_000 && extra < 25 {
                extra += 1;
                let n = 900 + extra;
                let out = agent
                    .run(&format!(
                        "Line count is still under 30k. Run `python3 scripts/emit.py {n} 1200` and stop."
                    ))
                    .await
                    .unwrap();
                steps_sum += out.steps;
                eprintln!(
                    "mix top-up extra={extra} loc={} steps={}",
                    count_rs_lines(&dir),
                    out.steps
                );
            }
        }
        let log = SessionLog::open_in(&sess, "live-mix").unwrap();
        let names = tool_names(log.events());
        let loc = count_rs_lines(&dir);
        let skill_notes = log
            .events()
            .iter()
            .filter(|e| match e {
                SessionEvent::User(u) => u.text.contains("[skill:"),
                _ => false,
            })
            .count();
        let mcp_n = names.iter().filter(|n| *n == "mcp").count();
        let view_n = names.iter().filter(|n| *n == "view").count();
        let write_n = names
            .iter()
            .filter(|n| *n == "write" || *n == "edit")
            .count();
        let bash_n = names.iter().filter(|n| *n == "bash").count();
        let read_n = names.iter().filter(|n| *n == "read").count();
        let compact_n = log
            .events()
            .iter()
            .filter(|e| e.type_name() == "session/compact")
            .count();
        eprintln!(
            "mix done loc={loc} compact={compact_n} skill_notes={skill_notes} mcp={mcp_n} view={view_n} \
read={read_n} write/edit={write_n} bash={bash_n} steps={steps_sum} errors={errors} wall={:?} live={}",
            t0.elapsed(),
            agent.messages().len()
        );

        assert_eq!(budget_halts, 0, "compact failed to hold the live window");
        assert!(
            compact_n >= 1 || rounds < 60,
            "expected extractive compact on a 12k window after {rounds} rounds"
        );
        assert!(
            agent.messages().len() <= 80,
            "live window bloated to {}",
            agent.messages().len()
        );
        if rounds >= 50 {
            assert!(
                skill_notes >= 1,
                "modgen skill was never injected: {names:?}"
            );
            assert!(mcp_n >= 1, "mcp tool never ran: {names:?}");
            assert!(view_n >= 1, "view never ran (image path dead): {names:?}");
            assert!(
                write_n >= 5,
                "too little file development: write/edit={write_n}"
            );
        }
        let loc_floor = if rounds >= 300 {
            20_000usize
        } else if rounds >= 80 {
            2_500usize
        } else {
            0
        };
        assert!(
            loc >= loc_floor,
            "rust loc={loc} below floor {loc_floor} after {rounds} rounds"
        );

        let ui = std::fs::read_to_string(dir.join("docs/ui-notes.md")).unwrap_or_default();
        if rounds >= 80 {
            assert!(
                ui.to_ascii_lowercase().contains("red")
                    || ui.to_ascii_lowercase().contains("ember"),
                "image rounds did not land a color in docs/ui-notes.md: {ui:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
