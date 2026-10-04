//! Graph-selected tests run before the full suite, reported honestly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kode_core::CancellationToken;
use kode_core::config::TargetedMode;

use crate::{
    ProjectKind, ProjectProfile, StepResult, StepStatus, VerificationReport, VerifyStep,
    run_verification,
};

pub fn is_test_step(step: &VerifyStep) -> bool {
    let base = step.name.rsplit(':').next().unwrap_or(&step.name);
    base == "test" || base == "pytest"
}

fn skipped(name: &str, reason: &str) -> StepResult {
    StepResult {
        name: name.to_string(),
        status: StepStatus::Skipped(reason.to_string()),
        exit_code: None,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        duration: Duration::ZERO,
    }
}

/// Nearest ancestor `Cargo.toml` with a `[package] name`.
fn rust_package(root: &Path, file: &str) -> Option<String> {
    let mut dir: PathBuf = root.join(file).parent()?.to_path_buf();
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml"))
            && let Some(name) = text
                .lines()
                .skip_while(|l| l.trim() != "[package]")
                .find_map(|l| {
                    l.trim()
                        .strip_prefix("name")
                        .and_then(|r| r.split('"').nth(1))
                })
        {
            return Some(name.to_string());
        }
        if dir == root || !dir.pop() {
            return None;
        }
    }
}

fn mk(name: String, program: &str, args: Vec<String>, timeout: Duration) -> VerifyStep {
    VerifyStep {
        name,
        program: program.into(),
        args,
        cwd: PathBuf::new(),
        required: true,
        timeout,
    }
}

pub fn targeted_steps(
    root: &Path,
    kind_of: impl Fn(&str) -> ProjectKind,
    tests: &[(String, String)],
    timeout: Duration,
) -> Vec<VerifyStep> {
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new(); // (kind, unit) -> names
    let mut py_files: Vec<String> = Vec::new();
    for (name, file) in tests {
        match kind_of(file) {
            ProjectKind::Rust => {
                if let Some(pkg) = rust_package(root, file) {
                    groups
                        .entry(("rust".into(), pkg))
                        .or_default()
                        .push(name.clone());
                }
            }
            ProjectKind::Go => {
                let dir = file.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
                groups
                    .entry(("go".into(), format!("./{dir}")))
                    .or_default()
                    .push(name.clone());
            }
            ProjectKind::Python => {
                if !py_files.contains(file) {
                    py_files.push(file.clone());
                }
                groups
                    .entry(("py".into(), String::new()))
                    .or_default()
                    .push(name.clone());
            }
            _ => {}
        }
    }
    let python = if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    };
    groups
        .into_iter()
        .map(|((kind, unit), mut names)| {
            names.dedup();
            let label = format!("test·targeted ({})", names.len());
            match kind.as_str() {
                "rust" => {
                    let mut args = vec!["test".into(), "-p".into(), unit, "--".into()];
                    args.extend(names);
                    mk(label, "cargo", args, timeout)
                }
                "go" => mk(
                    label,
                    "go",
                    vec![
                        "test".into(),
                        unit,
                        "-run".into(),
                        format!("^({})$", names.join("|")),
                    ],
                    timeout,
                ),
                _ => {
                    let mut args = vec!["-m".into(), "pytest".into(), "-q".into()];
                    args.extend(py_files.clone());
                    args.push("-k".into());
                    args.push(names.join(" or "));
                    mk(label, python, args, timeout)
                }
            }
        })
        .collect()
}

/// Runs `targeted` first, then the profile, honoring `mode`.
pub(crate) async fn run_with_steps(
    root: &Path,
    profile: &ProjectProfile,
    targeted: Vec<VerifyStep>,
    mode: TargetedMode,
    cancel: &CancellationToken,
) -> VerificationReport {
    let targeted_report = run_verification(
        root,
        &ProjectProfile {
            kind: profile.kind,
            steps: targeted,
            fail_fast: true,
        },
        cancel,
    )
    .await;
    let skip_reason = match (mode, targeted_report.ok) {
        (TargetedMode::Only, _) => Some("targeted mode"),
        (_, false) => Some("targeted tests failed"),
        _ => None,
    };
    let (run_steps, skipped_steps): (Vec<VerifyStep>, Vec<VerifyStep>) = match skip_reason {
        Some(_) => profile
            .steps
            .iter()
            .cloned()
            .partition(|s| !is_test_step(s)),
        None => (profile.steps.clone(), Vec::new()),
    };
    let full = run_verification(
        root,
        &ProjectProfile {
            kind: profile.kind,
            steps: run_steps,
            fail_fast: profile.fail_fast,
        },
        cancel,
    )
    .await;
    let mut steps = targeted_report.steps;
    steps.extend(full.steps);
    if let Some(reason) = skip_reason {
        steps.extend(skipped_steps.iter().map(|s| skipped(&s.name, reason)));
    }
    VerificationReport {
        ok: targeted_report.ok && full.ok,
        diff_stat: full.diff_stat,
        steps,
    }
}

pub async fn run_with_targets(
    root: &Path,
    profile: &ProjectProfile,
    tests: &[(String, String)],
    mode: TargetedMode,
    cancel: &CancellationToken,
) -> VerificationReport {
    if mode == TargetedMode::Off || !profile.steps.iter().any(is_test_step) {
        return run_verification(root, profile, cancel).await;
    }
    let timeout = profile
        .steps
        .iter()
        .find(|s| is_test_step(s))
        .map(|s| s.timeout)
        .unwrap_or(Duration::from_secs(600));
    let kind = profile.kind;
    let kind_of = |file: &str| match kind {
        ProjectKind::Mixed | ProjectKind::Unknown => {
            if file.ends_with(".rs") {
                ProjectKind::Rust
            } else if file.ends_with(".go") {
                ProjectKind::Go
            } else if file.ends_with(".py") {
                ProjectKind::Python
            } else {
                ProjectKind::Unknown
            }
        }
        k => k,
    };
    let targeted = targeted_steps(root, kind_of, tests, timeout);
    if targeted.is_empty() {
        let reason = if tests.is_empty() {
            "no covering tests found"
        } else {
            "no test filter for this project"
        };
        let mut report = run_verification(root, profile, cancel).await;
        report.steps.insert(0, skipped("test·targeted", reason));
        return report;
    }
    run_with_steps(root, profile, targeted, mode, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A step that succeeds on every platform (`true` is not a Windows command).
    fn step(name: &str) -> VerifyStep {
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), "exit 0".to_string()])
        } else {
            ("true", vec![])
        };
        VerifyStep {
            name: name.into(),
            program: program.into(),
            args,
            cwd: Default::default(),
            required: true,
            timeout: Duration::from_secs(5),
        }
    }

    fn temp_rust_ws() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-targeted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("crates/kode-core/src")).unwrap();
        std::fs::write(
            dir.join("crates/kode-core/Cargo.toml"),
            "[package]\nname = \"kode-core\"\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn recognizes_prefixed_test_steps() {
        assert!(is_test_step(&step("test")));
        assert!(is_test_step(&step("web:test")));
        assert!(is_test_step(&step("py:pytest")));
        assert!(!is_test_step(&step("clippy")));
        assert!(!is_test_step(&step("test·targeted (2)")));
    }

    #[test]
    fn rust_targets_group_by_package() {
        let root = temp_rust_ws();
        let steps = targeted_steps(
            &root,
            |_| ProjectKind::Rust,
            &[
                ("parses".into(), "crates/kode-core/src/config.rs".into()),
                ("round_trip".into(), "crates/kode-core/src/event.rs".into()),
            ],
            Duration::from_secs(60),
        );
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "test·targeted (2)");
        assert_eq!(steps[0].program, "cargo");
        assert_eq!(
            steps[0].args,
            vec!["test", "-p", "kode-core", "--", "parses", "round_trip"]
        );
    }

    #[test]
    fn go_and_python_filters() {
        let root = std::env::temp_dir();
        let go = targeted_steps(
            &root,
            |_| ProjectKind::Go,
            &[
                ("TestA".into(), "pkg/a/a_test.go".into()),
                ("TestB".into(), "pkg/a/b_test.go".into()),
            ],
            Duration::from_secs(1),
        );
        assert_eq!(
            go[0].args,
            vec!["test", "./pkg/a", "-run", "^(TestA|TestB)$"]
        );
        let py = targeted_steps(
            &root,
            |_| ProjectKind::Python,
            &[("test_x".into(), "tests/test_api.py".into())],
            Duration::from_secs(1),
        );
        assert_eq!(
            py[0].args,
            vec!["-m", "pytest", "-q", "tests/test_api.py", "-k", "test_x"]
        );
        let node = targeted_steps(
            &root,
            |_| ProjectKind::Node,
            &[("t".into(), "a.test.ts".into())],
            Duration::from_secs(1),
        );
        assert!(node.is_empty());
    }

    #[tokio::test]
    async fn only_mode_reports_full_test_step_skipped_never_passed() {
        let profile = ProjectProfile {
            kind: ProjectKind::Unknown,
            steps: vec![step("fmt"), step("test")],
            fail_fast: false,
        };
        let report = run_with_steps(
            std::path::Path::new("."),
            &profile,
            vec![step("test·targeted (1)")],
            TargetedMode::Only,
            &CancellationToken::new(),
        )
        .await;
        let names: Vec<_> = report
            .steps
            .iter()
            .map(|s| (s.name.as_str(), s.status.clone()))
            .collect();
        assert!(names.contains(&("test", StepStatus::Skipped("targeted mode".into()))));
        assert!(names.iter().any(|(n, _)| *n == "test·targeted (1)"));
    }

    #[tokio::test]
    async fn first_mode_skips_full_tests_when_targeted_fail() {
        let mut failing = step("test·targeted (1)");
        failing.program = if cfg!(windows) {
            "cmd".into()
        } else {
            "false".into()
        };
        if cfg!(windows) {
            failing.args = vec!["/C".into(), "exit 1".into()];
        }
        let profile = ProjectProfile {
            kind: ProjectKind::Unknown,
            steps: vec![step("test")],
            fail_fast: false,
        };
        let report = run_with_steps(
            std::path::Path::new("."),
            &profile,
            vec![failing],
            TargetedMode::First,
            &CancellationToken::new(),
        )
        .await;
        assert!(!report.ok);
        assert!(
            report.steps.iter().any(|s| s.name == "test"
                && s.status == StepStatus::Skipped("targeted tests failed".into()))
        );
    }

    #[tokio::test]
    async fn custom_steps_without_test_step_get_no_targeting() {
        let profile = ProjectProfile {
            kind: ProjectKind::Unknown,
            steps: vec![step("lint")],
            fail_fast: false,
        };
        let report = run_with_targets(
            std::path::Path::new("."),
            &profile,
            &[("t".into(), "x.rs".into())],
            TargetedMode::First,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].name, "lint");
    }

    #[tokio::test]
    async fn no_covering_tests_is_skipped_and_full_runs() {
        let profile = ProjectProfile {
            kind: ProjectKind::Unknown,
            steps: vec![step("test")],
            fail_fast: false,
        };
        let report = run_with_targets(
            std::path::Path::new("."),
            &profile,
            &[],
            TargetedMode::First,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(report.steps[0].name, "test·targeted");
        assert_eq!(
            report.steps[0].status,
            StepStatus::Skipped("no covering tests found".into())
        );
        assert_eq!(report.steps[1].name, "test");
    }
}
