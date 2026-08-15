use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use kode_core::CancellationToken;
use tokio::process::Command;

use crate::{ProjectProfile, StepResult, StepStatus, VerificationReport};

const STEP_TIMEOUT: Duration = Duration::from_secs(600);
const DIFF_TIMEOUT: Duration = Duration::from_secs(10);
const TAIL_CHARS: usize = 10_000;

enum StepOutcome {
    Finished(std::io::Result<std::process::Output>),
    TimedOut,
    Cancelled,
}

/// Runs every step in `profile` sequentially against `root`.
///
/// A failing required step blocks all remaining steps (they become
/// `Skipped("previous step failed")`). Optional steps whose program can't be
/// spawned are `Skipped("not available")`; a nonzero exit from an optional
/// step is `Failed` but the pipeline continues. `cancel` is checked before
/// each step and honored mid-step (the child is killed).
pub async fn run_verification(
    root: &Path,
    profile: &ProjectProfile,
    cancel: &CancellationToken,
) -> VerificationReport {
    let mut results = Vec::with_capacity(profile.steps.len());
    let mut blocked: Option<&'static str> = None;

    for verify_step in &profile.steps {
        if let Some(reason) = blocked {
            results.push(skipped(&verify_step.name, reason));
            continue;
        }

        if cancel.is_cancelled() {
            blocked = Some("cancelled");
            results.push(failed(&verify_step.name, None, "cancelled", Duration::ZERO));
            continue;
        }

        let start = Instant::now();
        let spawned = Command::new(&verify_step.program)
            .args(&verify_step.args)
            .current_dir(root)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let child = match spawned {
            Ok(child) => child,
            Err(e) => {
                if verify_step.required {
                    blocked = Some("previous step failed");
                    results.push(failed(
                        &verify_step.name,
                        None,
                        &format!("spawn error: {e}"),
                        start.elapsed(),
                    ));
                } else {
                    results.push(skipped(&verify_step.name, "not available"));
                }
                continue;
            }
        };

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => StepOutcome::Cancelled,
            _ = tokio::time::sleep(STEP_TIMEOUT) => StepOutcome::TimedOut,
            res = child.wait_with_output() => StepOutcome::Finished(res),
        };
        let duration = start.elapsed();

        match outcome {
            StepOutcome::Finished(Ok(output)) => {
                let stdout_tail = tail(&output.stdout, TAIL_CHARS);
                let stderr_tail = tail(&output.stderr, TAIL_CHARS);
                let exit_code = output.status.code();
                if output.status.success() {
                    results.push(StepResult {
                        name: verify_step.name.clone(),
                        status: StepStatus::Passed,
                        exit_code,
                        stdout_tail,
                        stderr_tail,
                        duration,
                    });
                } else {
                    if verify_step.required {
                        blocked = Some("previous step failed");
                    }
                    results.push(StepResult {
                        name: verify_step.name.clone(),
                        status: StepStatus::Failed,
                        exit_code,
                        stdout_tail,
                        stderr_tail,
                        duration,
                    });
                }
            }
            StepOutcome::Finished(Err(e)) => {
                if verify_step.required {
                    blocked = Some("previous step failed");
                }
                results.push(failed(
                    &verify_step.name,
                    None,
                    &format!("io error: {e}"),
                    duration,
                ));
            }
            StepOutcome::TimedOut => {
                if verify_step.required {
                    blocked = Some("previous step failed");
                }
                results.push(failed(
                    &verify_step.name,
                    None,
                    "timed out after 600s",
                    duration,
                ));
            }
            StepOutcome::Cancelled => {
                blocked = Some("cancelled");
                results.push(failed(&verify_step.name, None, "cancelled", duration));
            }
        }
    }

    let diff_stat = git_diff_stat(root).await;
    let ok = !results
        .iter()
        .any(|r| matches!(r.status, StepStatus::Failed));
    VerificationReport {
        steps: results,
        diff_stat,
        ok,
    }
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

fn failed(name: &str, exit_code: Option<i32>, stderr_tail: &str, duration: Duration) -> StepResult {
    StepResult {
        name: name.to_string(),
        status: StepStatus::Failed,
        exit_code,
        stdout_tail: String::new(),
        stderr_tail: stderr_tail.to_string(),
        duration,
    }
}

fn tail(bytes: &[u8], max_chars: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    let count = s.chars().count();
    if count <= max_chars {
        s.into_owned()
    } else {
        s.chars().skip(count - max_chars).collect()
    }
}

async fn git_diff_stat(root: &Path) -> Option<String> {
    let spawned = Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(root)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = spawned.ok()?;

    let output = tokio::select! {
        res = child.wait_with_output() => res.ok()?,
        _ = tokio::time::sleep(DIFF_TIMEOUT) => return None,
    };

    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VerifyStep;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("kode-verify-run-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn vstep(name: &str, program: &str, args: &[&str], required: bool) -> VerifyStep {
        VerifyStep {
            name: name.to_string(),
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            required,
        }
    }

    #[tokio::test]
    async fn required_failure_skips_remaining_steps() {
        let dir = temp_dir();
        let profile = ProjectProfile {
            kind: crate::ProjectKind::Unknown,
            steps: vec![
                vstep("git-version", "git", &["--version"], true),
                vstep("git-bad", "git", &["definitely-not-a-subcommand"], true),
                vstep("third", "git", &["--version"], true),
            ],
        };

        let cancel = CancellationToken::new();
        let report = run_verification(&dir, &profile, &cancel).await;

        assert!(!report.ok);
        assert_eq!(report.steps[0].status, StepStatus::Passed);
        assert_eq!(report.steps[1].status, StepStatus::Failed);
        assert_eq!(
            report.steps[2].status,
            StepStatus::Skipped("previous step failed".to_string())
        );

        let rendered = report.render();
        assert!(rendered.contains("PASS git-version"));
        assert!(rendered.contains("FAIL git-bad"));
    }

    #[tokio::test]
    async fn optional_missing_program_is_skipped_not_available() {
        let dir = temp_dir();
        let profile = ProjectProfile {
            kind: crate::ProjectKind::Unknown,
            steps: vec![
                vstep("ok", "git", &["--version"], true),
                vstep("opt-missing", "definitely-missing-program-xyz", &[], false),
            ],
        };

        let cancel = CancellationToken::new();
        let report = run_verification(&dir, &profile, &cancel).await;

        assert!(report.ok);
        assert_eq!(report.steps[0].status, StepStatus::Passed);
        assert_eq!(
            report.steps[1].status,
            StepStatus::Skipped("not available".to_string())
        );
    }

    #[tokio::test]
    async fn precancelled_token_makes_report_not_ok() {
        let dir = temp_dir();
        let profile = ProjectProfile {
            kind: crate::ProjectKind::Unknown,
            steps: vec![vstep("ok", "git", &["--version"], true)],
        };

        let cancel = CancellationToken::new();
        cancel.cancel();
        let report = run_verification(&dir, &profile, &cancel).await;

        assert!(!report.ok);
    }
}
