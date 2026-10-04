//! Graph-selected tests run before the full suite, reported honestly.

use std::collections::{BTreeMap, BTreeSet};
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

/// `dir` relative to `root` (empty when equal).
fn rel_dir(root: &Path, dir: &Path) -> PathBuf {
    dir.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Nearest ancestor `Cargo.toml` with a `[package] name`: the package name and
/// the manifest's directory relative to `root`. Only `name` lines inside the
/// `[package]` section count, so a later `[[bin]] name = ...` is never taken.
fn rust_package(root: &Path, file: &str) -> Option<(String, PathBuf)> {
    let mut dir: PathBuf = root.join(file).parent()?.to_path_buf();
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) {
            let mut in_package = false;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_package = line == "[package]";
                } else if in_package
                    && let Some(rest) = line.strip_prefix("name")
                    && let Some(value) = rest.trim_start().strip_prefix('=')
                    && let Some(name) = value.split('"').nth(1)
                {
                    return Some((name.to_string(), rel_dir(root, &dir)));
                }
            }
        }
        if dir == root || !dir.pop() {
            return None;
        }
    }
}

/// Nearest ancestor directory of `file` (not above `root`) containing one of
/// the marker files, relative to `root`.
fn nearest_dir_with(root: &Path, file: &str, markers: &[&str]) -> Option<PathBuf> {
    let mut dir: PathBuf = root.join(file).parent()?.to_path_buf();
    loop {
        if markers.iter().any(|m| dir.join(m).is_file()) {
            return Some(rel_dir(root, &dir));
        }
        if dir == root || !dir.pop() {
            return None;
        }
    }
}

/// Nearest ancestor directory holding a `go.mod`, relative to `root`.
fn go_module_dir(root: &Path, file: &str) -> Option<PathBuf> {
    nearest_dir_with(root, file, &["go.mod"])
}

/// Nearest Python project directory (pytest/packaging config), relative to
/// `root`; empty (the root) when none is found.
fn python_project_dir(root: &Path, file: &str) -> PathBuf {
    nearest_dir_with(
        root,
        file,
        &["pyproject.toml", "pytest.ini", "setup.cfg", "tox.ini"],
    )
    .unwrap_or_default()
}

fn mk(
    name: String,
    cwd: PathBuf,
    program: &str,
    args: Vec<String>,
    timeout: Duration,
) -> VerifyStep {
    VerifyStep {
        name,
        program: program.into(),
        args,
        cwd,
        required: true,
        timeout,
    }
}

/// One targeted step's accumulated inputs (sorted, unique).
#[derive(Default)]
struct Group {
    names: BTreeSet<String>,
    files: BTreeSet<String>,
}

/// Builds one step per (project dir, unit). Step names are `test·targeted
/// <unit>` with no count, so a check keeps its identity across repair runs.
pub fn targeted_steps(
    root: &Path,
    kind_of: impl Fn(&str) -> ProjectKind,
    tests: &[(String, String)],
    timeout: Duration,
) -> Vec<VerifyStep> {
    // (kind, cwd, unit) -> names/files
    let mut groups: BTreeMap<(&'static str, String, String), Group> = BTreeMap::new();
    for (name, file) in tests {
        match kind_of(file) {
            ProjectKind::Rust => {
                if let Some((pkg, cwd)) = rust_package(root, file) {
                    groups
                        .entry(("rust", slashed(&cwd), pkg))
                        .or_default()
                        .names
                        .insert(name.clone());
                }
            }
            ProjectKind::Go => {
                let dir = file.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
                let (cwd, unit) = match go_module_dir(root, file) {
                    Some(module) => {
                        let module = slashed(&module);
                        let unit = if module == dir {
                            ".".to_string()
                        } else if module.is_empty() {
                            format!("./{dir}")
                        } else {
                            let rest = dir.strip_prefix(&format!("{module}/")).unwrap_or(dir);
                            format!("./{rest}")
                        };
                        (module, unit)
                    }
                    None => (String::new(), format!("./{dir}")),
                };
                groups
                    .entry(("go", cwd, unit))
                    .or_default()
                    .names
                    .insert(name.clone());
            }
            ProjectKind::Python => {
                let cwd = python_project_dir(root, file);
                let cwd_s = slashed(&cwd);
                let rel_file = slashed(
                    Path::new(file)
                        .strip_prefix(&cwd)
                        .unwrap_or(Path::new(file)),
                );
                let group = groups.entry(("py", cwd_s.clone(), cwd_s)).or_default();
                group.names.insert(name.clone());
                group.files.insert(rel_file);
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
        .map(|((kind, cwd, unit), group)| {
            let cwd = PathBuf::from(cwd);
            let names: Vec<String> = group.names.into_iter().collect();
            match kind {
                "rust" => {
                    let label = format!("test·targeted {unit}");
                    let mut args = vec!["test".into(), "-p".into(), unit, "--".into()];
                    args.extend(names);
                    mk(label, cwd, "cargo", args, timeout)
                }
                "go" => mk(
                    format!("test·targeted {unit}"),
                    cwd,
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
                    let label = format!(
                        "test·targeted {}",
                        if unit.is_empty() { "." } else { &unit }
                    );
                    let mut args = vec!["-m".into(), "pytest".into(), "-q".into()];
                    args.extend(group.files);
                    args.push("-k".into());
                    args.push(names.join(" or "));
                    mk(label, cwd, python, args, timeout)
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
        assert!(!is_test_step(&step("test·targeted kode-core")));
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
        assert_eq!(steps[0].name, "test·targeted kode-core");
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
            vec![step("test·targeted pkg")],
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
        assert!(names.iter().any(|(n, _)| *n == "test·targeted pkg"));
    }

    #[tokio::test]
    async fn first_mode_skips_full_tests_when_targeted_fail() {
        let mut failing = step("test·targeted pkg");
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

    fn temp_root(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-targeted-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn nested_rust_crate_runs_in_its_own_dir() {
        let root = temp_root("nested");
        write(
            &root,
            "backend/Cargo.toml",
            "[package]\nname = \"nested-pkg\"\n",
        );
        let steps = targeted_steps(
            &root,
            |_| ProjectKind::Rust,
            &[("it_works".into(), "backend/src/lib.rs".into())],
            Duration::from_secs(60),
        );
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].cwd, Path::new("backend"));
        assert_eq!(steps[0].name, "test·targeted nested-pkg");
        assert_eq!(
            steps[0].args,
            vec!["test", "-p", "nested-pkg", "--", "it_works"]
        );
    }

    #[tokio::test]
    async fn nested_crate_targeted_pass_does_not_skip_real_test_step() {
        let root = temp_root("e2e");
        write(
            &root,
            "backend/Cargo.toml",
            "[package]\nname = \"nested-pkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        );
        write(
            &root,
            "backend/src/lib.rs",
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn it_works() {}\n}\n",
        );
        let mut real = step("backend:test");
        real.cwd = "backend".into();
        let profile = ProjectProfile {
            kind: ProjectKind::Mixed,
            steps: vec![real],
            fail_fast: false,
        };
        let report = run_with_targets(
            &root,
            &profile,
            &[("it_works".into(), "backend/src/lib.rs".into())],
            TargetedMode::First,
            &CancellationToken::new(),
        )
        .await;
        assert!(report.ok, "{:?}", report.steps);
        assert!(
            !report
                .steps
                .iter()
                .any(|s| s.name == "backend:test" && matches!(s.status, StepStatus::Skipped(_)))
        );
    }

    #[test]
    fn duplicate_test_names_are_listed_once() {
        let root = temp_rust_ws();
        let steps = targeted_steps(
            &root,
            |_| ProjectKind::Rust,
            &[
                ("parses".into(), "crates/kode-core/src/a.rs".into()),
                ("round_trip".into(), "crates/kode-core/src/b.rs".into()),
                ("parses".into(), "crates/kode-core/src/c.rs".into()),
            ],
            Duration::from_secs(60),
        );
        assert_eq!(steps[0].args.iter().filter(|a| *a == "parses").count(), 1);
    }

    #[test]
    fn step_name_is_stable_across_test_counts() {
        let root = temp_rust_ws();
        let f = "crates/kode-core/src/a.rs";
        let three: Vec<_> = (0..3).map(|i| (format!("t{i}"), f.to_string())).collect();
        let four: Vec<_> = (0..4).map(|i| (format!("t{i}"), f.to_string())).collect();
        let a = targeted_steps(&root, |_| ProjectKind::Rust, &three, Duration::from_secs(1));
        let b = targeted_steps(&root, |_| ProjectKind::Rust, &four, Duration::from_secs(1));
        assert_eq!(a[0].name, b[0].name);
        assert_eq!(a[0].name, "test·targeted kode-core");
    }

    #[test]
    fn bin_name_after_package_without_name_is_not_used() {
        let root = temp_root("bin");
        write(
            &root,
            "Cargo.toml",
            "[package]\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"tool\"\n",
        );
        let steps = targeted_steps(
            &root,
            |_| ProjectKind::Rust,
            &[("t".into(), "src/lib.rs".into())],
            Duration::from_secs(1),
        );
        assert!(steps.is_empty());
    }

    #[test]
    fn go_nested_module_and_python_project_dirs() {
        let root = temp_root("polyglot");
        write(&root, "svc/go.mod", "module svc\n");
        write(&root, "svc/pkg/a/a_test.go", "package a\n");
        write(&root, "svc/x_test.go", "package svc\n");
        write(&root, "api/pyproject.toml", "[project]\n");
        write(&root, "api/tests/test_api.py", "");
        let go = targeted_steps(
            &root,
            |_| ProjectKind::Go,
            &[
                ("TestA".into(), "svc/pkg/a/a_test.go".into()),
                ("TestX".into(), "svc/x_test.go".into()),
            ],
            Duration::from_secs(1),
        );
        assert_eq!(go.len(), 2);
        assert!(go.iter().all(|s| s.cwd == Path::new("svc")));
        let units: Vec<_> = go.iter().map(|s| s.args[1].as_str()).collect();
        assert!(units.contains(&"./pkg/a") && units.contains(&"."));
        assert!(go.iter().any(|s| s.name == "test·targeted ./pkg/a"));
        let py = targeted_steps(
            &root,
            |_| ProjectKind::Python,
            &[("test_x".into(), "api/tests/test_api.py".into())],
            Duration::from_secs(1),
        );
        assert_eq!(py[0].cwd, Path::new("api"));
        assert_eq!(py[0].name, "test·targeted api");
        assert_eq!(
            py[0].args,
            vec!["-m", "pytest", "-q", "tests/test_api.py", "-k", "test_x"]
        );
    }
}
