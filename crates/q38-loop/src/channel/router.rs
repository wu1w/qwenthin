//! Persist `route_key` → session JSONL id. Same sender/chat keeps one session.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::session::new_session_id;

use super::envelope::NativePayload;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RoutesFile {
    #[serde(default)]
    routes: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct SessionRouter {
    path: PathBuf,
    map: BTreeMap<String, String>,
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
        Ok(Self { path, map })
    }

    pub fn in_home() -> Result<Self> {
        let dir = crate::config::Config::home_dir()?.join("channels");
        Self::open(dir.join("routes.json"))
    }

    pub fn resolve(&mut self, env: &NativePayload) -> Result<String> {
        if !env.session_id.is_empty() {
            self.map.insert(env.route_key(), env.session_id.clone());
            self.flush()?;
            return Ok(env.session_id.clone());
        }
        let key = env.route_key();
        if let Some(id) = self.map.get(&key) {
            return Ok(id.clone());
        }
        let id = new_session_id();
        self.map.insert(key, id.clone());
        self.flush()?;
        Ok(id)
    }

    pub fn lookup(&self, route_key: &str) -> Option<&str> {
        self.map.get(route_key).map(|s| s.as_str())
    }

    fn flush(&self) -> Result<()> {
        let body = serde_json::to_string_pretty(&RoutesFile {
            routes: self.map.clone(),
        })
        .map_err(Error::msg)?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, body)?;
        fs::rename(&tmp, &self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
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
