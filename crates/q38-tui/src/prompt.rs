//! Composer. Grok pager `PromptWidget`: bare Enter submits, trailing `\` continues.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Debug, Default)]
pub struct Prompt {
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    None,
    Submit,
    Cancel,
    Quit,
    Scroll { delta: i32 },
    Follow,
    ToggleThink,
    Interrupt,
}

impl Prompt {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert_str(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| *c != '\r').collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    pub fn handle_key(&mut self, key: KeyEvent, turn_running: bool) -> PromptAction {
        if key.kind != crossterm::event::KeyEventKind::Press
            && key.kind != crossterm::event::KeyEventKind::Repeat
        {
            return PromptAction::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if turn_running {
                    PromptAction::Interrupt
                } else if self.is_empty() {
                    PromptAction::Quit
                } else {
                    self.clear();
                    PromptAction::None
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) if self.is_empty() => PromptAction::Quit,
            (KeyCode::Esc, _) => {
                if turn_running {
                    PromptAction::Interrupt
                } else {
                    PromptAction::Cancel
                }
            }
            (KeyCode::Char('e'), KeyModifiers::CONTROL) => PromptAction::ToggleThink,
            (KeyCode::Enter, KeyModifiers::ALT) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.insert_str("\n");
                PromptAction::None
            }
            (KeyCode::Enter, _) => {
                if self.text.ends_with('\\') {
                    self.text.pop();
                    self.cursor = self.text.len();
                    self.insert_str("\n");
                    PromptAction::None
                } else if self.text.trim().is_empty() {
                    PromptAction::None
                } else {
                    PromptAction::Submit
                }
            }
            (KeyCode::Backspace, _) => {
                self.delete_back();
                PromptAction::None
            }
            (KeyCode::Left, _) => {
                self.left();
                PromptAction::None
            }
            (KeyCode::Right, _) => {
                self.right();
                PromptAction::None
            }
            (KeyCode::Home, _) => {
                self.cursor = 0;
                PromptAction::None
            }
            (KeyCode::End, _) => {
                self.cursor = self.text.len();
                PromptAction::None
            }
            (KeyCode::Up, KeyModifiers::SHIFT) => PromptAction::Scroll { delta: -1 },
            (KeyCode::Down, KeyModifiers::SHIFT) => PromptAction::Scroll { delta: 1 },
            (KeyCode::PageUp, _) => PromptAction::Scroll { delta: -10 },
            (KeyCode::PageDown, _) => PromptAction::Scroll { delta: 10 },
            (KeyCode::Char('g'), KeyModifiers::SHIFT) => PromptAction::Follow,
            (KeyCode::Char(c), m)
                if m.is_empty() || m == KeyModifiers::SHIFT || m == KeyModifiers::NONE =>
            {
                self.insert_str(&c.to_string());
                PromptAction::None
            }
            _ => PromptAction::None,
        }
    }

    fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .chars()
            .next_back()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        let start = self.cursor - prev;
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .chars()
            .next_back()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        self.cursor -= prev;
    }

    fn right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        self.cursor += next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn enter_submits_backslash_continues() {
        let mut p = Prompt::default();
        p.insert_str("hello\\");
        assert_eq!(
            p.handle_key(press(KeyCode::Enter), false),
            PromptAction::None
        );
        assert_eq!(p.text(), "hello\n");
        p.insert_str("world");
        assert_eq!(
            p.handle_key(press(KeyCode::Enter), false),
            PromptAction::Submit
        );
    }
}
