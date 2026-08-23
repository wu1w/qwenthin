//! Role + boundary only. Official Jinja owns thinking and the tool-call format.
//!
//! `AGENT.md` is the frozen identity slot (Hermes SOUL.md, shorter). One line of
//! voice markers beats a 4–8 line essay on this 27B. Workspace file wins, then
//! `~/.q38-agent/AGENT.md`. Coding identity is opt-in (`prompt.coding`) and only
//! used when no file exists. Personality does not belong in MEMORY.md.

use std::fs;
use std::path::Path;

pub const AGENT_MD_NAME: &str = "AGENT.md";

/// 工作区助手 + 路径相对 + 只落盘最终交付.
pub const DEFAULT_AGENT_MD: &str = "工作区助手。路径相对。只落盘交付。\n";

/// 编程助手 + 路径相对. Used when `prompt.coding` and no AGENT.md.
pub const CODING_AGENT_MD: &str = "编程助手。路径相对。只落盘交付。\n";

/// Back-compat alias for tests that still name the frozen system blob.
pub const CODING_SYSTEM_PROMPT: &str = DEFAULT_AGENT_MD;

pub fn builtin_role_boundary(coding: bool) -> &'static str {
    if coding {
        CODING_AGENT_MD
    } else {
        DEFAULT_AGENT_MD
    }
}

pub fn load_role_boundary(
    workspace: &Path,
    home: Option<&Path>,
    file: &str,
    coding: bool,
) -> String {
    let name = if file.trim().is_empty() {
        AGENT_MD_NAME
    } else {
        file.trim()
    };
    for root in [Some(workspace), home] {
        let Some(root) = root else { continue };
        let path = root.join(name);
        if let Ok(raw) = fs::read_to_string(&path) {
            let t = raw.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    builtin_role_boundary(coding).trim().to_string()
}

pub fn session_prompt(workspace: &Path, home: Option<&Path>, file: &str, coding: bool) -> String {
    let role = load_role_boundary(workspace, home, file, coding);
    with_workspace(&role, &workspace.display().to_string())
}

pub fn with_workspace(role_boundary: &str, workspace: &str) -> String {
    format!("{}\nWorkspace:\n    {workspace}\n", role_boundary.trim())
}

/// Tests / callers that only have a display path (no AGENT.md search).
pub fn coding_prompt(workspace: &str) -> String {
    with_workspace(DEFAULT_AGENT_MD, workspace)
}

/// Skill / MCP name lists only. Empty catalogs emit nothing.
pub fn periphery_section(skills_catalog: &str, mcp_catalog: &str) -> String {
    let mut s = String::new();
    if !skills_catalog.is_empty() {
        s.push('\n');
        s.push_str(skills_catalog.trim_end());
        s.push('\n');
    }
    if !mcp_catalog.is_empty() {
        s.push('\n');
        s.push_str(mcp_catalog.trim_end());
        s.push('\n');
    }
    s
}

#[cfg(test)]
fn cjk_len(s: &str) -> usize {
    s.chars()
        .filter(|c| !c.is_whitespace() && !"。，、．.!?,;:".contains(*c))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_is_about_ten_chars() {
        assert!(cjk_len(DEFAULT_AGENT_MD) <= 16);
        assert!(cjk_len(DEFAULT_AGENT_MD) >= 8);
        assert!(cjk_len(CODING_AGENT_MD) <= 16);
        assert!(DEFAULT_AGENT_MD.contains("工作区助手"));
        assert!(DEFAULT_AGENT_MD.contains("只落盘交付"));
        assert!(!DEFAULT_AGENT_MD.contains("本地27B"));
        assert!(!DEFAULT_AGENT_MD.contains("TODO.md"));
        assert!(!DEFAULT_AGENT_MD.contains("Think first"));
        assert!(!DEFAULT_AGENT_MD.contains("coding agent"));
        assert!(!DEFAULT_AGENT_MD.contains("do not"));
        assert!(!DEFAULT_AGENT_MD.contains("云端"));
    }

    #[test]
    fn coding_flag_only_changes_builtin() {
        assert!(builtin_role_boundary(false).contains("工作区助手"));
        assert!(builtin_role_boundary(true).contains("编程助手"));
    }

    #[test]
    fn diy_file_wins_over_coding_flag() {
        let dir =
            std::env::temp_dir().join(format!("q38-agent-md-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENT.md"), "家里猫。别出家门。\n").unwrap();
        let s = load_role_boundary(&dir, None, "AGENT.md", true);
        assert_eq!(s, "家里猫。别出家门。");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn periphery_empty_when_nothing_to_say() {
        assert!(periphery_section("", "").is_empty());
        let s = periphery_section("Skills: pdf\n", "");
        assert!(s.contains("pdf"));
        assert!(!s.contains("MEMORY.md"));
        assert!(!s.contains("do not guess"));
    }

    #[test]
    fn prompt_includes_workspace_boundary() {
        let s = coding_prompt("/tmp/ws");
        assert!(s.contains("工作区助手"));
        assert!(s.contains("/tmp/ws"));
    }
}
