mod detect;
mod run;

pub use detect::{detect, detect_with_config};
pub use run::run_verification;

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Rust,
    Go,
    Node,
    Python,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct VerifyStep {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    /// Workspace-relative working directory.
    pub cwd: PathBuf,
    pub required: bool,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ProjectProfile {
    pub kind: ProjectKind,
    pub steps: Vec<VerifyStep>,
    pub fail_fast: bool,
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
    pub fn ran_any(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s.status, StepStatus::Passed | StepStatus::Failed))
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            match &step.status {
                StepStatus::Passed => out.push_str(&format!(
                    "PASS {} ({:.1}s)\n",
                    step.name,
                    step.duration.as_secs_f64()
                )),
                StepStatus::Failed => {
                    let exit = step
                        .exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    out.push_str(&format!("FAIL {} (exit {exit})\n", step.name));
                    for (label, value) in
                        [("stdout", &step.stdout_tail), ("stderr", &step.stderr_tail)]
                    {
                        if !value.is_empty() {
                            let tail = truncate_chars(value, 4000);
                            out.push_str(&format!("{label}:\n{tail}"));
                            if !tail.ends_with('\n') {
                                out.push('\n');
                            }
                        }
                    }
                }
                StepStatus::Skipped(reason) => {
                    out.push_str(&format!("SKIP {} ({reason})\n", step.name))
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
    fn step(status: StepStatus) -> StepResult {
        StepResult {
            name: "check".into(),
            status,
            exit_code: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn skipped_only_is_honestly_unverified() {
        let report = VerificationReport {
            steps: vec![step(StepStatus::Skipped("missing".into()))],
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
    fn failed_output_is_rendered() {
        let mut failed = step(StepStatus::Failed);
        failed.stdout_tail = "assertion failed".into();
        failed.stderr_tail = "trace".into();
        let rendered = VerificationReport {
            steps: vec![failed],
            diff_stat: None,
            ok: false,
        }
        .render();
        assert!(rendered.contains("stdout:\nassertion failed"));
        assert!(rendered.contains("stderr:\ntrace"));
    }
}
