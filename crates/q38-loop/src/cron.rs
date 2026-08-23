//! Console scheduled jobs. Not an OpenAI tool — the agent writes
//! `{workspace}/.q38/cron.json`, and `q38 web` fires `turn.start`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const WORKSPACE_REL: &str = ".q38/cron.json";

/// One line in the frozen-ish agent system blob (console schedule surface).
pub const CRON_SYSTEM_LINE: &str = "\
定时任务：写 `.q38/cron.json`（jobs: id, name, interval_s, prompt, enabled；interval_s 为秒）。\
会出现在控制台「定时任务」。不要用系统 crontab。";

/// Hidden user card when the live query is about scheduling.
pub const CRON_CARD: &str = "\
[console-cron]
定时任务写工作区 `.q38/cron.json`，不要用系统 crontab。
格式：{\"jobs\":[{\"id\":\"name\",\"name\":\"name\",\"interval_s\":3600,\"prompt\":\"要做的事\",\"enabled\":true}]}
interval_s 是秒，不是五段 cron 表达式。写完会出现在控制台「定时任务」。";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub prompt: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run: Option<u64>,
}

fn default_interval() -> u64 {
    3600
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileShape {
    #[serde(default)]
    jobs: Vec<CronJob>,
}

pub fn workspace_path(workspace: impl AsRef<Path>) -> PathBuf {
    workspace.as_ref().join(WORKSPACE_REL)
}

pub fn wants_cron_card(user: &str) -> bool {
    let t = user.to_ascii_lowercase();
    [
        "cron",
        "crontab",
        "interval_s",
        "定时",
        "定时任务",
        "每隔",
        "每小时",
        "web-cron",
        ".q38/cron",
    ]
    .iter()
    .any(|k| t.contains(k))
}

pub fn load_jobs(path: impl AsRef<Path>) -> Vec<CronJob> {
    let Ok(raw) = fs::read_to_string(path.as_ref()) else {
        return Vec::new();
    };
    parse_jobs_json(&raw)
}

pub fn parse_jobs_json(raw: &str) -> Vec<CronJob> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    if let Ok(file) = serde_json::from_str::<FileShape>(raw) {
        return file.jobs;
    }
    serde_json::from_str::<Vec<CronJob>>(raw).unwrap_or_default()
}

pub fn save_jobs(path: impl AsRef<Path>, jobs: &[CronJob]) -> Result<()> {
    let path = path.as_ref();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = FileShape {
        jobs: jobs.to_vec(),
    };
    fs::write(
        path,
        serde_json::to_string_pretty(&file).map_err(Error::msg)?,
    )?;
    Ok(())
}

pub fn upsert(list: &mut Vec<CronJob>, mut add: CronJob) {
    if add.id.trim().is_empty() {
        add.id = slug(&add.name);
    }
    if add.name.trim().is_empty() {
        add.name = add.id.clone();
    }
    if add.interval_s == 0 {
        add.interval_s = default_interval();
    }
    if let Some(prev) = list.iter().find(|p| p.id == add.id) {
        add.last_run = match (prev.last_run, add.last_run) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
    }
    if let Some(i) = list.iter().position(|p| p.id == add.id) {
        list[i] = add;
    } else {
        list.push(add);
    }
}

pub fn remove(list: &mut Vec<CronJob>, id: &str) {
    list.retain(|j| j.id != id && j.name != id);
}

/// `/cron` and `/cron add name interval prompt…` / `/cron rm id`.
pub fn apply_slash(workspace: impl AsRef<Path>, args: &str) -> String {
    let path = workspace_path(workspace);
    let mut jobs = load_jobs(&path);
    let args = args.trim();
    if args.is_empty() || args.eq_ignore_ascii_case("list") {
        return list_text(&path, &jobs);
    }
    let (cmd, rest) = match args.split_once(char::is_whitespace) {
        Some((c, r)) => (c.to_ascii_lowercase(), r.trim().to_string()),
        None => (args.to_ascii_lowercase(), String::new()),
    };
    match cmd.as_str() {
        "add" => match parse_add(&rest) {
            Ok(job) => {
                let id = job.id.clone();
                upsert(&mut jobs, job);
                if let Err(e) = save_jobs(&path, &jobs) {
                    return format!("cron: write failed ({e})");
                }
                format!("cron: saved `{id}` → {}", path.display())
            }
            Err(e) => e,
        },
        "rm" | "remove" | "delete" => {
            if rest.is_empty() {
                return "cron: /cron rm <id>".into();
            }
            let before = jobs.len();
            remove(&mut jobs, &rest);
            if jobs.len() == before {
                return format!("cron: no job `{rest}`");
            }
            if let Err(e) = save_jobs(&path, &jobs) {
                return format!("cron: write failed ({e})");
            }
            format!("cron: removed `{rest}`")
        }
        _ => list_text(&path, &jobs),
    }
}

fn list_text(path: &Path, jobs: &[CronJob]) -> String {
    let mut s = format!(
        "定时任务 {}\n/cron add <name> <seconds|30m|1h|1d> <prompt>\n/cron rm <id>\n\n",
        path.display()
    );
    if jobs.is_empty() {
        s.push_str("(empty — write this file or use /cron add)\n");
        return s;
    }
    for j in jobs {
        s.push_str(&format!(
            "- {}  {}s  {}  {}\n",
            j.id,
            j.interval_s,
            if j.enabled { "on" } else { "off" },
            if j.prompt.is_empty() {
                "(no prompt)"
            } else {
                j.prompt.as_str()
            }
        ));
    }
    s
}

fn parse_add(rest: &str) -> std::result::Result<CronJob, String> {
    let mut parts = rest.splitn(3, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let interval = parts.next().unwrap_or("").trim();
    let prompt = parts.next().unwrap_or("").trim();
    if name.is_empty() || interval.is_empty() || prompt.is_empty() {
        return Err("cron: /cron add <name> <seconds|30m|1h|1d> <prompt>".into());
    }
    let interval_s = parse_interval(interval)
        .ok_or_else(|| format!("cron: bad interval `{interval}` (use seconds, 30m, 1h, 1d)"))?;
    Ok(CronJob {
        id: slug(name),
        name: name.to_string(),
        interval_s,
        prompt: prompt.to_string(),
        enabled: true,
        last_run: None,
    })
}

pub fn parse_interval(raw: &str) -> Option<u64> {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let (n, mult) = if let Some(n) = t.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = t.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = t.strip_suffix('d') {
        (n, 86400)
    } else {
        (t.as_str(), 1)
    };
    n.parse::<u64>().ok().filter(|v| *v > 0).map(|v| v * mult)
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "job".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_array_or_object() {
        let a = parse_jobs_json(
            r#"[{"id":"n","name":"n","interval_s":60,"prompt":"x","enabled":true}]"#,
        );
        assert_eq!(a[0].id, "n");
        let b = parse_jobs_json(r#"{"jobs":[{"id":"m","prompt":"y","enabled":true}]}"#);
        assert_eq!(b[0].id, "m");
        assert_eq!(b[0].interval_s, 3600);
    }

    #[test]
    fn interval_suffixes() {
        assert_eq!(parse_interval("60"), Some(60));
        assert_eq!(parse_interval("30m"), Some(1800));
        assert_eq!(parse_interval("1h"), Some(3600));
        assert_eq!(parse_interval("1d"), Some(86400));
        assert_eq!(parse_interval("0"), None);
    }

    #[test]
    fn slash_add_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("q38-cron-{}", crate::session::new_session_id()));
        fs::create_dir_all(&dir).unwrap();
        let msg = apply_slash(&dir, "add nightly 1h 跑 clippy");
        assert!(msg.contains("nightly"), "{msg}");
        let jobs = load_jobs(workspace_path(&dir));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].interval_s, 3600);
        assert!(jobs[0].enabled);
        assert_eq!(jobs[0].prompt, "跑 clippy");
        apply_slash(&dir, "rm nightly");
        assert!(load_jobs(workspace_path(&dir)).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn card_keywords() {
        assert!(wants_cron_card("帮我写个 cron"));
        assert!(wants_cron_card("加一个定时任务"));
        assert!(!wants_cron_card("修一下编译错误"));
    }
}
