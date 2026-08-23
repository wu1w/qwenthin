use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::policy::ThinkPolicy;
use crate::session::event::{SessionEvent, SessionStart};
use crate::session::index::{HistoryIndex, Hit};
use crate::session::{derive_messages, live_policy};
use crate::template::ChatMessage;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Append-only JSONL at `~/.q38-agent/sessions/<id>.jsonl` (mode 0600, exclusive flock on write).
pub struct SessionLog {
    dir: PathBuf,
    id: String,
    path: PathBuf,
    events: Vec<SessionEvent>,
    index: Option<HistoryIndex>,
}

impl SessionLog {
    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(Config::home_dir()?.join("sessions"))
    }

    pub fn create(start: SessionStart) -> Result<Self> {
        Self::create_in(Self::sessions_dir()?, start)
    }

    pub fn open(id: &str) -> Result<Self> {
        Self::open_in(Self::sessions_dir()?, id)
    }

    pub fn create_in(dir: impl AsRef<Path>, start: SessionStart) -> Result<Self> {
        let dir = dir.as_ref();
        ensure_dir(dir)?;
        let path = jsonl_path(dir, &start.id);
        if path.exists() {
            return Err(Error::msg(format!(
                "session already exists: {}",
                path.display()
            )));
        }
        let mut log = Self {
            dir: dir.to_path_buf(),
            id: start.id.clone(),
            path,
            events: Vec::new(),
            index: HistoryIndex::open(dir).ok(),
        };
        log.write_event(SessionEvent::Start(start))?;
        Ok(log)
    }

    pub fn open_in(dir: impl AsRef<Path>, id: impl AsRef<str>) -> Result<Self> {
        let dir = dir.as_ref();
        let id = id.as_ref().to_string();
        let path = jsonl_path(dir, &id);
        let events = read_jsonl(&path)?;
        if !matches!(events.first(), Some(SessionEvent::Start(_))) {
            return Err(Error::msg(format!(
                "session JSONL missing session/start as events[0]: {}",
                path.display()
            )));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            id,
            path,
            events,
            index: HistoryIndex::open(dir).ok(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    pub fn start(&self) -> Option<&SessionStart> {
        match self.events.first() {
            Some(SessionEvent::Start(s)) => Some(s),
            _ => None,
        }
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        derive_messages(&self.events)
    }

    pub fn policy(&self) -> Option<ThinkPolicy> {
        live_policy(&self.events)
    }

    pub fn append(&mut self, event: SessionEvent) -> Result<()> {
        if event.is_ephemeral() {
            return Ok(());
        }
        if matches!(event, SessionEvent::Start(_)) {
            return Err(Error::msg(
                "session/start must be events[0]; append a policy event instead of a second start",
            ));
        }
        self.write_event(event)
    }

    /// Copy this JSONL to `start.id`, replace events[0], then append `session/fork`.
    /// Depth/`policy` events stay in the new file. `/think` `/fast` must not call this.
    pub fn fork(&self, start: SessionStart) -> Result<Self> {
        if start.id == self.id {
            return Err(Error::msg("fork requires a new session id"));
        }
        if self.events.is_empty() || !matches!(self.events[0], SessionEvent::Start(_)) {
            return Err(Error::msg("cannot fork: missing session/start"));
        }
        let mut copied = self.events.clone();
        copied[0] = SessionEvent::Start(start.clone());

        ensure_dir(&self.dir)?;
        let path = jsonl_path(&self.dir, &start.id);
        if path.exists() {
            return Err(Error::msg(format!(
                "session already exists: {}",
                path.display()
            )));
        }
        write_jsonl(&path, &copied)?;

        let mut log = Self {
            dir: self.dir.clone(),
            id: start.id,
            path,
            events: copied,
            index: HistoryIndex::open(&self.dir).ok(),
        };
        if let Some(index) = &log.index {
            let _ = index.reindex_session(&log.id, &log.events);
        }
        log.append(SessionEvent::fork(self.id.clone()))?;
        Ok(log)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        match &self.index {
            Some(index) => index.search(query, Some(&self.id), limit),
            None => Ok(Vec::new()),
        }
    }

    fn write_event(&mut self, event: SessionEvent) -> Result<()> {
        append_jsonl(&self.path, &event)?;
        self.events.push(event);
        if let Some(index) = &self.index {
            let seq = (self.events.len() - 1) as i64;
            let _ = index.upsert(&self.id, seq, self.events.last().unwrap());
        }
        Ok(())
    }
}

fn jsonl_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

fn encode_line(event: &SessionEvent) -> Result<String> {
    let mut line = serde_json::to_string(event).map_err(Error::msg)?;
    line.push('\n');
    Ok(line)
}

fn ensure_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn set_file_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn open_rw_create(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true).append(true);
    #[cfg(unix)]
    opts.mode(0o600);
    Ok(opts.open(path)?)
}

fn append_jsonl(path: &Path, event: &SessionEvent) -> Result<()> {
    let mut file = open_rw_create(path)?;
    set_file_mode(path)?;
    file.lock_exclusive()?;
    let result = (|| {
        file.write_all(encode_line(event)?.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    let _ = file.unlock();
    result
}

fn write_jsonl(path: &Path, events: &[SessionEvent]) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.create_new(true).read(true).write(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(path)?;
    set_file_mode(path)?;
    file.lock_exclusive()?;
    let result = (|| {
        for event in events {
            file.write_all(encode_line(event)?.as_bytes())?;
        }
        file.sync_all()?;
        Ok(())
    })();
    let _ = file.unlock();
    result
}

fn read_jsonl(path: &Path) -> Result<Vec<SessionEvent>> {
    let file = File::open(path)?;
    file.lock_shared()?;
    let result = (|| {
        let mut events = Vec::new();
        for (i, line) in BufReader::new(&file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: SessionEvent = serde_json::from_str(&line)
                .map_err(|e| Error::msg(format!("{}:{}: {e}", path.display(), i + 1)))?;
            events.push(event);
        }
        Ok(events)
    })();
    let _ = file.unlock();
    result
}
