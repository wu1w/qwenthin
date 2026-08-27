//! Result-driven test oracle: run a **scoped** suite after a code edit, not
//! because the user said "fix". Office docs / plan.md never trigger this.
//!
//! Commands stay cheap: `cargo test -p <pkg> --lib` or `pytest` on related
//! files. Full-workspace `cargo test` is never the default.

use std::path::{Path, PathBuf};

const CODE_EXT: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java", "kt", "kts", "c", "cc",
    "cpp", "cxx", "h", "hpp", "cs", "rb", "swift", "scala", "php", "zig",
];

pub fn is_code_path(path: &str) -> bool {
    let p = normalize(path);
    let Some((_, ext)) = p.rsplit_once('.') else {
        return false;
    };
    CODE_EXT.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// tests/ or test/ directory component, or a test-named file.
pub fn is_test_path(path: &str) -> bool {
    let p = normalize(path).to_lowercase();
    let mut parts = p.split('/').peekable();
    let mut file = "";
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            file = part;
        } else if part == "tests" || part == "test" || part == "__tests__" {
            return true;
        }
    }
    file.starts_with("test_")
        || file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_tests.rs")
}

fn normalize(path: &str) -> String {
    path.trim().trim_start_matches("./").replace('\\', "/")
}

/// Workspace has some way to run tests. Used for `--print` baseline only.
pub fn workspace_has_tests(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
        || root.join("pyproject.toml").is_file()
        || root.join("pytest.ini").is_file()
        || root.join("tests").is_dir()
        || has_root_python_tests(root)
}

/// Best-effort command for the files just edited. `None` = do not auto-run.
pub fn scoped_test_cmd(root: &Path, edited: &[String]) -> Option<String> {
    let code: Vec<String> = edited
        .iter()
        .map(|p| normalize(p))
        .filter(|p| is_code_path(p) && !is_test_path(p))
        .collect();
    if code.is_empty() {
        return None;
    }
    if let Some(cmd) = cargo_scoped(root, &code) {
        return Some(cmd);
    }
    if let Some(cmd) = pytest_scoped(root, &code) {
        return Some(cmd);
    }
    python_unittest(root)
}

/// Start-of-turn baseline when we do not yet know which files will change.
pub fn workspace_default_test_cmd(root: &Path) -> Option<String> {
    if root.join("Cargo.toml").is_file() {
        if package_name(&root.join("Cargo.toml")).is_some() && root.join("src/lib.rs").is_file() {
            let name = package_name(&root.join("Cargo.toml"))?;
            return Some(format!("cargo test -p {name} --lib"));
        }
        return None;
    }
    pytest_scoped(root, &[]).or_else(|| python_unittest(root))
}

fn cargo_scoped(root: &Path, edited: &[String]) -> Option<String> {
    if !root.join("Cargo.toml").is_file() {
        return None;
    }
    let path = edited.first()?;
    let (dir, name) = nearest_cargo_package(root, path)?;
    if dir.join("src/lib.rs").is_file() {
        Some(format!("cargo test -p {name} --lib"))
    } else {
        Some(format!("cargo test -p {name}"))
    }
}

fn nearest_cargo_package(root: &Path, rel: &str) -> Option<(PathBuf, String)> {
    let mut dir = root.join(rel);
    if dir.is_file() {
        dir.pop();
    }
    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file() {
            if let Some(name) = package_name(&cargo) {
                return Some((dir, name));
            }
        }
        if dir == root || !dir.pop() {
            break;
        }
    }
    None
}

fn package_name(cargo_toml: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(cargo_toml).ok()?;
    let v: toml::Value = raw.parse().ok()?;
    v.get("package")?
        .get("name")?
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn pytest_scoped(root: &Path, edited: &[String]) -> Option<String> {
    // Only claim pytest when the tree actually looks like a pytest project.
    // A lone `test_*.py` next to an edit is stdlib unittest — pytest may be
    // missing, and `-q` output would then never match the green→red detector.
    let pyish = root.join("pyproject.toml").is_file()
        || root.join("pytest.ini").is_file()
        || root.join("setup.cfg").is_file()
        || root.join("tests").is_dir();
    if !pyish {
        return None;
    }
    let py: Vec<&str> = edited
        .iter()
        .map(|s| s.as_str())
        .filter(|p| p.ends_with(".py"))
        .collect();
    if py.is_empty() && !root.join("tests").is_dir() && !root.join("pytest.ini").is_file() {
        return None;
    }
    let pybin = python_launcher();
    if py.is_empty() {
        return Some(format!("{pybin} -B -m pytest -q tests"));
    }
    let mut targets: Vec<String> = Vec::new();
    for p in &py {
        if let Some(t) = related_pytest(root, p) {
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
    }
    if targets.is_empty() {
        if let Some(parent) = Path::new(py[0]).parent() {
            let s = parent.to_string_lossy().replace('\\', "/");
            if !s.is_empty() && s != "." {
                targets.push(s);
            }
        }
    }
    if targets.is_empty() {
        return Some(format!("{pybin} -B -m pytest -q"));
    }
    Some(format!("{pybin} -B -m pytest -q {}", targets.join(" ")))
}

fn related_pytest(root: &Path, rel: &str) -> Option<String> {
    let p = Path::new(rel);
    let stem = p.file_stem()?.to_string_lossy();
    let parent = p.parent().unwrap_or(Path::new(""));
    let candidates = [
        parent.join(format!("test_{stem}.py")),
        parent.join("tests").join(format!("test_{stem}.py")),
        PathBuf::from("tests").join(format!("test_{stem}.py")),
        parent.join(format!("{stem}_test.py")),
    ];
    for c in candidates {
        if root.join(&c).is_file() {
            return Some(c.to_string_lossy().replace('\\', "/"));
        }
    }
    None
}

fn python_unittest(root: &Path) -> Option<String> {
    let python = python_launcher();
    if root.join("tests").is_dir() {
        return Some(format!("{python} -B -m unittest discover -s tests -v"));
    }
    has_root_python_tests(root)
        .then(|| format!("{python} -B -m unittest discover -s . -p \"test*.py\" -v"))
}

fn has_root_python_tests(root: &Path) -> bool {
    std::fs::read_dir(root).ok().is_some_and(|it| {
        it.filter_map(|e| e.ok()).any(|e| {
            let s = e.file_name().to_string_lossy().into_owned();
            s.starts_with("test_") && s.ends_with(".py")
        })
    })
}

pub fn python_launcher() -> &'static str {
    #[cfg(windows)]
    {
        if std::process::Command::new("py")
            .args(["-3", "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            "py -3"
        } else {
            "python"
        }
    }
    #[cfg(not(windows))]
    {
        "python3"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_path_skips_office_docs() {
        assert!(is_code_path("src/foo.rs"));
        assert!(is_code_path("pkg/a.py"));
        assert!(!is_code_path("notes.md"));
        assert!(!is_code_path("drafts/memo.txt"));
        assert!(!is_code_path("config.example.toml"));
    }

    #[test]
    fn cargo_scoped_uses_package_lib() {
        let dir =
            std::env::temp_dir().join(format!("q38-verify-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("crates/pack/src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/pack\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("crates/pack/Cargo.toml"),
            "[package]\nname = \"pack\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("crates/pack/src/lib.rs"), "pub fn f() {}\n").unwrap();
        let cmd = scoped_test_cmd(&dir, &["crates/pack/src/lib.rs".into()]).unwrap();
        assert_eq!(cmd, "cargo test -p pack --lib");
        assert!(scoped_test_cmd(&dir, &["README.md".into()]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pytest_picks_related_file() {
        let dir = std::env::temp_dir().join(format!("q38-py-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("mod.py"), "def f():\n    return 1\n").unwrap();
        std::fs::write(dir.join("tests/test_mod.py"), "def test_f():\n    pass\n").unwrap();
        let cmd = scoped_test_cmd(&dir, &["mod.py".into()]).unwrap();
        assert!(cmd.contains("pytest"), "{cmd}");
        assert!(cmd.contains("tests/test_mod.py"), "{cmd}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unittest_tree_does_not_claim_pytest() {
        let dir = std::env::temp_dir().join(format!("q38-ut-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
        std::fs::write(dir.join("test_app.py"), "import unittest\n").unwrap();
        let cmd = scoped_test_cmd(&dir, &["app.py".into()]).unwrap();
        assert!(cmd.contains("unittest"), "{cmd}");
        assert!(!cmd.contains("pytest"), "{cmd}");
        assert!(workspace_has_tests(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }
}
