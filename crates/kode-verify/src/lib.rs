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
    /// Model- and human-readable rendering of the report: one line per step,
    /// truncated stderr for failed steps, and a trailing diff stat section.
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
                    if !step.stderr_tail.is_empty() {
                        let tail = truncate_chars(&step.stderr_tail, 4000);
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
        out
    }

    /// One-line summary, e.g. "verification: 3 passed, 1 failed, 1 skipped".
    pub fn summary_line(&self) -> String {
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
