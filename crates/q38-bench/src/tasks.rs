//! Three deterministic coding tasks and their success predicates.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use q38_loop::tokenize::count_tokens;
use q38_loop::Family;

pub const EXPECTED_BANNER: &str = "q38-ok";
pub const FAT_PREFIX_TARGET_TOKENS: u32 = 12_000;

pub const SCALE_ASSERTS: &[&str] = &[
    "assert_eq!(scale(3), 6)",
    "assert_eq!(scale(0), 0)",
    "assert_eq!(scale(-2), -4)",
];

#[derive(Clone, Copy, Debug)]
pub struct Task {
    pub name: &'static str,
    pub dir_name: &'static str,
    pub prompt: &'static str,
}

pub const TASKS: &[Task] = &[
    Task {
        name: "01-read-edit",
        dir_name: "01-read-edit",
        prompt: "Read note.txt first. Then edit src/main.rs so it prints new instead of old. \
The file must contain println!(\"new\") and must not contain old. Change only what is required.",
    },
    Task {
        name: "02-multi-file",
        dir_name: "02-multi-file",
        prompt: "Read hint.txt. Change the BANNER constant in src/lib.rs so the program would print q38-ok. \
Do not change src/main.rs.",
    },
    Task {
        name: "03-test-fix",
        dir_name: "03-test-fix",
        prompt: "src/lib.rs has a failing unit test. Fix the scale implementation so the existing asserts pass. \
Do not change the test.",
    },
];

#[derive(Clone, Debug)]
pub struct Eval {
    pub ok: bool,
    pub notes: String,
}

impl Eval {
    fn pass(notes: impl Into<String>) -> Self {
        Self {
            ok: true,
            notes: notes.into(),
        }
    }

    fn fail(notes: impl Into<String>) -> Self {
        Self {
            ok: false,
            notes: notes.into(),
        }
    }
}

pub fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

pub fn fixture_dir(task: &Task) -> PathBuf {
    fixtures_root().join(task.dir_name)
}

pub fn copy_task(task: &Task, dest: &Path) -> Result<()> {
    let src = fixture_dir(task);
    if !src.is_dir() {
        bail!("{}: missing fixture {}", task.name, src.display());
    }
    if dest.exists() {
        fs::remove_dir_all(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    copy_dir(&src, dest).with_context(|| format!("copy {} -> {}", src.display(), dest.display()))
}

pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

pub fn evaluate(task: &Task, root: &Path) -> Eval {
    match task.name {
        "01-read-edit" => check_01(root),
        "02-multi-file" => check_02(root),
        "03-test-fix" => check_03(root),
        other => Eval::fail(format!("unknown task {other}")),
    }
}

/// Same as [`evaluate`], then optionally `cargo test` for 03-test-fix.
pub fn evaluate_live(task: &Task, root: &Path) -> Eval {
    let mut ev = evaluate(task, root);
    if task.name != "03-test-fix" || !ev.ok {
        return ev;
    }
    match cargo_test(root) {
        Ok(true) => {
            ev.notes = format!("{}; cargo test passed", ev.notes);
        }
        Ok(false) => {
            ev.ok = false;
            ev.notes = format!("{}; cargo test failed", ev.notes);
        }
        Err(e) => {
            ev.notes = format!("{}; cargo test skipped: {e}", ev.notes);
        }
    }
    ev
}

pub fn apply_golden(task: &Task, root: &Path) -> Result<()> {
    match task.name {
        "01-read-edit" => replace_in(
            root.join("src/main.rs"),
            "println!(\"old\")",
            "println!(\"new\")",
        ),
        "02-multi-file" => replace_in(
            root.join("src/lib.rs"),
            "\"alpha\"",
            &format!("\"{EXPECTED_BANNER}\""),
        ),
        "03-test-fix" => replace_in(root.join("src/lib.rs"), "n * n", "n * 2"),
        other => bail!("unknown task {other}"),
    }
}

fn replace_in(path: PathBuf, from: &str, to: &str) -> Result<()> {
    let src = fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    fs::write(&path, src.replace(from, to)).with_context(|| path.display().to_string())
}

fn check_01(root: &Path) -> Eval {
    let path = root.join("src/main.rs");
    let Ok(src) = fs::read_to_string(&path) else {
        return Eval::fail("missing src/main.rs");
    };
    if !src.contains("println!(\"new\")") {
        return Eval::fail("src/main.rs does not contain println!(\"new\")");
    }
    if src.contains("old") {
        return Eval::fail("src/main.rs still contains old");
    }
    Eval::pass("println!(\"new\") and no old")
}

fn check_02(root: &Path) -> Eval {
    let path = root.join("src/lib.rs");
    let Ok(src) = fs::read_to_string(&path) else {
        return Eval::fail("missing src/lib.rs");
    };
    if !src.contains(EXPECTED_BANNER) {
        return Eval::fail(format!("src/lib.rs does not contain {EXPECTED_BANNER}"));
    }
    if src.contains("\"alpha\"") {
        return Eval::fail("src/lib.rs still has BANNER alpha");
    }
    Eval::pass(format!("BANNER is {EXPECTED_BANNER}"))
}

fn check_03(root: &Path) -> Eval {
    let path = root.join("src/lib.rs");
    let Ok(src) = fs::read_to_string(&path) else {
        return Eval::fail("missing src/lib.rs");
    };
    if !src.contains("#[cfg(test)]") || !src.contains("mod tests") {
        return Eval::fail("unit tests were removed");
    }
    for a in SCALE_ASSERTS {
        if !src.contains(a) {
            return Eval::fail(format!("assert changed or missing: {a}"));
        }
    }
    let Some(func) = extract_fn(&src, "scale") else {
        return Eval::fail("could not find pub fn scale");
    };
    if func.contains("n * n") || func.contains("n.pow(") {
        return Eval::fail("scale still squares n (fix the implementation, not the test)");
    }
    Eval::pass("scale implementation changed; asserts intact")
}

fn extract_fn(src: &str, name: &str) -> Option<String> {
    let needle = format!("fn {name}");
    let start = src.find(&needle)?;
    let rest = &src[start..];
    let brace = rest.find('{')?;
    let mut depth = 0i32;
    let mut end = brace;
    for (i, c) in rest[brace..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = brace + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    Some(rest[..end].to_string())
}

pub fn cargo_test(root: &Path) -> Result<bool> {
    let out = std::process::Command::new("cargo")
        .current_dir(root)
        .args(["test", "--offline", "--quiet"])
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .context("spawn cargo test")?;
    Ok(out.status.success())
}

/// Repeated filler (~`target` tokens) using the vendored Qwen3.8 tokenizer.
/// Not a Hermes prompt.
pub fn dummy_fat_prefix(target: u32) -> Result<(String, u32)> {
    const HEADER: &str =
        "Ignore this filler. It is not a task instruction and must not change how you edit files.\n";
    const UNIT: &str = " lorem";
    let header_n = count_tokens(Family::Qwen38, HEADER).map_err(|e| anyhow::anyhow!("{e}"))?;
    let unit_n = count_tokens(Family::Qwen38, UNIT)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .max(1);
    let need = target.saturating_sub(header_n);
    let mut reps = (need / unit_n).saturating_add(1) as usize;
    let mut text = String::from(HEADER);
    text.push_str(&UNIT.repeat(reps));
    let mut n = count_tokens(Family::Qwen38, &text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut guard = 0u32;
    while n > target.saturating_add(64) && reps > 1 && guard < 8 {
        let extra = ((n - target) / unit_n).max(1) as usize;
        reps = reps.saturating_sub(extra).max(1);
        text = String::from(HEADER);
        text.push_str(&UNIT.repeat(reps));
        n = count_tokens(Family::Qwen38, &text).map_err(|e| anyhow::anyhow!("{e}"))?;
        guard += 1;
    }
    Ok((text, n))
}

/// Copy each fixture, confirm unsolved fails and a golden patch passes.
pub fn validate_fixtures() -> Result<Vec<String>> {
    let tmp = std::env::temp_dir().join(format!("q38-bench-validate-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let mut lines = Vec::new();
    let result = (|| {
        for task in TASKS {
            let unsolved = tmp.join(format!("{}-unsolved", task.name));
            copy_task(task, &unsolved)?;
            let u = evaluate(task, &unsolved);
            if u.ok {
                bail!(
                    "{} unsolved fixture already passes the success predicate",
                    task.name
                );
            }
            let patched = tmp.join(format!("{}-patched", task.name));
            copy_task(task, &patched)?;
            apply_golden(task, &patched)?;
            let p = evaluate(task, &patched);
            if !p.ok {
                bail!("{} golden patch failed: {}", task.name, p.notes);
            }
            lines.push(format!(
                "{}  unsolved=fail  patched=ok  ({})",
                task.name, p.notes
            ));
        }
        Ok(lines)
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_tasks() {
        assert_eq!(TASKS.len(), 3);
        assert_eq!(TASKS[0].name, "01-read-edit");
        assert_eq!(TASKS[1].name, "02-multi-file");
        assert_eq!(TASKS[2].name, "03-test-fix");
    }

    #[test]
    fn fixtures_exist() {
        for task in TASKS {
            assert!(fixture_dir(task).is_dir(), "{}", task.dir_name);
        }
        assert!(fixture_dir(&TASKS[0]).join("note.txt").is_file());
        assert!(fixture_dir(&TASKS[0]).join("src/main.rs").is_file());
        assert!(fixture_dir(&TASKS[1]).join("src/lib.rs").is_file());
        assert!(fixture_dir(&TASKS[1]).join("src/main.rs").is_file());
        assert!(fixture_dir(&TASKS[2]).join("src/lib.rs").is_file());
    }

    #[test]
    fn unsolved_fail_golden_pass() {
        validate_fixtures().unwrap();
    }

    #[test]
    fn copy_to_temp_stays_independent() {
        let tmp = std::env::temp_dir().join(format!("q38-bench-copy-test-{}", std::process::id()));
        copy_task(&TASKS[0], &tmp).unwrap();
        let main = tmp.join("src/main.rs");
        fs::write(&main, "fn main() { println!(\"new\"); }\n").unwrap();
        assert!(evaluate(&TASKS[0], &tmp).ok);
        let repo = fs::read_to_string(fixture_dir(&TASKS[0]).join("src/main.rs")).unwrap();
        assert!(repo.contains("old"), "repo fixture must stay unsolved");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fat_filler_is_about_12k_qwen38_tokens() {
        let (text, n) = dummy_fat_prefix(FAT_PREFIX_TARGET_TOKENS).unwrap();
        assert!(!text.contains("Hermes"), "{text}");
        assert!(
            (11_500..=13_500).contains(&n),
            "fat prefix tokens={n}, want ~12000"
        );
    }

    #[test]
    fn extract_scale_fn() {
        let src = fs::read_to_string(fixture_dir(&TASKS[2]).join("src/lib.rs")).unwrap();
        let f = extract_fn(&src, "scale").unwrap();
        assert!(f.contains("n * n"));
        assert!(!f.contains("mod tests"));
    }
}
