//! Per-session state map. Replaces Python `LoopGate._sessions` + contextvar.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::lock_unpoison;

pub struct SessionMap<T> {
    inner: Mutex<HashMap<String, T>>,
}

impl<T> Default for SessionMap<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> SessionMap<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, session: &str, value: T) {
        lock_unpoison(&self.inner).insert(session.to_string(), value);
    }

    pub fn remove(&self, session: &str) {
        lock_unpoison(&self.inner).remove(session);
    }

    pub fn modify<R>(&self, session: &str, f: impl FnOnce(Option<&mut T>) -> R) -> R {
        let mut g = lock_unpoison(&self.inner);
        f(g.get_mut(session))
    }

    pub fn get_or_insert_with<R>(
        &self,
        session: &str,
        init: impl FnOnce() -> T,
        f: impl FnOnce(&mut T) -> R,
    ) -> R {
        let mut g = lock_unpoison(&self.inner);
        let slot = g.entry(session.to_string()).or_insert_with(init);
        f(slot)
    }
}
