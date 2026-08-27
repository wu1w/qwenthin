//! Persist `route_key` → session JSONL id. Same sender/chat keeps one session.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::session::new_session_id;

use super::envelope::NativePayload;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RoutesFile {
    #[serde(default)]
    routes: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct SessionRouter {
    path: PathBuf,
    map: BTreeMap<String, String>,
    /// Keys this process wrote. Flush overlays only these onto a locked read
    /// of the file so WeChat and QQ cannot clobber each other.
    dirty: BTreeMap<String, String>,
}

impl SessionRouter {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let map = if path.is_file() {
            let raw = fs::read_to_string(&path)?;
            serde_json::from_str::<RoutesFile>(&raw)
                .map(|f| f.routes)
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path,
            map,
            dirty: BTreeMap::new(),
        })
    }

    pub fn in_home() -> Result<Self> {
        let dir = crate::config::Config::home_dir()?.join("channels");
        Self::open(dir.join("routes.json"))
    }

    pub fn resolve(&mut self, env: &NativePayload) -> Result<String> {
        if !env.session_id.is_empty() {
            self.touch(env.route_key(), env.session_id.clone());
            self.flush()?;
            return Ok(env.session_id.clone());
        }
        let key = env.route_key();
        if let Some(id) = self.map.get(&key) {
            return Ok(id.clone());
        }
        let id = new_session_id();
        self.touch(key, id.clone());
        self.flush()?;
        Ok(id)
    }

    pub fn lookup(&self, route_key: &str) -> Option<&str> {
        self.map.get(route_key).map(|s| s.as_str())
    }

    fn touch(&mut self, key: String, id: String) {
        self.map.insert(key.clone(), id.clone());
        self.dirty.insert(key, id);
    }

    fn flush(&self) -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut file = opts.open(&self.path)?;
        file.lock_exclusive()?;
        let result = (|| {
            let disk = read_routes(&mut file)?;
            let mut merged = disk;
            for (k, v) in &self.dirty {
                merged.insert(k.clone(), v.clone());
            }
            let body =
                serde_json::to_string_pretty(&RoutesFile { routes: merged }).map_err(Error::msg)?;
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        let _ = file.unlock();
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }
        result
    }
}

fn read_routes(file: &mut File) -> Result<BTreeMap<String, String>> {
    file.seek(SeekFrom::Start(0))?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    Ok(serde_json::from_str::<RoutesFile>(&raw)
        .map(|f| f.routes)
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::new_session_id;

    #[test]
    fn same_sender_reuses_session() {
        let dir = std::env::temp_dir().join(format!("q38-rt-{}", new_session_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("routes.json");
        let mut r = SessionRouter::open(&path).unwrap();
        let env = NativePayload::text_only("telegram", "hi").tap_sender("7");
        let a = r.resolve(&env).unwrap();
        let b = r.resolve(&env).unwrap();
        assert_eq!(a, b);
        let mut r2 = SessionRouter::open(&path).unwrap();
        assert_eq!(r2.resolve(&env).unwrap(), a);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn flush_merges_other_managers_keys() {
        let dir = std::env::temp_dir().join(format!("q38-rt-{}", new_session_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("routes.json");
        let mut wechat = SessionRouter::open(&path).unwrap();
        let mut qq = SessionRouter::open(&path).unwrap();
        let wx = NativePayload::text_only("wechat", "hi").tap_sender("wx-1");
        let qq_env = NativePayload::text_only("qq", "hi").tap_sender("qq-1");
        let wx_id = wechat.resolve(&wx).unwrap();
        let qq_id = qq.resolve(&qq_env).unwrap();
        let wx2 = NativePayload::text_only("wechat", "again").tap_sender("wx-2");
        wechat.resolve(&wx2).unwrap();
        let disk = SessionRouter::open(&path).unwrap();
        assert_eq!(disk.lookup(&wx.route_key()), Some(wx_id.as_str()));
        assert_eq!(disk.lookup(&qq_env.route_key()), Some(qq_id.as_str()));
        assert!(disk.lookup(&wx2.route_key()).is_some());
        let _ = fs::remove_dir_all(dir);
    }

    trait Tap {
        fn tap_sender(self, id: &str) -> Self;
    }
    impl Tap for NativePayload {
        fn tap_sender(mut self, id: &str) -> Self {
            self.sender_id = id.into();
            self
        }
    }
}
