//! Exclusive flock so `q38 web` and `q38 --channels` cannot both long-poll
//! the same iLink / Telegram / gateway endpoint (they fight over the cursor).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::config::Config;
use crate::error::{Error, Result};

/// Held for the lifetime of a poll/gateway loop. Unlock on drop.
pub struct PollLock(Option<File>);

impl Drop for PollLock {
    fn drop(&mut self) {
        if let Some(file) = self.0.take() {
            let _ = file.unlock();
        }
    }
}

/// Long-poll / gateway kinds that break if two processes share one bot.
pub fn needs_exclusive(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "telegram" | "wechat" | "qq" | "wecom" | "dingtalk" | "feishu"
    )
}

pub fn acquire(kind: &str, id: &str) -> Result<PollLock> {
    if !needs_exclusive(kind) {
        return Ok(PollLock(None));
    }
    let dir = Config::home_dir()
        .map(|h| h.join("channels"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/q38-channels"));
    acquire_in(&dir, kind, id)
}

pub fn acquire_in(dir: &Path, kind: &str, id: &str) -> Result<PollLock> {
    if !needs_exclusive(kind) {
        return Ok(PollLock(None));
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("{id}.poll.lock"));
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path)?;
    if file.try_lock_exclusive().is_err() {
        let holder = std::fs::read_to_string(&path).unwrap_or_default();
        let holder = holder.trim();
        let extra = if holder.is_empty() {
            String::new()
        } else {
            format!(" (pid {holder})")
        };
        return Err(Error::msg(format!(
            "another q38 process is already polling {kind} `{id}`{extra}; stop `q38 web` or `q38 --channels` so they do not share a cursor"
        )));
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    let _ = file.flush();
    Ok(PollLock(Some(file)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_skips_lock_file() {
        let dir = std::env::temp_dir().join(format!("q38-lock-{}", uuid::Uuid::new_v4().simple()));
        let lock = acquire_in(&dir, "webhook", "wh").unwrap();
        drop(lock);
        assert!(!dir.join("wh.poll.lock").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wechat_writes_pid() {
        let dir = std::env::temp_dir().join(format!("q38-lock-{}", uuid::Uuid::new_v4().simple()));
        let _lock = acquire_in(&dir, "wechat", "bot").unwrap();
        let body = std::fs::read_to_string(dir.join("bot.poll.lock")).unwrap();
        assert_eq!(body.trim(), std::process::id().to_string());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exclusive_kinds() {
        assert!(needs_exclusive("wechat"));
        assert!(needs_exclusive("Telegram"));
        assert!(!needs_exclusive("webhook"));
        assert!(!needs_exclusive("console"));
    }
}
