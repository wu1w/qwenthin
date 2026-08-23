//! Host-side cron / heartbeat. Timers fire `turn.start` — never a model tool.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use q38_loop::config::Config;
use q38_loop::cron;
pub use q38_loop::cron::CronJob;

fn default_interval() -> u64 {
    3600
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Heartbeat {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub last_run: Option<u64>,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_s: 3600,
            prompt: String::new(),
            last_run: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CronStore {
    #[serde(default)]
    pub jobs: Vec<CronJob>,
    #[serde(default)]
    pub heartbeat: Heartbeat,
}

pub fn later_ts(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, y) => y,
    }
}

pub fn merge_cron_jobs(old: &[CronJob], incoming: Vec<CronJob>) -> Vec<CronJob> {
    incoming
        .into_iter()
        .map(|mut j| {
            if let Some(prev) = old.iter().find(|p| p.id == j.id) {
                j.last_run = later_ts(prev.last_run, j.last_run);
            }
            j
        })
        .collect()
}

/// Apply UI edits without dropping jobs the form didn't list (agent / other tab).
pub fn overlay_cron_jobs(old: &[CronJob], incoming: Vec<CronJob>) -> Vec<CronJob> {
    let ids: std::collections::BTreeSet<String> = incoming.iter().map(|j| j.id.clone()).collect();
    let mut out = merge_cron_jobs(old, incoming);
    for prev in old {
        if !ids.contains(&prev.id) {
            out.push(prev.clone());
        }
    }
    out
}

pub fn upsert_cron_job(list: &mut Vec<CronJob>, mut add: CronJob) {
    if add.id.trim().is_empty() {
        add.id = format!("job-{}", now_s());
    }
    if let Some(prev) = list.iter().find(|p| p.id == add.id) {
        add.last_run = later_ts(prev.last_run, add.last_run);
    }
    if let Some(i) = list.iter().position(|p| p.id == add.id) {
        list[i] = add;
    } else {
        list.push(add);
    }
}

pub fn remove_cron_job(list: &mut Vec<CronJob>, id: &str) {
    list.retain(|j| j.id != id);
}

pub fn drop_workspace_job(workspace: &std::path::Path, id: &str) {
    let path = cron::workspace_path(workspace);
    let mut jobs = cron::load_jobs(&path);
    let before = jobs.len();
    cron::remove(&mut jobs, id);
    if jobs.len() != before {
        let _ = cron::save_jobs(&path, &jobs);
    }
}

impl CronStore {
    pub fn load() -> Self {
        let path = store_path();
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn reload(workspace: &std::path::Path, mem: &CronStore) -> Self {
        let mut disk = Self::load();
        disk.absorb_runtime(mem);
        disk.ingest_workspace(workspace);
        disk
    }

    /// Disk wins for job/heartbeat bodies; keep the newer `last_run` so a
    /// console save cannot rewind the timer.
    pub fn absorb_runtime(&mut self, mem: &CronStore) {
        for m in &mem.jobs {
            if let Some(d) = self.jobs.iter_mut().find(|j| j.id == m.id) {
                d.last_run = later_ts(d.last_run, m.last_run);
            }
        }
        self.heartbeat.last_run = later_ts(self.heartbeat.last_run, mem.heartbeat.last_run);
    }

    pub fn ingest_workspace(&mut self, workspace: &std::path::Path) {
        for job in cron::load_jobs(cron::workspace_path(workspace)) {
            upsert_cron_job(&mut self.jobs, job);
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = store_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// UI / hub jobs also land in workspace `.q38/cron.json` so ingest cannot
    /// clobber console edits with a stale agent file, and the model can `read` them.
    pub fn save_with_workspace(&self, workspace: &std::path::Path) -> Result<()> {
        self.save()?;
        cron::save_jobs(cron::workspace_path(workspace), &self.jobs)?;
        Ok(())
    }

    pub fn json(&self) -> serde_json::Value {
        json!({
            "ok": true,
            "jobs": self.jobs,
            "heartbeat": self.heartbeat,
        })
    }

    pub fn due(&self, now: u64) -> Vec<String> {
        let mut out = Vec::new();
        for job in &self.jobs {
            if job.enabled && job.interval_s > 0 {
                let last = job.last_run.unwrap_or(0);
                if now.saturating_sub(last) >= job.interval_s && !job.prompt.trim().is_empty() {
                    out.push(job.id.clone());
                }
            }
        }
        out
    }

    pub fn mark(&mut self, id: &str, now: u64) -> Option<String> {
        let job = self.jobs.iter_mut().find(|j| j.id == id)?;
        job.last_run = Some(now);
        Some(format!("[cron:{}] {}", job.name, job.prompt))
    }

    /// Revert a job's `last_run` after a failed cron turn. `mark` already
    /// stamped `now` so the job is not due while `live`; without this restore
    /// it would sit out a full `interval_s`.
    pub fn restore_last_run(&mut self, id: &str, prev: Option<u64>) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            job.last_run = prev;
        }
    }

    pub fn restore_heartbeat(&mut self, prev: Option<u64>) {
        self.heartbeat.last_run = prev;
    }

    pub fn defer_job(&mut self, id: &str, now: u64, delay_s: u64) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            job.last_run = Some(retry_last_run(now, job.interval_s, delay_s));
        }
    }

    pub fn defer_heartbeat(&mut self, now: u64, delay_s: u64) {
        let interval = self.heartbeat.interval_s;
        self.heartbeat.last_run = Some(retry_last_run(now, interval, delay_s));
    }

    pub fn heartbeat_due(&self, now: u64) -> bool {
        let hb = &self.heartbeat;
        hb.enabled
            && hb.interval_s > 0
            && now.saturating_sub(hb.last_run.unwrap_or(0)) >= hb.interval_s
    }
}

pub fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// After a failed cron/heartbeat turn, wait this long before firing again.
/// Immediate `restore_last_run` made a daily job retry every 1s tick while
/// GET /models was down, writing a stop-storm into the session log.
pub(crate) const CRON_RETRY_DELAY_S: u64 = 30;

/// `last_run` such that `due(now + delay)` becomes true without skipping the
/// rest of `interval_s`.
pub(crate) fn retry_last_run(now: u64, interval_s: u64, delay_s: u64) -> u64 {
    let interval = interval_s.max(1);
    let delay = delay_s.clamp(1, interval);
    now.saturating_sub(interval.saturating_sub(delay))
}

pub fn heartbeat_prompt(store: &CronStore, workspace: &std::path::Path) -> String {
    let p = store.heartbeat.prompt.trim();
    if !p.is_empty() {
        return format!("[heartbeat] {p}");
    }
    for name in ["HEARTBEAT.md", ".q38/HEARTBEAT.md"] {
        if let Ok(body) = fs::read_to_string(workspace.join(name)) {
            let body = body.trim();
            if !body.is_empty() {
                return format!("[heartbeat]\n{body}");
            }
        }
    }
    "[heartbeat] Check workspace status. Reply with a short note.".into()
}

fn store_path() -> PathBuf {
    Config::home_dir()
        .map(|h| h.join("web-cron.json"))
        .unwrap_or_else(|_| PathBuf::from("web-cron.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_jobs_keeps_newer_last_run() {
        let old = vec![CronJob {
            id: "a".into(),
            name: "old".into(),
            interval_s: 60,
            prompt: "x".into(),
            enabled: true,
            last_run: Some(200),
        }];
        let incoming = vec![CronJob {
            id: "a".into(),
            name: "new".into(),
            interval_s: 90,
            prompt: "y".into(),
            enabled: false,
            last_run: Some(50),
        }];
        let out = merge_cron_jobs(&old, incoming);
        assert_eq!(out[0].name, "new");
        assert_eq!(out[0].last_run, Some(200));
    }

    #[test]
    fn failed_turn_reverts_last_run_and_is_due_again() {
        let now = 1000u64;
        let mut store = CronStore {
            jobs: vec![CronJob {
                id: "nightly".into(),
                name: "nightly".into(),
                interval_s: 60,
                prompt: "run".into(),
                enabled: true,
                last_run: Some(100),
            }],
            ..Default::default()
        };
        assert_eq!(store.due(now), vec!["nightly".to_string()]);
        let prev = store
            .jobs
            .iter()
            .find(|j| j.id == "nightly")
            .and_then(|j| j.last_run);
        store.mark("nightly", now);
        assert_eq!(store.jobs[0].last_run, Some(now));
        assert!(store.due(now).is_empty());
        store.restore_last_run("nightly", prev);
        assert_eq!(store.jobs[0].last_run, prev);
        assert_eq!(store.due(now + 1), vec!["nightly".to_string()]);
    }

    #[test]
    fn failed_turn_on_never_run_job_is_due_again() {
        let now = 500u64;
        let mut store = CronStore {
            jobs: vec![CronJob {
                id: "fresh".into(),
                name: "fresh".into(),
                interval_s: 60,
                prompt: "run".into(),
                enabled: true,
                last_run: None,
            }],
            ..Default::default()
        };
        let prev = store
            .jobs
            .iter()
            .find(|j| j.id == "fresh")
            .and_then(|j| j.last_run);
        store.mark("fresh", now);
        assert!(store.due(now).is_empty());
        store.restore_last_run("fresh", prev);
        assert!(store.due(now + 1).contains(&"fresh".to_string()));
    }

    #[test]
    fn restore_last_run_noop_when_job_gone() {
        let mut store = CronStore {
            jobs: vec![CronJob {
                id: "a".into(),
                name: "a".into(),
                interval_s: 60,
                prompt: "x".into(),
                enabled: true,
                last_run: Some(1),
            }],
            ..Default::default()
        };
        store.restore_last_run("missing", Some(0));
        assert_eq!(store.jobs[0].last_run, Some(1));
    }

    #[test]
    fn failed_heartbeat_reverts_last_run_and_is_due_again() {
        let now = 1000u64;
        let mut store = CronStore {
            heartbeat: Heartbeat {
                enabled: true,
                interval_s: 60,
                prompt: "pulse".into(),
                last_run: Some(100),
            },
            ..Default::default()
        };
        assert!(store.heartbeat_due(now));
        let prev = store.heartbeat.last_run;
        store.heartbeat.last_run = Some(now);
        assert!(!store.heartbeat_due(now));
        store.restore_heartbeat(prev);
        assert_eq!(store.heartbeat.last_run, prev);
        assert!(store.heartbeat_due(now + 1));
    }

    #[test]
    fn retry_last_run_is_due_after_delay_not_immediately() {
        let now = 1_000_000u64;
        let last = retry_last_run(now, 86_400, 30);
        let due = |t: u64, last: u64| t.saturating_sub(last) >= 86_400;
        assert!(!due(now, last));
        assert!(!due(now + 29, last));
        assert!(due(now + 30, last));
    }

    #[test]
    fn defer_job_waits_delay_then_fires() {
        let now = 1000u64;
        let mut store = CronStore {
            jobs: vec![CronJob {
                id: "nightly".into(),
                name: "nightly".into(),
                interval_s: 60,
                prompt: "run".into(),
                enabled: true,
                last_run: Some(100),
            }],
            ..Default::default()
        };
        store.mark("nightly", now);
        store.defer_job("nightly", now, 30);
        assert!(store.due(now).is_empty());
        assert!(store.due(now + 29).is_empty());
        assert_eq!(store.due(now + 30), vec!["nightly".to_string()]);
    }

    #[test]
    fn defer_heartbeat_waits_delay_then_fires() {
        let now = 1000u64;
        let mut store = CronStore {
            heartbeat: Heartbeat {
                enabled: true,
                interval_s: 60,
                prompt: "pulse".into(),
                last_run: Some(100),
            },
            ..Default::default()
        };
        store.heartbeat.last_run = Some(now);
        store.defer_heartbeat(now, 30);
        assert!(!store.heartbeat_due(now));
        assert!(!store.heartbeat_due(now + 29));
        assert!(store.heartbeat_due(now + 30));
    }

    #[test]
    fn overlay_keeps_unlisted_jobs() {
        let old = vec![
            CronJob {
                id: "a".into(),
                name: "keep".into(),
                interval_s: 60,
                prompt: "x".into(),
                enabled: true,
                last_run: None,
            },
            CronJob {
                id: "b".into(),
                name: "edit".into(),
                interval_s: 60,
                prompt: "y".into(),
                enabled: true,
                last_run: Some(1),
            },
        ];
        let incoming = vec![CronJob {
            id: "b".into(),
            name: "edited".into(),
            interval_s: 90,
            prompt: "z".into(),
            enabled: false,
            last_run: None,
        }];
        let out = overlay_cron_jobs(&old, incoming);
        assert_eq!(out.len(), 2);
        assert_eq!(out.iter().find(|j| j.id == "a").unwrap().name, "keep");
        let b = out.iter().find(|j| j.id == "b").unwrap();
        assert_eq!(b.name, "edited");
        assert_eq!(b.last_run, Some(1));
    }

    #[test]
    fn upsert_does_not_drop_siblings() {
        let mut list = vec![CronJob {
            id: "a".into(),
            name: "keep".into(),
            interval_s: 60,
            prompt: "x".into(),
            enabled: true,
            last_run: None,
        }];
        upsert_cron_job(
            &mut list,
            CronJob {
                id: "b".into(),
                name: "add".into(),
                interval_s: 60,
                prompt: "y".into(),
                enabled: false,
                last_run: None,
            },
        );
        assert_eq!(list.len(), 2);
        remove_cron_job(&mut list, "b");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "a");
    }

    #[test]
    fn ingest_workspace_cron_json() {
        let dir = std::env::temp_dir().join(format!("q38-web-cron-{}", now_s()));
        fs::create_dir_all(dir.join(".q38")).unwrap();
        fs::write(
            dir.join(".q38/cron.json"),
            r#"{"jobs":[{"id":"nightly","name":"nightly","interval_s":60,"prompt":"跑测试","enabled":true}]}"#,
        )
        .unwrap();
        let mut store = CronStore::default();
        store.ingest_workspace(&dir);
        assert_eq!(store.jobs.len(), 1);
        assert_eq!(store.jobs[0].id, "nightly");
        assert!(store.jobs[0].enabled);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn workspace_file_can_hold_ui_jobs() {
        let dir = std::env::temp_dir().join(format!("q38-web-cron-ws-{}", now_s()));
        fs::create_dir_all(dir.join(".q38")).unwrap();
        let jobs = vec![CronJob {
            id: "ui".into(),
            name: "from-ui".into(),
            interval_s: 60,
            prompt: "hello".into(),
            enabled: true,
            last_run: None,
        }];
        cron::save_jobs(dir.join(".q38/cron.json"), &jobs).unwrap();
        let loaded = cron::load_jobs(dir.join(".q38/cron.json"));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "ui");
        assert_eq!(loaded[0].prompt, "hello");
        let _ = fs::remove_dir_all(dir);
    }
}
