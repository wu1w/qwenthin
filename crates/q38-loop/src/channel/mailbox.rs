//! Per-session inbound mailbox. Wash of QwenPaw `UnifiedQueueManager` + Hermes
//! `/busy` (interrupt / queue / steer).
//!
//! One mailbox per live session. Same session is serialized; different sessions
//! do not share the queue. IM adapters (when present) call [`Mailbox::offer`].

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::lock_unpoison;

/// What Enter does while a turn is running. Hermes `/busy`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Abort the live turn, then start this prompt (Hermes default).
    #[default]
    Interrupt,
    /// Run after the live turn finishes. Hermes `/queue`.
    Queue,
    /// Inject after the next tool result. Hermes `/steer`.
    Steer,
}

impl BusyPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }
}

impl FromStr for BusyPolicy {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "interrupt" => Ok(Self::Interrupt),
            "queue" | "q" => Ok(Self::Queue),
            "steer" => Ok(Self::Steer),
            other => Err(Error::msg(format!(
                "unknown busy policy '{other}' (interrupt|queue|steer)"
            ))),
        }
    }
}

/// How an inbound message was accepted while a turn is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyDecision {
    AbortThenRedirect,
    Queued,
    Steered,
}

/// Shared slot the agent drains after each tool round.
pub type SteerSlot = Arc<Mutex<Vec<String>>>;

#[derive(Debug)]
pub struct Mailbox {
    pub busy: BusyPolicy,
    queue: VecDeque<String>,
    steer: SteerSlot,
    redirect: Option<String>,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self {
            busy: BusyPolicy::Interrupt,
            queue: VecDeque::new(),
            steer: Arc::new(Mutex::new(Vec::new())),
            redirect: None,
        }
    }
}

impl Mailbox {
    pub fn steer_slot(&self) -> SteerSlot {
        self.steer.clone()
    }

    /// Idle inbound always starts a turn. Busy inbound follows [`BusyPolicy`].
    pub fn offer_while_busy(&mut self, text: String) -> BusyDecision {
        match self.busy {
            BusyPolicy::Interrupt => {
                self.redirect = Some(text);
                BusyDecision::AbortThenRedirect
            }
            BusyPolicy::Queue => {
                self.queue.push_back(text);
                BusyDecision::Queued
            }
            BusyPolicy::Steer => {
                push_steer(&self.steer, text);
                BusyDecision::Steered
            }
        }
    }

    pub fn push_queue(&mut self, text: String) {
        self.queue.push_back(text);
    }

    pub fn push_steer(&mut self, text: String) {
        push_steer(&self.steer, text);
    }

    pub fn has_redirect(&self) -> bool {
        self.redirect.is_some()
    }

    pub fn take_redirect(&mut self) -> Option<String> {
        self.redirect.take()
    }

    pub fn pop_queue(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn steered(&self) -> usize {
        lock_unpoison(&self.steer).len()
    }

    pub fn peek_queue(&self) -> Vec<String> {
        self.queue.iter().cloned().collect()
    }

    pub fn peek_steer(&self) -> Vec<String> {
        lock_unpoison(&self.steer).clone()
    }

    /// Leftover steer after a turn with no further tool boundary.
    pub fn take_unused_steer(&self) -> Vec<String> {
        take_steer(&self.steer)
    }

    pub fn clear_queue(&mut self) -> usize {
        let n = self.queue.len();
        self.queue.clear();
        n
    }
}

pub fn push_steer(slot: &SteerSlot, text: String) {
    if text.trim().is_empty() {
        return;
    }
    lock_unpoison(slot).push(text);
}

pub fn take_steer(slot: &SteerSlot) -> Vec<String> {
    std::mem::take(&mut *lock_unpoison(slot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_stashes_redirect() {
        let mut m = Mailbox::default();
        assert_eq!(
            m.offer_while_busy("fix auth".into()),
            BusyDecision::AbortThenRedirect
        );
        assert_eq!(m.take_redirect().as_deref(), Some("fix auth"));
    }

    #[test]
    fn queue_then_pop_fifo() {
        let mut m = Mailbox {
            busy: BusyPolicy::Queue,
            ..Mailbox::default()
        };
        m.offer_while_busy("a".into());
        m.offer_while_busy("b".into());
        assert_eq!(m.pop_queue().as_deref(), Some("a"));
        assert_eq!(m.pop_queue().as_deref(), Some("b"));
    }

    #[test]
    fn peek_does_not_consume_steer() {
        let mut m = Mailbox::default();
        m.push_steer("focus auth".into());
        assert_eq!(m.steered(), 1);
        assert_eq!(m.peek_steer(), vec!["focus auth".to_string()]);
        assert_eq!(m.steered(), 1);
        assert_eq!(m.take_unused_steer(), vec!["focus auth".to_string()]);
        assert_eq!(m.steered(), 0);
    }

    #[test]
    fn poisoned_steer_slot_recovers() {
        let m = Mailbox::default();
        let slot = m.steer_slot();
        let poisoner = slot.clone();
        std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the steer slot");
        })
        .join()
        .unwrap_err();
        assert!(slot.is_poisoned());

        push_steer(&slot, "still alive".into());
        assert_eq!(m.steered(), 1);
        assert_eq!(m.peek_steer(), vec!["still alive".to_string()]);
        assert_eq!(m.take_unused_steer(), vec!["still alive".to_string()]);
        assert_eq!(m.steered(), 0);
    }
}
