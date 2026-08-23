//! Progressive disclosure for skills. Catalog (name + one trigger line) may go
//! in the session system prompt when `skills_auto_catalog` is on. SKILL.md
//! bodies never do, and `skill` is not in `tools[]`.
//!
//! Overlay matches MCP: `~/.q38-agent/skills` then workspace `.q38/skills`
//! (later wins). Harness injects at most one body as a hidden user after the
//! live query. A model-emitted `skill` call still hits [`run_skill`] so XML
//! does not fall through to unknown-tool.

use std::path::{Path, PathBuf};

use crate::tool_calls::{ToolCall, ToolResponse, ToolState};
use crate::tools::{arg_str, folded_response, BlobStore, ToolLimits};

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    pub skills: Vec<Skill>,
}

impl SkillCatalog {
    pub fn load(home: &Path, workspace: &Path) -> Self {
        let mut skills = Vec::new();
        scan_dir(home.join("skills"), &mut skills);
        scan_dir(workspace.join(".q38").join("skills"), &mut skills);
        let mut out: Vec<Skill> = Vec::new();
        for sk in skills {
            if let Some(i) = out
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(&sk.name))
            {
                out[i] = sk;
            } else {
                out.push(sk);
            }
        }
        out.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
        Self { skills: out }
    }

    /// One name + trigger line each. Empty when there are no skills.
    pub fn catalog_markdown(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut s = String::from("skills:\n");
        for sk in &self.skills {
            let trig = if sk.description.is_empty() {
                "on demand"
            } else {
                sk.description.trim()
            };
            let trig: String = trig.chars().take(40).collect();
            s.push_str(&format!("- {}: {}\n", sk.name, trig));
        }
        s
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        let n = name.trim();
        self.skills.iter().find(|s| s.name.eq_ignore_ascii_case(n))
    }
}

/// Direct SKILL.md load. Not in `tools[]`; bodies normally go through
/// [`hidden_card`] (fail-closed at 400 tok). `dispatch_one` still calls this
/// when the model emits `skill` so the tool_call id gets a result.
pub fn run_skill(
    catalog: &SkillCatalog,
    call: &ToolCall,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(name) = arg_str(&call.arguments, "name") else {
        return ToolResponse::text(&call.id, "Error: skill needs `name`.", ToolState::Error);
    };
    let Some(sk) = catalog.get(&name) else {
        let known: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        return ToolResponse::text(
            &call.id,
            format!(
                "Error: unknown skill '{name}'. Known: {}",
                if known.is_empty() {
                    "(none)".into()
                } else {
                    known.join(", ")
                }
            ),
            ToolState::Error,
        );
    };
    match std::fs::read_to_string(&sk.path) {
        Ok(body) => folded_response(&call.id, body, ToolState::Success, limits, blobs),
        Err(e) => ToolResponse::text(&call.id, format!("Error: {e}"), ToolState::Error),
    }
}

pub fn hidden_card(skill: &Skill) -> Option<String> {
    let raw = std::fs::read_to_string(&skill.path).ok()?;
    let body = strip_frontmatter(&raw);
    if body.is_empty() {
        return None;
    }
    if crate::sticky::tokens(&body) > crate::sticky::SKILL_BODY_MAX_TOKENS {
        return None;
    }
    Some(format!("[skill: {}]\n{}", skill.name, body))
}

/// At most one skill. Explicit `[skill:name]` / exact name beats FAILED / commit.
pub fn match_user<'a>(catalog: &'a SkillCatalog, user: &str) -> Option<&'a Skill> {
    let (forced, rest) = crate::sticky::split_skill_prefix(user);
    if let Some(name) = forced {
        return catalog.get(&name);
    }
    if let Some(sk) = named_in_text(catalog, rest.as_str()) {
        return Some(sk);
    }
    if commitish(&rest) {
        return catalog
            .get("commit")
            .or_else(|| named_in_text(catalog, "commit"));
    }
    None
}

pub fn match_tool_output<'a>(catalog: &'a SkillCatalog, output: &str) -> Option<&'a Skill> {
    if !tests_failed(output) {
        return None;
    }
    catalog
        .get("testhook")
        .or_else(|| catalog.get("test"))
        .or_else(|| catalog.get("tests"))
}

fn named_in_text<'a>(catalog: &'a SkillCatalog, text: &str) -> Option<&'a Skill> {
    for sk in &catalog.skills {
        if has_name_token(text, &sk.name) {
            return Some(sk);
        }
    }
    None
}

fn has_name_token(hay: &str, name: &str) -> bool {
    if name.chars().any(|c| !c.is_ascii()) {
        return hay.contains(name);
    }
    hay.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .any(|w| w.eq_ignore_ascii_case(name))
}

fn commitish(user: &str) -> bool {
    user.contains("提交")
        || user
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w.eq_ignore_ascii_case("commit") || w.eq_ignore_ascii_case("commits"))
}

pub fn tests_failed(text: &str) -> bool {
    text.contains("FAILED")
        || text.contains("test failed")
        || text.contains("failures:")
        || text.contains("error: test failed")
}

fn strip_frontmatter(raw: &str) -> String {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("---") else {
        return t.to_string();
    };
    let rest = rest.trim_start_matches('\n');
    match rest.split_once("\n---") {
        Some((_, body)) => body.trim().to_string(),
        None => t.to_string(),
    }
}

fn scan_dir(dir: PathBuf, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let skill_md = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            path.clone()
        } else {
            continue;
        };
        if !skill_md.is_file() {
            continue;
        }
        if let Some(sk) = parse_skill(&skill_md) {
            out.push(sk);
        }
    }
}

fn parse_skill(path: &Path) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("skill")
        .to_string();
    let (name, description) = frontmatter(&raw);
    Some(Skill {
        name: name.unwrap_or(dir_name),
        description: description.unwrap_or_default(),
        path: path.to_path_buf(),
    })
}

fn frontmatter(raw: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (None, None);
    };
    let Some((fm, _)) = rest.split_once("\n---") else {
        return (None, None);
    };
    let mut name = None;
    let mut description = None;
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().trim_matches('"').to_string());
        }
    }
    (name, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_from_frontmatter() {
        let dir = std::env::temp_dir().join(format!("q38-sk-{}", uuid::Uuid::new_v4().simple()));
        let skill_dir = dir.join("skills").join("pdf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: Extract text from PDFs\n---\nUse pdftotext.\n",
        )
        .unwrap();
        let hook = dir.join("skills").join("testhook");
        std::fs::create_dir_all(&hook).unwrap();
        std::fs::write(
            hook.join("SKILL.md"),
            "---\nname: testhook\n---\nRerun the failing file.\n",
        )
        .unwrap();
        let cat = SkillCatalog::load(&dir, &dir);
        assert_eq!(cat.skills.len(), 2);
        assert_eq!(cat.skills[0].name, "pdf");
        assert!(cat.catalog_markdown().contains("pdf"));
        assert!(cat.catalog_markdown().starts_with("skills:"));
        let card = hidden_card(&cat.skills[0]).unwrap();
        assert!(card.starts_with("[skill: pdf]"));
        assert!(card.contains("Use pdftotext"));
        assert!(match_user(&cat, "Call pdf on this file").is_some());
        assert!(match_user(&cat, "这个函数 off-by-one 吗").is_none());
        assert!(match_tool_output(&cat, "test FAILED in foo.rs").is_some());
        let call = crate::tool_calls::ToolCall {
            id: "1".into(),
            name: "skill".into(),
            arguments: serde_json::json!({"name": "pdf"}),
        };
        let loaded = run_skill(&cat, &call, crate::tools::ToolLimits::default(), None);
        assert!(loaded.joined_text().contains("Use pdftotext"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workspace_skill_overlays_home() {
        let root =
            std::env::temp_dir().join(format!("q38-sk-ov-{}", uuid::Uuid::new_v4().simple()));
        let home = root.join("home");
        let ws = root.join("ws");
        let home_pdf = home.join("skills").join("pdf");
        let ws_pdf = ws.join(".q38").join("skills").join("pdf");
        let home_only = home.join("skills").join("commit");
        std::fs::create_dir_all(&home_pdf).unwrap();
        std::fs::create_dir_all(&ws_pdf).unwrap();
        std::fs::create_dir_all(&home_only).unwrap();
        std::fs::write(
            home_pdf.join("SKILL.md"),
            "---\nname: pdf\ndescription: home pdf\n---\nhome body\n",
        )
        .unwrap();
        std::fs::write(
            ws_pdf.join("SKILL.md"),
            "---\nname: pdf\ndescription: workspace pdf\n---\nworkspace body\n",
        )
        .unwrap();
        std::fs::write(
            home_only.join("SKILL.md"),
            "---\nname: commit\n---\ncommit body\n",
        )
        .unwrap();
        let cat = SkillCatalog::load(&home, &ws);
        assert_eq!(cat.skills.len(), 2);
        let pdf = cat.get("pdf").unwrap();
        assert_eq!(pdf.description, "workspace pdf");
        assert!(
            hidden_card(pdf).unwrap().contains("workspace body"),
            "{:?}",
            hidden_card(pdf)
        );
        assert!(cat.get("commit").is_some());
        let _ = std::fs::remove_dir_all(root);
    }
}
