mod detect;
mod run;

pub use detect::detect;
pub use run::run_verification;

use std::time::Duration;

/// The kind of project detected at a repository root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Rust,
    Go,
    Node,
    Python,
    Unknown,
}

/// A single verification step: a program invocation, and whether its
/// failure blocks the rest of the pipeline.
#[derive(Debug, Clone)]
pub struct VerifyStep {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub required: bool,
}

/// The detected project kind plus the ordered steps to run for it.
#[derive(Debug, Clone)]
pub struct ProjectProfile {
    pub kind: ProjectKind,
    pub steps: Vec<VerifyStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Passed,
    Failed,
    Skipped(String),
}

#[derive(Debug, Clone)]
pub struct StepResult {
    pub name: String,
    pub status: StepStatus,
    pub exit_code: Option<i32>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub duration: Duration,
}

#[derive(Debug, Clone)]
pub struct VerificationReport {
    pub steps: Vec<StepResult>,
    pub diff_stat: Option<String>,
    pub ok: bool,
}

impl VerificationReport {
    /// True iff at least one step actually ran (`Passed` or `Failed`).
    /// `Skipped` steps don't count — a report whose every step was skipped
    /// (or which has no steps at all) has not verified anything.
    pub fn ran_any(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s.status, StepStatus::Passed | StepStatus::Failed))
    }

    /// Model- and human-readable rendering of the report: one line per step,
    /// truncated stdout/stderr for failed steps, and a trailing diff stat
    /// section.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            match &step.status {
                StepStatus::Passed => {
                    out.push_str(&format!(
                        "PASS {} ({:.1}s)\n",
                        step.name,
                        step.duration.as_secs_f64()
                    ));
                }
                StepStatus::Failed => {
                    let exit = step
                        .exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    out.push_str(&format!("FAIL {} (exit {exit})\n", step.name));
                    if !step.stdout_tail.is_empty() {
                        let tail = truncate_chars(&step.stdout_tail, 4000);
                        out.push_str("stdout:\n");
                        out.push_str(&tail);
                        if !tail.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    if !step.stderr_tail.is_empty() {
                        let tail = truncate_chars(&step.stderr_tail, 4000);
                        out.push_str("stderr:\n");
                        out.push_str(&tail);
                        if !tail.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                }
                StepStatus::Skipped(reason) => {
                    out.push_str(&format!("SKIP {} ({reason})\n", step.name));
                }
            }
        }
        if let Some(diff) = &self.diff_stat {
            out.push_str("## diff\n");
            out.push_str(diff);
            if !diff.ends_with('\n') {
                out.push('\n');
            }
        }
        if !self.ran_any() {
            out.push_str("verification: no checks ran — unverified\n");
        }
        out
    }

    /// One-line summary, e.g. "verification: 3 passed, 1 failed, 1 skipped".
    /// When nothing actually ran, reads honestly as unverified rather than
    /// "0 passed, 0 failed" (which looks like a clean pass).
    pub fn summary_line(&self) -> String {
        if !self.ran_any() {
            return "verification: no checks ran — unverified".to_string();
        }
        let passed = self
            .steps
            .iter()
            .filter(|s| matches!(s.status, StepStatus::Passed))
            .count();
        let failed = self
            .steps
            .iter()
            .filter(|s| matches!(s.status, StepStatus::Failed))
            .count();
        let skipped = self
            .steps
            .iter()
            .filter(|s| matches!(s.status, StepStatus::Skipped(_)))
            .count();
        format!("verification: {passed} passed, {failed} failed, {skipped} skipped")
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn step(name: &str, status: StepStatus, stdout: &str, stderr: &str) -> StepResult {
        StepResult {
            name: name.to_string(),
            status,
            exit_code: None,
            stdout_tail: stdout.to_string(),
            stderr_tail: stderr.to_string(),
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn empty_report_summary_reads_unverified() {
        let report = VerificationReport {
            steps: Vec::new(),
            diff_stat: None,
            ok: true,
        };
        assert!(!report.ran_any());
        assert_eq!(
            report.summary_line(),
            "verification: no checks ran — unverified"
        );
    }

    #[test]
    fn ran_any_false_when_all_skipped() {
        let report = VerificationReport {
            steps: vec![step(
                "fmt",
                StepStatus::Skipped("not available".to_string()),
                "",
                "",
            )],
            diff_stat: None,
            ok: true,
        };
        assert!(!report.ran_any());
        assert_eq!(
            report.summary_line(),
            "verification: no checks ran — unverified"
        );
    }

    #[test]
    fn ran_any_true_when_a_step_passed_or_failed() {
        let report = VerificationReport {
            steps: vec![step("fmt", StepStatus::Passed, "", "")],
            diff_stat: None,
            ok: true,
        };
        assert!(report.ran_any());
    }

    #[test]
    fn failed_step_render_includes_stdout_only_content() {
        let report = VerificationReport {
            steps: vec![step(
                "test",
                StepStatus::Failed,
                "assertion failed: left == right",
                "",
            )],
            diff_stat: None,
            ok: false,
        };
        let rendered = report.render();
        assert!(rendered.contains("FAIL test"));
        assert!(rendered.contains("stdout:"));
        assert!(rendered.contains("assertion failed: left == right"));
        assert!(!rendered.contains("stderr:"));
    }

    #[test]
    fn failed_step_render_includes_both_stdout_and_stderr() {
        let report = VerificationReport {
            steps: vec![step(
                "test",
                StepStatus::Failed,
                "stdout content here",
                "stderr content here",
            )],
            diff_stat: None,
            ok: false,
        };
        let rendered = report.render();
        assert!(rendered.contains("stdout:"));
        assert!(rendered.contains("stdout content here"));
        assert!(rendered.contains("stderr:"));
        assert!(rendered.contains("stderr content here"));
    }
}
