use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kode_core::config::VerifyConfig;

use crate::{ProjectKind, ProjectProfile, VerifyStep};

pub fn detect(root: &Path) -> ProjectProfile {
    detect_with_config(root, &VerifyConfig::default())
}

/// Builds an explicit profile when configured, otherwise discovers project
/// roots (including polyglot subprojects) to a bounded depth.
pub fn detect_with_config(root: &Path, config: &VerifyConfig) -> ProjectProfile {
    if !config.steps.is_empty() {
        let steps = config
            .steps
            .iter()
            .map(|configured| VerifyStep {
                name: configured.name.clone(),
                program: configured.command.clone(),
                args: configured.args.clone(),
                cwd: configured.cwd.clone().unwrap_or_default(),
                required: configured.required,
                timeout: Duration::from_secs(
                    configured.timeout_seconds.unwrap_or(config.timeout_seconds),
                ),
            })
            .collect();
        return ProjectProfile {
            kind: ProjectKind::Unknown,
            steps,
            fail_fast: config.fail_fast,
        };
    }

    let roots = discover_roots(root);
    let kinds: BTreeSet<_> = roots.iter().map(|(_, kind)| kind_rank(*kind)).collect();
    let kind = if kinds.len() > 1 {
        ProjectKind::Mixed
    } else {
        roots
            .first()
            .map(|(_, kind)| *kind)
            .unwrap_or(ProjectKind::Unknown)
    };
    let timeout = Duration::from_secs(config.timeout_seconds);
    let mut steps = Vec::new();
    for (relative, project_kind) in roots {
        let prefix = if relative.as_os_str().is_empty() {
            String::new()
        } else {
            format!("{}:", relative.display())
        };
        let mut project_steps = match project_kind {
            ProjectKind::Rust => rust_steps(timeout),
            ProjectKind::Go => go_steps(timeout),
            ProjectKind::Node => node_steps(&root.join(&relative), timeout),
            ProjectKind::Python => python_steps(&root.join(&relative), timeout),
            ProjectKind::Mixed | ProjectKind::Unknown => Vec::new(),
        };
        for step in &mut project_steps {
            step.name = format!("{prefix}{}", step.name);
            step.cwd = relative.clone();
        }
        steps.extend(project_steps);
    }
    ProjectProfile {
        kind,
        steps,
        fail_fast: config.fail_fast,
    }
}

fn kind_rank(kind: ProjectKind) -> u8 {
    match kind {
        ProjectKind::Rust => 0,
        ProjectKind::Go => 1,
        ProjectKind::Node => 2,
        ProjectKind::Python => 3,
        _ => 4,
    }
}

fn discover_roots(root: &Path) -> Vec<(PathBuf, ProjectKind)> {
    fn visit(root: &Path, relative: &Path, depth: usize, found: &mut Vec<(PathBuf, ProjectKind)>) {
        let dir = root.join(relative);
        let markers = [
            ("Cargo.toml", ProjectKind::Rust),
            ("go.mod", ProjectKind::Go),
            ("package.json", ProjectKind::Node),
            ("pyproject.toml", ProjectKind::Python),
            ("pytest.ini", ProjectKind::Python),
        ];
        for (marker, kind) in markers {
            if dir.join(marker).is_file()
                && !found.iter().any(|(ancestor, ancestor_kind)| {
                    *ancestor_kind == kind && relative.starts_with(ancestor)
                })
            {
                found.push((relative.to_path_buf(), kind));
            }
        }
        if depth == 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut children: Vec<_> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !name.starts_with('.')
                    && !matches!(
                        name.as_ref(),
                        "target" | "node_modules" | "vendor" | "dist" | "build" | "__pycache__"
                    )
            })
            .collect();
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            visit(root, &relative.join(child.file_name()), depth + 1, found);
        }
    }

    let mut found = Vec::new();
    visit(root, Path::new(""), 0, &mut found);
    found.sort_by(|(a, ak), (b, bk)| a.cmp(b).then(kind_rank(*ak).cmp(&kind_rank(*bk))));
    found
}

fn rust_steps(timeout: Duration) -> Vec<VerifyStep> {
    vec![
        step("fmt", "cargo", &["fmt", "--check"], true, timeout),
        step("check", "cargo", &["check", "--workspace"], true, timeout),
        step("test", "cargo", &["test", "--workspace"], true, timeout),
        step(
            "clippy",
            "cargo",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
            false,
            timeout,
        ),
    ]
}
fn go_steps(timeout: Duration) -> Vec<VerifyStep> {
    vec![
        step("build", "go", &["build", "./..."], true, timeout),
        step("test", "go", &["test", "./..."], true, timeout),
        step("vet", "go", &["vet", "./..."], false, timeout),
    ]
}

fn node_steps(root: &Path, timeout: Duration) -> Vec<VerifyStep> {
    let (manager, run_prefix): (&str, &[&str]) = if root.join("pnpm-lock.yaml").is_file() {
        (if cfg!(windows) { "pnpm.cmd" } else { "pnpm" }, &["run"])
    } else if root.join("yarn.lock").is_file() {
        (if cfg!(windows) { "yarn.cmd" } else { "yarn" }, &[])
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        (if cfg!(windows) { "bun.exe" } else { "bun" }, &["run"])
    } else {
        (if cfg!(windows) { "npm.cmd" } else { "npm" }, &["run"])
    };
    let scripts = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("scripts").cloned())
        .and_then(|v| v.as_object().cloned());
    let mut steps = Vec::new();
    if let Some(scripts) = scripts {
        for (name, required) in [("build", false), ("test", true), ("lint", false)] {
            let Some(script) = scripts.get(name).and_then(|v| v.as_str()) else {
                continue;
            };
            if name == "test" && script.contains("no test specified") {
                continue;
            }
            let mut args = run_prefix.to_vec();
            args.push(name);
            steps.push(step(name, manager, &args, required, timeout));
        }
    }
    steps
}

fn python_steps(root: &Path, timeout: Duration) -> Vec<VerifyStep> {
    let python = if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    };
    let mut steps = vec![step(
        "pytest",
        python,
        &["-m", "pytest", "-q"],
        true,
        timeout,
    )];
    let config = std::fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
    if config.contains("[tool.ruff") {
        steps.push(step(
            "ruff",
            python,
            &["-m", "ruff", "check", "."],
            false,
            timeout,
        ));
    }
    if config.contains("[tool.mypy") {
        steps.push(step("mypy", python, &["-m", "mypy", "."], false, timeout));
    }
    steps
}

fn step(name: &str, program: &str, args: &[&str], required: bool, timeout: Duration) -> VerifyStep {
    VerifyStep {
        name: name.into(),
        program: program.into(),
        args: args.iter().map(|s| (*s).into()).collect(),
        cwd: PathBuf::new(),
        required,
        timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-detect-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_polyglot_monorepo() {
        let dir = temp_dir();
        std::fs::write(dir.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::create_dir(dir.join("web")).unwrap();
        std::fs::write(
            dir.join("web/package.json"),
            r#"{"scripts":{"test":"vitest"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("web/pnpm-lock.yaml"), "").unwrap();
        let profile = detect(&dir);
        assert_eq!(profile.kind, ProjectKind::Mixed);
        assert!(profile.steps.iter().any(|s| s.name == "web:test"
            && s.program.contains("pnpm")
            && s.cwd == Path::new("web")));
        assert!(
            profile
                .steps
                .iter()
                .any(|s| s.name == "clippy" && s.args.ends_with(&["-D".into(), "warnings".into()]))
        );
    }

    #[test]
    fn explicit_steps_override_detection() {
        let dir = temp_dir();
        std::fs::write(dir.join("Cargo.toml"), "[workspace]").unwrap();
        let config: VerifyConfig = toml::from_str(
            r#"timeout_seconds=20
[[steps]]
name="custom"
command="tool"
args=["check"]
cwd="app"
"#,
        )
        .unwrap();
        let profile = detect_with_config(&dir, &config);
        assert_eq!(profile.steps.len(), 1);
        assert_eq!(profile.steps[0].name, "custom");
        assert_eq!(profile.steps[0].timeout, Duration::from_secs(20));
    }

    #[test]
    fn detects_python_optional_tools() {
        let dir = temp_dir();
        std::fs::write(dir.join("pyproject.toml"), "[tool.ruff]\n[tool.mypy]").unwrap();
        let profile = detect(&dir);
        assert!(profile.steps.iter().any(|s| s.name == "ruff"));
        assert!(profile.steps.iter().any(|s| s.name == "mypy"));
    }

    #[test]
    fn empty_is_unknown() {
        let profile = detect(&temp_dir());
        assert_eq!(profile.kind, ProjectKind::Unknown);
        assert!(profile.steps.is_empty());
    }
}
