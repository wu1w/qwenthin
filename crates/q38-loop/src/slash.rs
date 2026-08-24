//! Hermes-shaped slash registry. Wash of QwenPaw `SlashCommandRegistry`
//! (`resolve` + aliases + category) and Hermes `COMMAND_REGISTRY` names.
//!
//! Local commands never call the model. Unknown `/foo` is `None` so a path
//! paste can still be a user turn. Known-but-rejected zoo commands return
//! [`SlashCmd::Unsupported`].

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

use crate::channel::BusyPolicy;
use crate::config::CODING_CTX_TOKENS;
use crate::family::Family;
use crate::mcp::McpRegistry;
use crate::permit::{ApprovalMode, PlanAction, PLAN_IMPLEMENT};
use crate::policy::{Effort, ThinkPolicy};
use crate::prefix_cache::{self, clustered, stuck_at_short_prefix};
use crate::session::catalog::SessionInfo;
use crate::session::{
    derive_messages, policy_for_effort, CompactEvent, SessionEvent, SessionMode, UndoEvent,
};
use crate::skills::SkillCatalog;
use crate::template::is_hidden_user_text;
use crate::tokenize::count_tokens;

const HINT_MAX: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCmd {
    Think(Effort),
    Off,
    Mode(SessionMode),
    Help,
    Status,
    Context { all: bool },
    New { title: Option<String> },
    Clear,
    Title { name: String },
    Resume { query: Option<String> },
    Sessions { search: Option<String> },
    History,
    Compress { hint: Option<String> },
    Stop,
    Queue { text: String },
    Steer { text: String },
    Busy { policy: Option<BusyPolicy> },
    Undo,
    Retry,
    Model { args: String },
    Setup,
    Approvals { mode: Option<ApprovalMode> },
    Plan { action: PlanAction },
    Clarify { on: Option<bool> },
    Tools,
    Skills,
    Mcp,
    Usage,
    Diff { args: String },
    Reload,
    Version,
    Config,
    InvokeSkill { name: String, args: String },
    InvokeMcp { name: String, args: String },
    Cron { args: String },
    Unsupported { name: String },
    LowPrecision { on: Option<bool> },
}

impl SlashCmd {
    pub fn policy(&self) -> Option<ThinkPolicy> {
        match self {
            Self::Off => Some(ThinkPolicy::off_with(&crate::policy::ThinkBudget::default())),
            Self::Think(effort) => Some(policy_for_effort(*effort)),
            _ => None,
        }
    }
}

const REJECT: &[(&str, &str)] = &[
    ("personality", "q38 has no personality overlays"),
    ("skin", "TUI theme is not in q38 yet"),
    ("voice", "voice mode is not in q38"),
    ("heartbeat", "use the console 心跳 page"),
    ("hb", "use the console 心跳 page"),
    ("goal", "no Ralph/judge loop"),
    ("subgoal", "no Ralph/judge loop"),
    ("moa", "no mixture-of-agents"),
    ("browser", "no CDP browser tool"),
    ("kanban", "no kanban board"),
    ("pet", "no pets"),
    ("hatch", "no pets"),
    (
        "platforms",
        "configure [[channels.endpoints]] and run q38 --channels",
    ),
    (
        "gateway",
        "configure [[channels.endpoints]] and run q38 --channels",
    ),
    ("handoff", "no cross-platform handoff"),
    ("topup", "no billing portal"),
    ("subscription", "no billing portal"),
    ("insights", "no 30-day analytics"),
    ("rollback", "no shadow-git checkpoints"),
    ("snapshot", "no config snapshots"),
    ("export", "no profile tarball"),
    ("import", "no profile tarball"),
    ("learn", "author a SKILL.md in ~/.q38-agent/skills instead"),
    ("curator", "no skill curator"),
    ("blueprint", "no cron blueprints"),
    ("suggestions", "no automation suggestions"),
    ("plugins", "no plugin hub"),
    ("wake", "no wake word"),
    ("journey", "no memory graph"),
    ("background", "no parallel background sessions yet"),
    ("bg", "no parallel background sessions yet"),
    ("btw", "no parallel background sessions yet"),
    ("agents", "no subagent tree"),
    ("tasks", "no subagent tree"),
    ("branch", "tool-set forks use /mode, not session branch"),
    ("fork", "tool-set forks use /mode, not session branch"),
    ("worktree", "no git worktree helper"),
    ("checkpoint", "no shadow-git checkpoints"),
    (
        "deny",
        "use n on the permission overlay, or /approvals yolo",
    ),
    ("daemon", "no daemon process"),
    ("restart", "no daemon process"),
    ("dream", "no dream job"),
    ("memorize", "write MEMORY.md or use memory_search"),
    ("reme_status", "no ReMe auto-memory"),
    ("proactive", "no proactive mode"),
    (
        "system_prompt",
        "system is frozen at session start; /status shows it clipped",
    ),
    (
        "dump_history",
        "JSONL is the dump: ~/.q38-agent/sessions/<id>.jsonl",
    ),
    ("load_history", "use /resume <id>"),
    ("summarize_status", "compact is extractive and synchronous"),
    (
        "compact_str",
        "use /history after /compress; archive is in the live window",
    ),
    ("message", "use /history"),
    ("theme", "TUI-local; not in this runtime"),
    ("inspect", "TUI-local; not in this runtime"),
    ("indicator", "TUI-local"),
    ("mouse", "TUI-local"),
    ("redraw", "TUI-local"),
    ("statusbar", "TUI-local"),
    ("footer", "TUI-local"),
    ("focus", "TUI-local"),
    ("verbose", "TUI-local"),
    ("timestamps", "TUI-local"),
    ("battery", "TUI-local"),
    ("details", "TUI-local"),
    ("quit", "exit the client"),
    ("exit", "exit the client"),
];

/// Parse a slash line. `None` = not a command (send to the model).
pub fn parse_slash(text: &str) -> Option<SlashCmd> {
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let body = t[1..].trim_start();
    if body.is_empty() {
        return None;
    }
    let (name, rest) = match body.split_once(char::is_whitespace) {
        Some((n, r)) => (n.to_ascii_lowercase(), r.trim().to_string()),
        None => (body.to_ascii_lowercase(), String::new()),
    };
    if let Some((_, why)) = REJECT.iter().find(|(n, _)| *n == name) {
        return Some(SlashCmd::Unsupported {
            name: format!("/{name}: {why}"),
        });
    }
    match name.as_str() {
        "help" => rest.is_empty().then_some(SlashCmd::Help),
        "status" => rest.is_empty().then_some(SlashCmd::Status),
        "version" => rest.is_empty().then_some(SlashCmd::Version),
        "config" => rest.is_empty().then_some(SlashCmd::Config),
        "reload" => rest.is_empty().then_some(SlashCmd::Reload),
        "usage" => rest.is_empty().then_some(SlashCmd::Usage),
        "tools" => Some(SlashCmd::Tools),
        "skills" => Some(SlashCmd::Skills),
        "mcp" => parse_mcp_slash(&rest),
        "undo" => rest.is_empty().then_some(SlashCmd::Undo),
        "retry" => rest.is_empty().then_some(SlashCmd::Retry),
        "stop" => Some(SlashCmd::Stop),
        "history" => rest.is_empty().then_some(SlashCmd::History),
        "clear" => Some(SlashCmd::Clear),
        "fast" => {
            if rest.is_empty() {
                Some(SlashCmd::Off)
            } else {
                Some(SlashCmd::Unsupported {
                    name: "/fast is thinking-off in q38, not cloud Priority Processing".into(),
                })
            }
        }
        "think" => parse_think(&rest),
        "reasoning" => parse_reasoning(&rest),
        "mode" => {
            if rest.is_empty() {
                None
            } else {
                rest.parse().ok().map(SlashCmd::Mode)
            }
        }
        "context" | "ctx" => Some(SlashCmd::Context {
            all: rest.eq_ignore_ascii_case("all"),
        }),
        "new" | "reset" => Some(SlashCmd::New {
            title: parse_new_title(&rest),
        }),
        "title" => {
            if rest.is_empty() {
                None
            } else {
                Some(SlashCmd::Title { name: rest })
            }
        }
        "resume" => Some(SlashCmd::Resume {
            query: nonempty(rest),
        }),
        "sessions" | "switch" => Some(SlashCmd::Sessions {
            search: nonempty(rest),
        }),
        "compress" | "compact" => Some(SlashCmd::Compress {
            hint: nonempty(clip(&rest, HINT_MAX)),
        }),
        "queue" | "q" => {
            if rest.is_empty() {
                None
            } else {
                Some(SlashCmd::Queue { text: rest })
            }
        }
        "steer" => {
            if rest.is_empty() {
                None
            } else {
                Some(SlashCmd::Steer { text: rest })
            }
        }
        "busy" => parse_busy(&rest),
        "model" if rest.eq_ignore_ascii_case("setup") => Some(SlashCmd::Setup),
        "model" => Some(SlashCmd::Model { args: rest }),
        "setup" => rest.is_empty().then_some(SlashCmd::Setup),
        "plan" => parse_plan(&rest),
        "clarify" => parse_clarify(&rest),
        "approvals" | "approval" => parse_approvals(&rest),
        "lossy" | "low-precision" | "lowprecision" => parse_lossy(&rest),
        "yolo" => rest.is_empty().then_some(SlashCmd::Approvals {
            mode: Some(ApprovalMode::Yolo),
        }),
        "approve" => rest.is_empty().then_some(SlashCmd::Plan {
            action: PlanAction::Go,
        }),
        "diff" => Some(SlashCmd::Diff { args: rest }),
        "cron" => Some(SlashCmd::Cron { args: rest }),
        _ => None,
    }
}

pub fn parse_slash_with_skills(text: &str, skills: &SkillCatalog) -> Option<SlashCmd> {
    parse_slash_with_periphery(text, skills, None)
}

pub fn parse_slash_with_periphery(
    text: &str,
    skills: &SkillCatalog,
    mcp: Option<&McpRegistry>,
) -> Option<SlashCmd> {
    if let Some(cmd) = parse_slash(text) {
        return Some(cmd);
    }
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let body = t[1..].trim_start();
    let (name, rest) = match body.split_once(char::is_whitespace) {
        Some((n, r)) => (n, r.trim()),
        None => (body, ""),
    };
    if let Some(sk) = skills.get(name) {
        return Some(SlashCmd::InvokeSkill {
            name: sk.name.clone(),
            args: rest.to_string(),
        });
    }
    if let Some(srv) = mcp.and_then(|r| r.get(name)) {
        return Some(SlashCmd::InvokeMcp {
            name: srv.name.clone(),
            args: rest.to_string(),
        });
    }
    None
}

fn parse_mcp_slash(rest: &str) -> Option<SlashCmd> {
    if rest.is_empty() {
        return Some(SlashCmd::Mcp);
    }
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, r)) => (n, r.trim()),
        None => (rest, ""),
    };
    Some(SlashCmd::InvokeMcp {
        name: name.to_string(),
        args: args.to_string(),
    })
}

fn parse_plan(rest: &str) -> Option<SlashCmd> {
    match rest.trim().to_ascii_lowercase().as_str() {
        "" | "on" => Some(SlashCmd::Plan {
            action: PlanAction::On,
        }),
        "off" | "exit" | "quit" => Some(SlashCmd::Plan {
            action: PlanAction::Off,
        }),
        "go" | "ok" | "yes" | "approve" | "implement" => Some(SlashCmd::Plan {
            action: PlanAction::Go,
        }),
        _ => Some(SlashCmd::Unsupported {
            name: "plan".into(),
        }),
    }
}

fn parse_clarify(rest: &str) -> Option<SlashCmd> {
    match rest.trim().to_ascii_lowercase().as_str() {
        "" | "on" => Some(SlashCmd::Clarify { on: Some(true) }),
        "off" | "exit" | "quit" => Some(SlashCmd::Clarify { on: Some(false) }),
        _ => Some(SlashCmd::Unsupported {
            name: "clarify".into(),
        }),
    }
}

fn parse_approvals(rest: &str) -> Option<SlashCmd> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(SlashCmd::Approvals { mode: None });
    }
    ApprovalMode::parse(rest).map(|mode| SlashCmd::Approvals { mode: Some(mode) })
}

fn parse_lossy(rest: &str) -> Option<SlashCmd> {
    match rest.trim().to_ascii_lowercase().as_str() {
        "" => Some(SlashCmd::LowPrecision { on: None }),
        "on" | "true" | "1" => Some(SlashCmd::LowPrecision { on: Some(true) }),
        "off" | "false" | "0" => Some(SlashCmd::LowPrecision { on: Some(false) }),
        _ => None,
    }
}

fn parse_think(rest: &str) -> Option<SlashCmd> {
    if rest.is_empty() {
        return Some(SlashCmd::Think(Effort::Medium));
    }
    if rest.split_whitespace().nth(1).is_some() {
        return None;
    }
    match rest.to_ascii_lowercase().as_str() {
        "low" => Some(SlashCmd::Think(Effort::Low)),
        "medium" => Some(SlashCmd::Think(Effort::Medium)),
        "xhigh" => Some(SlashCmd::Think(Effort::Xhigh)),
        "off" => Some(SlashCmd::Off),
        _ => None,
    }
}

fn parse_reasoning(rest: &str) -> Option<SlashCmd> {
    if rest.is_empty() || matches!(rest.to_ascii_lowercase().as_str(), "status" | "show") {
        return Some(SlashCmd::Status);
    }
    let first = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match first.as_str() {
        "off" | "none" | "minimal" | "hide" => Some(SlashCmd::Off),
        "low" => Some(SlashCmd::Think(Effort::Low)),
        "medium" => Some(SlashCmd::Think(Effort::Medium)),
        "high" | "xhigh" | "max" | "ultra" => Some(SlashCmd::Think(Effort::Xhigh)),
        _ => Some(SlashCmd::Unsupported {
            name: format!("/reasoning {rest}: use /think low|medium|xhigh|off"),
        }),
    }
}

fn parse_busy(rest: &str) -> Option<SlashCmd> {
    if rest.is_empty() || rest.eq_ignore_ascii_case("status") {
        return Some(SlashCmd::Busy { policy: None });
    }
    Some(SlashCmd::Busy {
        policy: Some(rest.parse().ok()?),
    })
}

fn parse_new_title(rest: &str) -> Option<String> {
    let parts: Vec<&str> = rest
        .split_whitespace()
        .filter(|p| !matches!(*p, "now" | "--yes" | "-y"))
        .collect();
    nonempty(parts.join(" "))
}

fn nonempty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

pub struct SlashView<'a> {
    pub session_id: &'a str,
    pub workspace: &'a Path,
    pub mode: SessionMode,
    pub policy: &'a ThinkPolicy,
    pub events: &'a [SessionEvent],
    pub model: &'a str,
    pub busy: BusyPolicy,
    pub channel: &'a str,
    pub title: &'a str,
    pub tools: &'a [Value],
    pub skill_count: usize,
    pub mcp_count: usize,
    pub window: u32,
    pub family: Family,
    pub queued: usize,
    pub plan_mode: bool,
    pub clarify_mode: bool,
    pub approvals: ApprovalMode,
    pub low_precision: bool,
}

pub fn help_text() -> String {
    "\
q38 slash commands (Hermes-shaped; local unless noted)

Session
  /help                  this list
  /status                session recap (no LLM)
  /context [all]         live-window token breakdown
  /new [title]           fresh session id (old JSONL kept)
  /clear                 same as /new, no compact note
  /title <name>          name the session
  /resume [id|title]     reopen a JSONL
  /sessions [search]     list sessions
  /history               live window text
  /compress [hint]       extract compact (alias /compact)
  /undo                  tombstone last user turn
  /retry                 undo + resend last user
  /stop                  abort the live turn

Busy
  /queue <text>          next turn (alias /q)
  /steer <text>          inject after next tool
  /busy interrupt|queue|steer

Thinking (same session, no fork)
  /think [low|medium|xhigh|off]
  /fast                  thinking off
  /reasoning …           alias for /think
  /mode chat|agent|think|code   forks (tool set changes)

Plan / permissions (TUI; --print stays YOLO)
  /setup                 walk endpoint, api_key, model into config.toml
  /plan                  read-only: research, write a plan, no edits
  /plan go               approve the plan and implement (alias /approve)
  /plan off              leave plan mode
  /clarify [on|off]      arm ask (2–4 options). /plan also arms it
  /approvals ask|auto|yolo
                         ask = prompt write/bash (Grok default)
                         auto = edits pass, bash still prompts
                         yolo = never prompt (alias /yolo)
  /lossy [on|off]        tighter doom/parse/repeat guards (user switch; model does not see this)

Config
  /model [name|--global] show or switch (single endpoint)
  /model setup           same as /setup
  /tools                 list OpenAI tools[]
  /skills                catalog (hidden inject, not a tool)
  /mcp                   mounted servers (one mcp() when any exist)
  /usage                 token totals and first-hop prefix cache
  /diff [path]           git diff in workspace
  /reload                reread config.toml next turn
  /cron [add|rm|list]    控制台定时任务（写 .q38/cron.json）
  /version /config
"
    .to_string()
}

pub fn unsupported_text(name: &str) -> String {
    format!("{name}. Not in q38.")
}

pub fn status_text(v: &SlashView<'_>) -> String {
    let recap = UsageRecap::from_events(v.events);
    let last_user = last_real_user(v.events).unwrap_or("(none)");
    let last_asst = last_assistant(v.events).unwrap_or("(none)");
    let effort = match v.policy.effort {
        Some(e) => e.as_str().to_string(),
        None if v.policy.enabled => "on".into(),
        None => "off".into(),
    };
    let low = if v.low_precision {
        "         - low precision: on\n"
    } else {
        ""
    };
    format!(
        "**Session**\n\
         - id: {}\n\
         - title: {}\n\
         - channel: {}\n\
         - mode: {}\n\
         - model: {}\n\
         - workspace: {}\n\
         - thinking: {} effort={effort} cap={}\n\
{low}\
         - approvals: {}\n\
         - plan: {}\n\
         - clarify: {}\n\
         - busy: {} (queued {})\n\
         - events: {}\n\n\
         **Recap** (local)\n\
         - user turns: {}\n\
         - tool results: {}\n\
         - tokens in/out: {}/{}\n\
         - prefix cache: {}\n\
         - last user: {}\n\
         - last reply: {}",
        v.session_id,
        if v.title.is_empty() {
            "(untitled)"
        } else {
            v.title
        },
        v.channel,
        v.mode.as_str(),
        v.model,
        v.workspace.display(),
        if v.policy.enabled { "on" } else { "off" },
        v.policy.max_think_tokens,
        v.approvals.as_str(),
        if v.plan_mode { "on" } else { "off" },
        if v.clarify_mode { "on" } else { "off" },
        v.busy.as_str(),
        v.queued,
        v.events.len(),
        recap.users,
        recap.tools,
        recap.prompt_tokens,
        recap.completion_tokens,
        recap.hit_line(),
        clip(last_user, 160),
        clip(last_asst, 160),
    )
}

pub fn context_text(v: &SlashView<'_>) -> String {
    let window = if v.window == 0 {
        CODING_CTX_TOKENS
    } else {
        v.window
    };
    let recap = UsageRecap::from_events(v.events);
    let live = recap.last_prompt_tokens;
    let msgs = derive_messages(v.events);
    let system = msgs.first().map(|m| m.text()).unwrap_or("");
    let sys_n = tokens(v.family, system);
    let tools_json = serde_json::to_string(v.tools).unwrap_or_else(|_| "[]".into());
    let tools_n = tokens(v.family, &tools_json);
    let conv: String = msgs
        .iter()
        .skip(1)
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    let conv_n = tokens(v.family, &conv);
    let estimate = sys_n.saturating_add(tools_n).saturating_add(conv_n);
    let used = if live > 0 {
        live.min(u64::from(u32::MAX)) as u32
    } else {
        estimate
    };
    let free = window.saturating_sub(used);
    let pct = if window == 0 {
        0
    } else {
        (u64::from(used).saturating_mul(100) / u64::from(window)).min(100) as u32
    };
    let mut extra = String::new();
    if recap.compacts > 0 {
        extra.push_str(&format!("\n- compacts: {}", recap.compacts));
    }
    if v.skill_count > 0 {
        extra.push_str(&format!(
            "\n- skills: {} names (bodies injected on match, not tools[])",
            v.skill_count
        ));
    }
    if v.mcp_count > 0 {
        extra.push_str(&format!(
            "\n- mcp: {} servers (one mcp() in tools[]; cards on match, not system)",
            v.mcp_count
        ));
    }
    let source = if live > 0 {
        "Last hop prompt_tokens (live prefix after compact)."
    } else {
        "Local tokenizer estimate on derive_messages (compact-aware)."
    };
    format!(
        "**Context** ~{used}/{window} ({pct}%)\n\
         - system: {sys_n}\n\
         - tools[]: {tools_n}\n\
         - live conversation: {conv_n}\n\
         - free: {free}{extra}\n\
         {source} Cumulative prompt across hops is not the window."
    )
}

pub fn history_text(events: &[SessionEvent], max_chars: usize) -> String {
    let mut lines = Vec::new();
    for (i, e) in events.iter().enumerate() {
        match e {
            SessionEvent::User(u) if !is_hidden_user_text(&u.text) => {
                lines.push(format!("[{i} user] {}", clip(&u.text, 240)));
            }
            SessionEvent::Assistant(a) if !a.content.is_empty() => {
                lines.push(format!("[{i} assistant] {}", clip(&a.content, 240)));
            }
            SessionEvent::Tool(t) => {
                lines.push(format!("[{i} tool {}] {}", t.name, clip(&t.output, 120)));
            }
            SessionEvent::Compact(_) => lines.push(format!("[{i} compact]")),
            SessionEvent::Undo(u) => {
                lines.push(format!("[{i} undo {}..{}]", u.from_seq, u.until_seq));
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return "**History**\n\n(empty)".into();
    }
    let mut body = lines.join("\n");
    // Char-safe clip: byte slicing panics on CJK history at non-boundary offsets.
    if body.chars().count() > max_chars {
        let half = max_chars / 2;
        let total = body.chars().count();
        let head: String = body.chars().take(half).collect();
        let tail: String = body.chars().skip(total - half).collect();
        body = format!("{head}\n...\n{tail}");
    }
    format!("**History**\n\n{body}")
}

pub fn compact_reply(plan: Option<&CompactEvent>) -> String {
    match plan {
        None => {
            "**Nothing to compact.**\n\n- Live window already small\n- No turns were evicted"
                .into()
        }
        Some(p) => format!(
            "**Compact complete.**\n\n- Archived through seq {}\n- Continuation is extractive (not an LLM summary)\n- Older turns stay in JSONL; use recall",
            p.until_seq
        ),
    }
}

pub fn sessions_text(rows: &[SessionInfo]) -> String {
    if rows.is_empty() {
        return "**Sessions**\n\n(none)".into();
    }
    let mut s = String::from("**Sessions**\n");
    for r in rows.iter().take(40) {
        let title = if r.title.is_empty() {
            r.preview.as_str()
        } else {
            r.title.as_str()
        };
        s.push_str(&format!(
            "- {}  {}  {}  {}\n",
            r.id,
            r.mode.as_str(),
            r.channel,
            clip(title, 60)
        ));
    }
    s
}

pub fn mcp_text(reg: &McpRegistry) -> String {
    if reg.servers.is_empty() {
        return "**MCP**\n\nNone. Drop `[[mcp.servers]]` in config.toml or `.q38/mcp.toml`.".into();
    }
    let mut s = String::from(
        "**MCP** (matched by mcp / [mcp:name] / /name; one mcp() tool when mounted)\n",
    );
    for srv in &reg.servers {
        let trig = if srv.description.is_empty() {
            if srv.methods.is_empty() {
                "on demand".to_string()
            } else {
                srv.methods.join(", ")
            }
        } else {
            srv.description.clone()
        };
        s.push_str(&format!("- {}: {}\n", srv.name, clip(&trig, 80)));
    }
    s
}

pub fn skills_text(catalog: &SkillCatalog) -> String {
    if catalog.skills.is_empty() {
        return "**Skills**\n\nNone under ~/.q38-agent/skills. Drop a folder with SKILL.md.".into();
    }
    let mut s = String::from("**Skills** (matched by name / /name; not a tool)\n");
    for sk in &catalog.skills {
        s.push_str(&format!("- {}: {}\n", sk.name, clip(&sk.description, 80)));
    }
    s
}

pub fn tools_text(tools: &[Value]) -> String {
    if tools.is_empty() {
        return "**Tools**\n\n(none — chat mode)".into();
    }
    let mut s = String::from("**Tools** (current session)\n");
    for t in tools {
        if let Some(n) = t["function"]["name"].as_str() {
            s.push_str("- ");
            s.push_str(n);
            s.push('\n');
        }
    }
    s
}

pub fn usage_text(events: &[SessionEvent]) -> String {
    format_usage(&UsageRecap::from_events(events), None)
}

pub fn usage_view(v: &SlashView<'_>) -> String {
    let recap = UsageRecap::from_events(v.events);
    let system = v
        .events
        .first()
        .and_then(|e| match e {
            SessionEvent::Start(s) => Some(s.system.as_str()),
            _ => None,
        })
        .unwrap_or("");
    let frozen = prefix_cache::frozen_system_tools_tokens(v.family, system, v.tools, v.policy);
    format_usage(&recap, frozen)
}

fn format_usage(recap: &UsageRecap, frozen: Option<u64>) -> String {
    let mut s = format!(
        "**Usage** (from session events; local box has no $ bill)\n\
         - prompt_tokens: {}\n\
         - completion_tokens: {}\n\
         - cached_tokens: {}\n\
         - prefix cache hit: {}\n\
         - first-hop hit: {}\n\
         - last prompt (live prefix): {}\n\
         - compacts: {}\n\
         - total: {}",
        recap.prompt_tokens,
        recap.completion_tokens,
        if recap.cached_reported {
            recap.cached_tokens.to_string()
        } else {
            "(not reported)".into()
        },
        recap.hit_line(),
        recap.first_hop_line(),
        recap.last_prompt_tokens,
        recap.compacts,
        recap.prompt_tokens.saturating_add(recap.completion_tokens)
    );
    if let Some(note) = recap.prefix_note(frozen) {
        s.push_str("\n- note: ");
        s.push_str(&note);
    }
    s
}

/// Token / prefix-cache totals from assistant JSONL rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageRecap {
    pub users: usize,
    pub tools: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    /// Prompt tokens on steps that reported `cached_tokens`.
    pub cache_prompt_tokens: u64,
    pub cached_reported: bool,
    pub assistant_steps: u64,
    pub first_hop_prompt_tokens: u64,
    pub first_hop_cached_tokens: u64,
    pub first_hop_reported: u64,
    /// First hops (after the opening user turn) whose cached count stayed on a
    /// short prefix while the prompt grew. Compact / policy steps are skipped.
    pub stuck_cached: Vec<u64>,
    /// Last model call's prompt size (live prefix). Not the session sum.
    pub last_prompt_tokens: u64,
    pub compacts: u64,
}

impl UsageRecap {
    pub fn from_events(events: &[SessionEvent]) -> Self {
        let mut s = Self::default();
        let mut expect_first = false;
        let mut skip_next_first = false;
        for (i, e) in events.iter().enumerate() {
            if skipped_by_undo(events, i as u64) {
                continue;
            }
            match e {
                SessionEvent::User(u) if !is_hidden_user_text(&u.text) => {
                    s.users += 1;
                    expect_first = true;
                }
                SessionEvent::Tool(_) => s.tools += 1,
                SessionEvent::Compact(_) | SessionEvent::Policy(_) => {
                    skip_next_first = true;
                    if matches!(e, SessionEvent::Compact(_)) {
                        s.compacts += 1;
                    }
                }
                SessionEvent::Assistant(a) => {
                    s.assistant_steps += 1;
                    s.prompt_tokens += a.prompt_tokens;
                    s.completion_tokens += a.completion_tokens;
                    if a.prompt_tokens > 0 {
                        s.last_prompt_tokens = a.prompt_tokens;
                    }
                    if let Some(c) = a.cached_tokens {
                        if a.prompt_tokens > 0 {
                            s.cached_reported = true;
                            s.cached_tokens += c.min(a.prompt_tokens);
                            s.cache_prompt_tokens += a.prompt_tokens;
                        }
                    }
                    if expect_first {
                        expect_first = false;
                        let skip = skip_next_first;
                        skip_next_first = false;
                        if let Some(c) = a.cached_tokens {
                            if a.prompt_tokens > 0 {
                                let c = c.min(a.prompt_tokens);
                                s.first_hop_reported += 1;
                                s.first_hop_cached_tokens += c;
                                s.first_hop_prompt_tokens += a.prompt_tokens;
                                if !skip
                                    && s.users >= 2
                                    && stuck_at_short_prefix(c, a.prompt_tokens)
                                {
                                    s.stuck_cached.push(c);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// `cached_tokens / prompt_tokens` on steps that reported cache.
    pub fn hit_rate(&self) -> Option<f64> {
        if !self.cached_reported || self.cache_prompt_tokens == 0 {
            None
        } else {
            Some(self.cached_tokens as f64 / self.cache_prompt_tokens as f64)
        }
    }

    pub fn hit_pct(&self) -> Option<f64> {
        self.hit_rate().map(|r| (r * 1000.0).round() / 10.0)
    }

    pub fn hit_line(&self) -> String {
        match self.hit_pct() {
            Some(p) => format!(
                "{p:.1}%  ({}/{} prompt tok)",
                self.cached_tokens, self.cache_prompt_tokens
            ),
            None => "n/a (endpoint did not return cached_tokens)".into(),
        }
    }

    pub fn first_hop_hit_rate(&self) -> Option<f64> {
        if self.first_hop_reported == 0 || self.first_hop_prompt_tokens == 0 {
            None
        } else {
            Some(self.first_hop_cached_tokens as f64 / self.first_hop_prompt_tokens as f64)
        }
    }

    pub fn first_hop_line(&self) -> String {
        match self.first_hop_hit_rate() {
            Some(r) => {
                let p = (r * 1000.0).round() / 10.0;
                format!(
                    "{p:.1}%  ({}/{} prompt tok)",
                    self.first_hop_cached_tokens, self.first_hop_prompt_tokens
                )
            }
            None => "n/a".into(),
        }
    }

    pub fn prefix_note(&self, frozen: Option<u64>) -> Option<String> {
        let n = self.stuck_cached.len();
        if n == 0 {
            return None;
        }
        let med = clustered(&self.stuck_cached)?;
        let where_ = match frozen {
            Some(f) if prefix_cache::near(med, f) => {
                format!("frozen system+tools (~{f} tok)")
            }
            _ => format!("~{med} tok (first historical assistant)"),
        };
        Some(format!(
            "{n} first hop(s) reused {where_} while prompts grew; historical think was stripped"
        ))
    }

    pub fn json(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "cached_tokens": self.cached_tokens,
            "cache_prompt_tokens": self.cache_prompt_tokens,
            "cached_reported": self.cached_reported,
            "assistant_steps": self.assistant_steps,
            "hit_rate": self.hit_rate(),
            "hit_pct": self.hit_pct(),
            "first_hop_prompt_tokens": self.first_hop_prompt_tokens,
            "first_hop_cached_tokens": self.first_hop_cached_tokens,
            "first_hop_hit_rate": self.first_hop_hit_rate(),
            "stuck_first_hops": self.stuck_cached.len(),
            "stuck_prefix_tokens": clustered(&self.stuck_cached),
            "prefix_note": self.prefix_note(None),
            "last_prompt_tokens": self.last_prompt_tokens,
            "live_prompt_tokens": self.last_prompt_tokens,
            "compacts": self.compacts,
        })
    }
}

pub fn model_text(current: &str, args: &str) -> ModelAction {
    let args = args.trim();
    if args.is_empty() || args.eq_ignore_ascii_case("list") || args == "-h" || args == "help" {
        return ModelAction::Show(format!(
            "**Model** {current}\n\n\
             q38 talks to one OpenAI-compat endpoint (`config.toml` / Q38_MODEL).\n\
             `/model <name>` session-only. `/model <name> --global` writes config.toml.\n\
             `/setup` (or `/model setup`) walks base_url, api_key, model in the TUI.\n\
             `q38 web` always uses the file. Q38_* overlays CLI/TUI/tests only."
        ));
    }
    let global = args.split_whitespace().any(|p| p == "--global");
    let name = args
        .split_whitespace()
        .find(|p| *p != "--global" && *p != "--session" && *p != "--once")
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return ModelAction::Show("Usage: /model <name> [--global]".into());
    }
    ModelAction::Switch { name, global }
}

pub enum ModelAction {
    Show(String),
    Switch { name: String, global: bool },
}

pub fn version_text() -> String {
    format!(
        "q38 {}  family=qwen38  loop=q38-loop",
        env!("CARGO_PKG_VERSION")
    )
}

pub fn setup_text() -> String {
    "**Setup** is a TUI walk (`q38` with no prompt).\n\n\
     Fields: base_url → api_key → model. Saved to ~/.q38-agent/config.toml.\n\
     Or edit that file and set Q38_BASE_URL / Q38_API_KEY / Q38_MODEL."
        .into()
}

pub fn low_precision_text(on: bool) -> String {
    if on {
        "low precision: on — tighter doom/parse/repeat guards (model does not see this)".into()
    } else {
        "low precision: off".into()
    }
}

pub fn approvals_text(mode: ApprovalMode) -> String {
    format!(
        "**Approvals** {}\n\n\
         ask  — prompt write/edit/bash/run_code/mcp (y allow, a always this tool, n deny)\n\
         auto — workspace write/edit pass; bash/run_code/mcp still prompt\n\
         yolo — never prompt (`--print` is always yolo)\n\
         `/approvals ask|auto|yolo`  alias `/yolo`",
        mode.as_str()
    )
}

pub fn plan_text(on: bool) -> String {
    if on {
        format!(
            "**Plan mode on.** Read-only tools. Write a markdown plan, then `/plan go` \
             (or y on the TUI prompt) to implement.\n`/plan off` leaves without implementing.\n\
             `ask` is armed (2–4 options). Next implement prompt: `{PLAN_IMPLEMENT}`"
        )
    } else {
        "**Plan mode off.** `/plan` to research read-only before edits.".into()
    }
}

pub fn clarify_text(on: bool, plan: bool) -> String {
    if on {
        "**Clarify on.** `ask` is armed. The model may present 2–4 options; pick, skip \
         (recommended), or type Other.\n`/clarify off` disarms unless `/plan` is on."
            .into()
    } else if plan {
        "**Clarify off.** `ask` stays armed because `/plan` is on.".into()
    } else {
        "**Clarify off.** `/clarify` or `/plan` to arm `ask`.".into()
    }
}

pub fn config_text(model: &str, workspace: &Path, mode: SessionMode, busy: BusyPolicy) -> String {
    format!(
        "**Config**\n- model: {model}\n- workspace: {}\n- mode: {}\n- busy: {}",
        workspace.display(),
        mode.as_str(),
        busy.as_str()
    )
}

pub fn diff_text(workspace: &Path, args: &str) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workspace).arg("diff");
    let args = args.trim();
    if args == "staged" {
        cmd.arg("--cached");
    } else if args == "stat" || args == "--stat" {
        cmd.arg("--stat");
    } else if !args.is_empty() && args != "all" && args != "session" {
        for a in args.split_whitespace() {
            cmd.arg(a);
        }
    }
    match cmd.output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            if s.trim().is_empty() {
                s = String::from("(no diff)");
            }
            if s.len() > 8000 {
                s.truncate(8000);
                s.push_str("\n…");
            }
            format!("**Diff**\n```\n{s}\n```")
        }
        Err(e) => format!("git diff failed: {e}"),
    }
}

pub fn last_real_user(events: &[SessionEvent]) -> Option<&str> {
    events.iter().rev().find_map(|e| match e {
        SessionEvent::User(u) if !is_hidden_user_text(&u.text) => Some(u.text.as_str()),
        _ => None,
    })
}

pub fn last_undo_range(events: &[SessionEvent]) -> Option<&UndoEvent> {
    events.iter().rev().find_map(|e| match e {
        SessionEvent::Undo(u) => Some(u),
        _ => None,
    })
}

pub fn undo_range(events: &[SessionEvent]) -> Option<(u64, u64)> {
    let mut from = None;
    for (i, e) in events.iter().enumerate().rev() {
        if skipped_by_undo(events, i as u64) {
            continue;
        }
        match e {
            SessionEvent::User(u) if !is_hidden_user_text(&u.text) => {
                from = Some(i as u64);
                break;
            }
            _ => {}
        }
    }
    let from = from?;
    let until = (events.len().saturating_sub(1)) as u64;
    (from <= until).then_some((from, until))
}

pub fn skipped_by_undo(events: &[SessionEvent], seq: u64) -> bool {
    events.iter().any(|e| match e {
        SessionEvent::Undo(u) => seq >= u.from_seq && seq <= u.until_seq,
        _ => false,
    })
}

fn last_assistant(events: &[SessionEvent]) -> Option<&str> {
    events.iter().rev().find_map(|e| match e {
        SessionEvent::Assistant(a) if !a.content.is_empty() => Some(a.content.as_str()),
        _ => None,
    })
}

fn tokens(family: Family, text: &str) -> u32 {
    count_tokens(family, text).unwrap_or_else(|_| (text.len() as u32 / 4).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_names_and_q38_depth() {
        assert_eq!(parse_slash("/think"), Some(SlashCmd::Think(Effort::Medium)));
        assert_eq!(parse_slash("/fast"), Some(SlashCmd::Off));
        assert_eq!(
            parse_slash("/mode think"),
            Some(SlashCmd::Mode(SessionMode::Think))
        );
        assert!(matches!(parse_slash("/help"), Some(SlashCmd::Help)));
        assert!(matches!(
            parse_slash("/compress keep decisions"),
            Some(SlashCmd::Compress { .. })
        ));
        assert!(matches!(
            parse_slash("/compact"),
            Some(SlashCmd::Compress { hint: None })
        ));
        assert!(matches!(
            parse_slash("/queue later"),
            Some(SlashCmd::Queue { .. })
        ));
        assert!(matches!(
            parse_slash("/q later"),
            Some(SlashCmd::Queue { .. })
        ));
        assert!(matches!(
            parse_slash("/steer focus auth"),
            Some(SlashCmd::Steer { .. })
        ));
        assert!(matches!(
            parse_slash("/busy queue"),
            Some(SlashCmd::Busy {
                policy: Some(BusyPolicy::Queue)
            })
        ));
        assert!(matches!(
            parse_slash("/sessions"),
            Some(SlashCmd::Sessions { search: None })
        ));
        assert!(matches!(
            parse_slash("/ctx all"),
            Some(SlashCmd::Context { all: true })
        ));
        assert!(matches!(
            parse_slash("/new my-exp --yes"),
            Some(SlashCmd::New { .. })
        ));
        assert!(matches!(parse_slash("/cron"), Some(SlashCmd::Cron { .. })));
        assert!(matches!(
            parse_slash("/cron add n 1h x"),
            Some(SlashCmd::Cron { .. })
        ));
        assert!(parse_slash("hello").is_none());
        assert!(parse_slash("/think nope").is_none());
        assert!(parse_slash("/not-a-command").is_none());
        assert_eq!(
            parse_slash("/reasoning xhigh"),
            Some(SlashCmd::Think(Effort::Xhigh))
        );
        assert!(matches!(
            parse_slash("/personality"),
            Some(SlashCmd::Unsupported { .. })
        ));
        assert_eq!(parse_slash("/mcp"), Some(SlashCmd::Mcp));
        assert_eq!(parse_slash("/setup"), Some(SlashCmd::Setup));
        assert_eq!(parse_slash("/model setup"), Some(SlashCmd::Setup));
        assert_eq!(
            parse_slash("/plan"),
            Some(SlashCmd::Plan {
                action: PlanAction::On
            })
        );
        assert_eq!(
            parse_slash("/plan go"),
            Some(SlashCmd::Plan {
                action: PlanAction::Go
            })
        );
        assert_eq!(
            parse_slash("/clarify"),
            Some(SlashCmd::Clarify { on: Some(true) })
        );
        assert_eq!(
            parse_slash("/clarify on"),
            Some(SlashCmd::Clarify { on: Some(true) })
        );
        assert_eq!(
            parse_slash("/clarify off"),
            Some(SlashCmd::Clarify { on: Some(false) })
        );
        assert_eq!(
            parse_slash("/yolo"),
            Some(SlashCmd::Approvals {
                mode: Some(ApprovalMode::Yolo)
            })
        );
        assert_eq!(
            parse_slash("/approvals ask"),
            Some(SlashCmd::Approvals {
                mode: Some(ApprovalMode::Ask)
            })
        );
        assert_eq!(
            parse_slash("/lossy"),
            Some(SlashCmd::LowPrecision { on: None })
        );
        assert_eq!(
            parse_slash("/lossy on"),
            Some(SlashCmd::LowPrecision { on: Some(true) })
        );
        assert_eq!(
            parse_slash("/lossy off"),
            Some(SlashCmd::LowPrecision { on: Some(false) })
        );
        match parse_slash("/mcp docs search lantern") {
            Some(SlashCmd::InvokeMcp { name, args }) => {
                assert_eq!(name, "docs");
                assert_eq!(args, "search lantern");
                assert_eq!(
                    crate::sticky::mcp_turn_prompt(&name, &args),
                    "[mcp:docs]\nsearch lantern"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn slash_skill_name_invokes_without_tool() {
        let cat = SkillCatalog {
            skills: vec![crate::skills::Skill {
                name: "pdf".into(),
                description: "Extract".into(),
                path: std::path::PathBuf::from("/tmp/pdf/SKILL.md"),
            }],
        };
        match parse_slash_with_skills("/pdf extract this", &cat) {
            Some(SlashCmd::InvokeSkill { name, args }) => {
                assert_eq!(name, "pdf");
                assert_eq!(args, "extract this");
                assert_eq!(
                    crate::sticky::skill_turn_prompt(&name, &args),
                    "[skill:pdf]\nextract this"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_slash("/pdf").is_none());
        let mcp = crate::mcp::McpRegistry::with_servers(
            vec![crate::mcp::McpServer {
                name: "docs".into(),
                command: "python3".into(),
                ..crate::mcp::McpServer::default()
            }],
            std::time::Duration::from_secs(5),
        );
        match parse_slash_with_periphery("/docs search lantern", &cat, Some(&mcp)) {
            Some(SlashCmd::InvokeMcp { name, args }) => {
                assert_eq!(name, "docs");
                assert_eq!(args, "search lantern");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_recap_hit_rate() {
        let events = vec![
            SessionEvent::user("hi"),
            SessionEvent::assistant_usage("a", "", None, 100, 10, Some(0), None),
            SessionEvent::assistant_usage("b", "", None, 200, 10, Some(190), None),
        ];
        let u = UsageRecap::from_events(&events);
        assert_eq!(u.prompt_tokens, 300);
        assert_eq!(u.last_prompt_tokens, 200);
        assert_eq!(u.cached_tokens, 190);
        assert_eq!(u.hit_pct(), Some(63.3));
        assert!(usage_text(&events).contains("63.3%"));

        let poisoned = vec![
            SessionEvent::assistant_usage("a", "", None, 0, 0, Some(900), None),
            SessionEvent::assistant_usage("b", "", None, 200, 10, Some(190), None),
        ];
        let u = UsageRecap::from_events(&poisoned);
        assert_eq!(u.cached_tokens, 190);
        assert_eq!(u.cache_prompt_tokens, 200);
        assert_eq!(u.hit_pct(), Some(95.0));
    }

    #[test]
    fn usage_first_hop_stuck_without_hardcoded_643() {
        let events = vec![
            SessionEvent::user("one"),
            SessionEvent::assistant_usage("a", "think-a", None, 100, 10, Some(0), None),
            SessionEvent::user("two"),
            SessionEvent::assistant_usage("b", "think-b", None, 2000, 10, Some(640), None),
            SessionEvent::user("three"),
            SessionEvent::assistant_usage("c", "think-c", None, 4000, 10, Some(643), None),
        ];
        let u = UsageRecap::from_events(&events);
        assert_eq!(u.first_hop_reported, 3);
        assert_eq!(u.first_hop_cached_tokens, 1283);
        assert_eq!(u.stuck_cached, vec![640, 643]);
        let note = u.prefix_note(None).expect("note");
        assert!(note.contains("historical think"), "{note}");
        assert!(usage_text(&events).contains("first-hop hit"));

        let mut after_compact = vec![
            SessionEvent::user("one"),
            SessionEvent::assistant_usage("a", "", None, 100, 10, Some(0), None),
            SessionEvent::Compact(crate::session::CompactEvent {
                until_seq: 1,
                keep_user_seq: 2,
                summary: String::new(),
                index: String::new(),
            }),
            SessionEvent::user("two"),
            SessionEvent::assistant_usage("b", "", None, 2000, 10, Some(640), None),
        ];
        let u = UsageRecap::from_events(&after_compact);
        assert!(u.stuck_cached.is_empty(), "{:?}", u.stuck_cached);
        after_compact.push(SessionEvent::user("three"));
        after_compact.push(SessionEvent::assistant_usage(
            "c",
            "",
            None,
            4000,
            10,
            Some(641),
            None,
        ));
        let u = UsageRecap::from_events(&after_compact);
        assert_eq!(u.stuck_cached, vec![641]);
        assert_eq!(u.compacts, 1);
        assert_eq!(u.last_prompt_tokens, 4000);
        assert_eq!(u.prompt_tokens, 6100);
    }

    #[test]
    fn usage_live_prefix_is_last_hop_not_session_sum() {
        let events = vec![
            SessionEvent::user("one"),
            SessionEvent::assistant_usage("a", "", None, 80_000, 10, Some(0), None),
            SessionEvent::Compact(crate::session::CompactEvent {
                until_seq: 1,
                keep_user_seq: 2,
                summary: String::new(),
                index: String::new(),
            }),
            SessionEvent::user("two"),
            SessionEvent::assistant_usage("b", "", None, 12_000, 20, Some(8_000), None),
        ];
        let u = UsageRecap::from_events(&events);
        assert_eq!(u.prompt_tokens, 92_000);
        assert_eq!(u.last_prompt_tokens, 12_000);
        assert_eq!(u.compacts, 1);
        let j = u.json();
        assert_eq!(j["live_prompt_tokens"].as_u64(), Some(12_000));
    }

    #[test]
    fn history_text_cjk_does_not_panic_on_clip() {
        // CJK chars are 3 bytes; a byte-index clip at max_chars/2 used to land
        // mid-char and panic ("byte index is not a char boundary").
        // 40 lines x 240-char clip = ~9.6k chars > 8000, forcing the body clip
        // at a byte offset that lands mid-CJK-char.
        let events: Vec<SessionEvent> = (0..40)
            .map(|i| {
                if i % 2 == 0 {
                    SessionEvent::user(format!("审查{:#04x}", i).repeat(120))
                } else {
                    SessionEvent::assistant(format!("问题{:#04x}", i).repeat(120), "", None)
                }
            })
            .collect();
        let out = history_text(&events, 8000);
        assert!(out.contains("..."), "{out}");
        assert!(out.starts_with("**History**"), "{out}");
    }
}
