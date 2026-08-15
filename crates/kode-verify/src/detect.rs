use std::path::Path;

use crate::{ProjectKind, ProjectProfile, VerifyStep};

/// Detects the project kind at `root` and builds its verification steps.
///
/// Precedence when multiple markers are present: Cargo.toml > go.mod >
/// package.json > pyproject.toml (or pytest.ini). No marker found yields an
/// `Unknown` profile with no steps.
pub fn detect(root: &Path) -> ProjectProfile {
    if root.join("Cargo.toml").is_file() {
        return rust_profile();
    }
    if root.join("go.mod").is_file() {
        return go_profile();
    }
    if root.join("package.json").is_file() {
        return node_profile(root);
    }
    if root.join("pyproject.toml").is_file() || root.join("pytest.ini").is_file() {
        return python_profile();
    }
    ProjectProfile {
        kind: ProjectKind::Unknown,
        steps: Vec::new(),
    }
}

fn rust_profile() -> ProjectProfile {
    ProjectProfile {
        kind: ProjectKind::Rust,
        steps: vec![
            step("fmt", "cargo", &["fmt", "--check"], true),
            step("check", "cargo", &["check", "--workspace"], true),
            step("test", "cargo", &["test", "--workspace"], true),
            step(
                "clippy",
                "cargo",
                &["clippy", "--workspace", "--all-targets"],
                false,
            ),
        ],
    }
}

fn go_profile() -> ProjectProfile {
    ProjectProfile {
        kind: ProjectKind::Go,
        steps: vec![
            step("build", "go", &["build", "./..."], true),
            step("test", "go", &["test", "./..."], true),
            step("vet", "go", &["vet", "./..."], false),
        ],
    }
}

fn python_profile() -> ProjectProfile {
    ProjectProfile {
        kind: ProjectKind::Python,
        steps: vec![step("pytest", "python", &["-m", "pytest", "-q"], true)],
    }
}

fn node_profile(root: &Path) -> ProjectProfile {
    let mut steps = Vec::new();
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };

    // Unreadable or invalid package.json is treated as having no scripts.
    let scripts = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("scripts").cloned())
        .and_then(|v| v.as_object().cloned());

    if let Some(scripts) = scripts {
        if scripts.contains_key("build") {
            steps.push(step("build", npm, &["run", "build"], false));
        }
        if let Some(test_script) = scripts.get("test").and_then(|v| v.as_str())
            && !test_script.contains("no test specified")
        {
            steps.push(step("test", npm, &["test"], true));
        }
        if scripts.contains_key("lint") {
            steps.push(step("lint", npm, &["run", "lint"], false));
        }
    }

    ProjectProfile {
        kind: ProjectKind::Node,
        steps,
    }
}

fn step(name: &str, program: &str, args: &[&str], required: bool) -> VerifyStep {
    VerifyStep {
        name: name.to_string(),
        program: program.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        required,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("kode-verify-detect-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_rust_project() {
        let dir = temp_dir();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();

        let profile = detect(&dir);
        assert_eq!(profile.kind, ProjectKind::Rust);
        assert_eq!(profile.steps.len(), 4);
        let clippy = profile.steps.iter().find(|s| s.name == "clippy").unwrap();
        assert!(!clippy.required);
    }

    #[test]
    fn detects_go_project() {
        let dir = temp_dir();
        std::fs::write(dir.join("go.mod"), "module x\n").unwrap();

        let profile = detect(&dir);
        assert_eq!(profile.kind, ProjectKind::Go);
    }

    #[test]
    fn detects_node_project_with_test_and_lint() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"test":"vitest run","lint":"eslint ."}}"#,
        )
        .unwrap();

        let profile = detect(&dir);
        assert_eq!(profile.kind, ProjectKind::Node);
        assert!(profile.steps.iter().any(|s| s.name == "test" && s.required));
        assert!(
            profile
                .steps
                .iter()
                .any(|s| s.name == "lint" && !s.required)
        );
        assert!(!profile.steps.iter().any(|s| s.name == "build"));
    }

    #[test]
    fn node_placeholder_test_script_is_skipped() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();

        let profile = detect(&dir);
        assert_eq!(profile.kind, ProjectKind::Node);
        assert!(!profile.steps.iter().any(|s| s.name == "test"));
    }

    #[test]
    fn detects_python_project() {
        let dir = temp_dir();
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname=\"x\"\n").unwrap();

        let profile = detect(&dir);
        assert_eq!(profile.kind, ProjectKind::Python);
    }

    #[test]
    fn empty_dir_is_unknown() {
        let dir = temp_dir();

        let profile = detect(&dir);
        assert_eq!(profile.kind, ProjectKind::Unknown);
        assert_eq!(profile.steps.len(), 0);
    }
}
