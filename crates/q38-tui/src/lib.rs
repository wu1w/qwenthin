//! Interactive TUI washed from Grok Build (`xai-org/grok-build`) pager UX.
//!
//! Reused interaction, not the Grok agent loop: foldable thinking
//! (`truncated_lines = 3`), streaming `push_chunk` / committed replace,
//! `❯` composer (Enter submits, trailing `\` continues), Esc cancels.
//! Assistant/think bubbles pretty-render markdown (fences + syntect) and
//! ` ```mermaid ` flowcharts/sequence as Unicode art. q38-loop still owns
//! tools, ThinkPolicy, JSONL, and the model.

mod app;
mod markdown;
mod mermaid;
mod overlay;
mod prompt;
mod transcript;
mod turn;

pub use app::{run, TuiOpts};
