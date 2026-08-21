use std::collections::VecDeque;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use kode_core::CancellationToken;
use kode_core::process::{managed_command, spawn_managed};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{ProjectProfile, StepResult, StepStatus, VerificationReport};

const DIFF_TIMEOUT: Duration = Duration::from_secs(10);
const TAIL_CHARS: usize = 10_000;
const OUTPUT_TRUNCATED_MARKER: &str = "[earlier output truncated]\n";

enum StepOutcome {
    Finished(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

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
        let cwd = match step_directory(root, &verify_step.cwd) {
            Ok(cwd) => cwd,
            Err(error) => {
                if verify_step.required && profile.fail_fast {
                    blocked = Some("previous step failed");
                }
                results.push(failed(&verify_step.name, None, &error, Duration::ZERO));
                continue;
            }
        };

        let start = Instant::now();
        let mut command = managed_command(&verify_step.program);
        command.args(&verify_step.args).current_dir(cwd);
        let managed = match spawn_managed(&mut command) {
            Ok(child) => child,
            Err(error) => {
                if verify_step.required {
                    if profile.fail_fast {
                        blocked = Some("previous step failed");
                    }
                    results.push(failed(
                        &verify_step.name,
                        None,
                        &format!("spawn error: {error}"),
                        start.elapsed(),
                    ));
                } else {
                    results.push(skipped(&verify_step.name, "not available"));
                }
                continue;
            }
        };
        let (mut child, mut tree) = managed.into_parts();
        let stdout = child.stdout.take().expect("managed stdout is piped");
        let stderr = child.stderr.take().expect("managed stderr is piped");
        let stdout_task = tokio::spawn(read_tail(stdout, TAIL_CHARS));
        let stderr_task = tokio::spawn(read_tail(stderr, TAIL_CHARS));

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => StepOutcome::Cancelled,
            _ = tokio::time::sleep(verify_step.timeout) => StepOutcome::TimedOut,
            result = child.wait() => StepOutcome::Finished(result),
        };
        let duration = start.elapsed();

        match outcome {
            StepOutcome::Finished(Ok(status)) => {
                let stdout_tail = join_tail(stdout_task).await;
                let stderr_tail = join_tail(stderr_task).await;
                if status.success() {
                    results.push(StepResult {
                        name: verify_step.name.clone(),
                        status: StepStatus::Passed,
                        exit_code: status.code(),
                        stdout_tail,
                        stderr_tail,
                        duration,
                    });
                } else {
                    if verify_step.required && profile.fail_fast {
                        blocked = Some("previous step failed");
                    }
                    results.push(StepResult {
                        name: verify_step.name.clone(),
                        status: StepStatus::Failed,
                        exit_code: status.code(),
                        stdout_tail,
                        stderr_tail,
                        duration,
                    });
                }
            }
            StepOutcome::Finished(Err(error)) => {
                let _ = join_tail(stdout_task).await;
                let mut stderr = join_tail(stderr_task).await;
                if !stderr.is_empty() {
                    stderr.push('\n');
                }
                stderr.push_str(&format!("io error: {error}"));
                if verify_step.required && profile.fail_fast {
                    blocked = Some("previous step failed");
                }
                results.push(failed(&verify_step.name, None, &stderr, duration));
            }
            StepOutcome::TimedOut => {
                tree.kill_tree();
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                if verify_step.required && profile.fail_fast {
                    blocked = Some("previous step failed");
                }
                results.push(failed(
                    &verify_step.name,
                    None,
                    &format!("timed out after {}s", verify_step.timeout.as_secs()),
                    duration,
                ));
            }
            StepOutcome::Cancelled => {
                tree.kill_tree();
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                blocked = Some("cancelled");
                results.push(failed(&verify_step.name, None, "cancelled", duration));
            }
        }
    }

    let diff_stat = git_diff_stat(root).await;
    let ok = !results
        .iter()
        .any(|result| matches!(result.status, StepStatus::Failed));
    VerificationReport {
        steps: results,
        diff_stat,
        ok,
    }
}

fn step_directory(root: &Path, relative: &Path) -> Result<std::path::PathBuf, String> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(format!("invalid verification cwd: {}", relative.display()));
    }
    let directory = root.join(relative);
    if !directory.is_dir() {
        return Err(format!(
            "verification cwd does not exist: {}",
            relative.display()
        ));
    }
    let canonical_root =
        std::fs::canonicalize(root).map_err(|e| format!("cannot resolve workspace: {e}"))?;
    let canonical_directory = std::fs::canonicalize(&directory)
        .map_err(|e| format!("cannot resolve verification cwd: {e}"))?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(format!(
            "verification cwd escapes workspace: {}",
            relative.display()
        ));
    }
    Ok(canonical_directory)
}

fn skipped(name: &str, reason: &str) -> StepResult {
    StepResult {
        name: name.into(),
        status: StepStatus::Skipped(reason.into()),
        exit_code: None,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        duration: Duration::ZERO,
    }
}
fn failed(name: &str, exit_code: Option<i32>, stderr: &str, duration: Duration) -> StepResult {
    StepResult {
        name: name.into(),
        status: StepStatus::Failed,
        exit_code,
        stdout_tail: String::new(),
        stderr_tail: stderr.into(),
        duration,
    }
}

async fn read_tail<R: AsyncRead + Unpin>(
    mut reader: R,
    max_chars: usize,
) -> std::io::Result<String> {
    let max_bytes = max_chars.saturating_mul(4);
    let mut tail = VecDeque::with_capacity(max_bytes.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut discarded = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        tail.extend(&buffer[..read]);
        if tail.len() > max_bytes {
            discarded = true;
            tail.drain(..tail.len() - max_bytes);
        }
    }
    Ok(tail_chars(
        &tail.into_iter().collect::<Vec<_>>(),
        max_chars,
        discarded,
    ))
}
async fn join_tail(task: tokio::task::JoinHandle<std::io::Result<String>>) -> String {
    match task.await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => format!("output read error: {e}"),
        Err(e) => format!("output reader failed: {e}"),
    }
}
fn tail_chars(bytes: &[u8], max_chars: usize, bytes_discarded: bool) -> String {
    let text = String::from_utf8_lossy(bytes);
    let count = text.chars().count();
    let truncated = bytes_discarded || count > max_chars;
    if !truncated {
        return text.into_owned();
    }
    let marker: String = OUTPUT_TRUNCATED_MARKER.chars().take(max_chars).collect();
    let content_chars = max_chars.saturating_sub(marker.chars().count());
    let tail: String = text
        .chars()
        .skip(count.saturating_sub(content_chars))
        .collect();
    format!("{marker}{tail}")
}

async fn git_diff_stat(root: &Path) -> Option<String> {
    let mut command = managed_command("git");
    command.args(["diff", "--stat"]).current_dir(root);
    let managed = spawn_managed(&mut command).ok()?;
    let (mut child, mut tree) = managed.into_parts();
    let stdout_task = tokio::spawn(read_tail(child.stdout.take()?, TAIL_CHARS));
    let stderr_task = tokio::spawn(read_tail(child.stderr.take()?, TAIL_CHARS));
    let status = tokio::select! {
        result = child.wait() => result.ok()?,
        _ = tokio::time::sleep(DIFF_TIMEOUT) => {
            tree.kill_tree();
            let _ = child.wait().await;
            stdout_task.abort(); stderr_task.abort();
            return None;
        }
    };
    let stdout = join_tail(stdout_task).await;
    let _ = join_tail(stderr_task).await;
    status.success().then(|| stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectKind, VerifyStep};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-run-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn step(name: &str, args: &[&str], required: bool) -> VerifyStep {
        VerifyStep {
            name: name.into(),
            program: "git".into(),
            args: args.iter().map(|s| (*s).into()).collect(),
            cwd: PathBuf::new(),
            required,
            timeout: Duration::from_secs(20),
        }
    }

    #[test]
    fn tails_are_marked_and_bounded() {
        let output = tail_chars(b"0123456789", 32, true);
        assert_eq!(output.chars().count(), 32);
        assert!(output.starts_with(OUTPUT_TRUNCATED_MARKER));
        assert!(output.ends_with("56789"));
        assert_eq!(tail_chars("éééé".as_bytes(), 3, false), "[ea");
    }

    #[tokio::test]
    async fn fail_fast_skips_remaining_required_steps() {
        let profile = ProjectProfile {
            kind: ProjectKind::Unknown,
            steps: vec![
                step("ok", &["--version"], true),
                step("bad", &["not-a-command"], true),
                step("last", &["--version"], true),
            ],
            fail_fast: true,
        };
        let report = run_verification(&temp_dir(), &profile, &CancellationToken::new()).await;
        assert_eq!(report.steps[0].status, StepStatus::Passed);
        assert_eq!(report.steps[1].status, StepStatus::Failed);
        assert!(matches!(report.steps[2].status, StepStatus::Skipped(_)));
    }

    #[tokio::test]
    async fn fail_fast_false_continues() {
        let profile = ProjectProfile {
            kind: ProjectKind::Unknown,
            steps: vec![
                step("bad", &["not-a-command"], true),
                step("last", &["--version"], true),
            ],
            fail_fast: false,
        };
        let report = run_verification(&temp_dir(), &profile, &CancellationToken::new()).await;
        assert_eq!(report.steps[0].status, StepStatus::Failed);
        assert_eq!(report.steps[1].status, StepStatus::Passed);
    }

    #[tokio::test]
    async fn escaping_cwd_is_rejected() {
        let mut invalid = step("invalid", &["--version"], true);
        invalid.cwd = PathBuf::from("..");
        let profile = ProjectProfile {
            kind: ProjectKind::Unknown,
            steps: vec![invalid],
            fail_fast: true,
        };
        let report = run_verification(&temp_dir(), &profile, &CancellationToken::new()).await;
        assert_eq!(report.steps[0].status, StepStatus::Failed);
        assert!(
            report.steps[0]
                .stderr_tail
                .contains("invalid verification cwd")
        );
    }
}
