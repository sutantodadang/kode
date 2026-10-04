//! Router training capture: after a routed, completed task, ask the
//! teacher (the task's own model) for hindsight labels and append a record
//! to `.kode/router/dataset.jsonl`. Never affects the task's result.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use kode_core::secrets::looks_like_secret;
use kode_local::dataset::{self, Line, Record, TaskSummary, TeacherLabel};
use kode_local::route::{RouteDecision, RouteInput, questions_version};
use kode_local::teacher::{parse_teacher, teacher_prompt};
use kode_model::{Message, Model, ModelRequest, collect_response};

use crate::pipeline::{TaskOutcome, TaskStatus};

const TEACHER_TIMEOUT: Duration = Duration::from_secs(90);
const TEACHER_MAX_TOKENS: u32 = 300;

pub async fn ask_teacher(
    model: &dyn Model,
    state: &str,
    summary: &TaskSummary,
) -> Result<BTreeMap<String, Vec<f32>>, String> {
    let request = ModelRequest {
        messages: vec![Message::User(teacher_prompt(state, summary))],
        max_tokens: Some(TEACHER_MAX_TOKENS),
        ..Default::default()
    };
    let answer = tokio::time::timeout(TEACHER_TIMEOUT, async {
        let stream = model
            .stream(request)
            .await
            .map_err(|e| format!("teacher call failed: {e}"))?;
        collect_response(stream)
            .await
            .map_err(|e| format!("teacher call failed: {e}"))
    })
    .await
    .map_err(|_| "teacher call timed out".to_string())??;
    parse_teacher(&answer.content)
}

pub fn summary_of(outcome: &TaskOutcome, files_changed: usize) -> TaskSummary {
    TaskSummary {
        iterations: outcome.iterations,
        tool_calls: outcome.tool_calls,
        files_changed,
        mutated: outcome.mutated,
        verification: format!("{:?}", outcome.verification).to_lowercase(),
        repair_attempted: outcome.repair_attempted,
    }
}

pub struct CaptureInput<'a> {
    pub root: &'a Path,
    /// `provider/model`, recorded as the teacher's identity.
    pub model_id: String,
    pub author: String,
    pub route_input: &'a RouteInput,
    pub decision: &'a RouteDecision,
    pub outcome: &'a TaskOutcome,
    pub files_changed: usize,
}

/// Returns a transcript note, or `None` when there is nothing to say
/// (task not completed).
pub async fn capture(model: &dyn Model, c: CaptureInput<'_>) -> Option<String> {
    if c.outcome.status != TaskStatus::Completed {
        return None;
    }
    let state = c.route_input.state();
    if looks_like_secret(&state) {
        return Some(
            "router: training record skipped (task text looks like it contains a secret)"
                .to_string(),
        );
    }
    let summary = summary_of(c.outcome, c.files_changed);
    let probs = match ask_teacher(model, &state, &summary).await {
        Ok(probs) => probs,
        Err(e) => return Some(format!("router: label skipped ({e})")),
    };
    let id = dataset::new_id();
    let record = Record {
        v: 1,
        id: id.clone(),
        ts: dataset::now_secs(),
        author: c.author,
        kode: env!("CARGO_PKG_VERSION").to_string(),
        questions_version: questions_version(),
        state,
        laya: c.decision.probs.iter().cloned().collect(),
        teacher: Some(TeacherLabel {
            model: c.model_id,
            probs,
        }),
        outcome: summary,
    };
    let path = dataset::dataset_path(c.root);
    Some(match dataset::append(&path, &Line::Record(record)) {
        Ok(()) => format!("router: labeled for training ({id})"),
        Err(e) => format!(
            "router: training record not written ({}): {e}",
            path.display()
        ),
    })
}

pub async fn git_user_name(root: &Path) -> String {
    tokio::process::Command::new("git")
        .args(["config", "user.name"])
        .current_dir(root)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_local::route::{resolve_route, route_questions};
    use kode_model::{FinishReason, MockModel, StreamEvent, Usage};

    use crate::pipeline::VerificationStatus;

    const VALID: &str = r#"{"tier":{"light":0.1,"standard":0.3,"heavy":0.6},"effort":{"low":0.2,"medium":0.5,"high":0.3},"plan":{"plan":0.7,"direct":0.3},"answer":{"graph":0.3,"model":0.7},"graph_query":{"definition":0.4,"callers":0.2,"callees":0.2,"impact":0.1,"structure":0.1}}"#;

    fn scripted(answer: &str) -> MockModel {
        let m = MockModel::new();
        m.push_script(vec![
            StreamEvent::TextDelta(answer.to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);
        m
    }

    fn outcome(status: TaskStatus) -> TaskOutcome {
        TaskOutcome {
            status,
            mutated: true,
            verification: VerificationStatus::Verified,
            repair_attempted: false,
            iterations: 3,
            tool_calls: 7,
            usage: Usage::default(),
        }
    }

    fn root(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kode-training-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn input(task: &str) -> RouteInput {
        RouteInput {
            task: task.to_string(),
            project: "rust".to_string(),
            changed_files: 1,
        }
    }

    #[tokio::test]
    async fn ask_teacher_sends_short_request_and_parses_answer() {
        let model = scripted(VALID);
        let summary = summary_of(&outcome(TaskStatus::Completed), 2);
        let labels = ask_teacher(&model, "task: x", &summary).await.unwrap();
        assert_eq!(labels["tier"], vec![0.1, 0.3, 0.6]);
        let req = &model.requests()[0];
        assert_eq!(req.max_tokens, Some(300));
        assert!(req.effort.is_none());
    }

    #[tokio::test]
    async fn capture_appends_a_labeled_record() {
        let dir = root("ok");
        let decision = resolve_route(
            &route_questions(),
            Ok(vec![
                vec![0.0, 0.0, 1.0],
                vec![1.0, 0.0, 0.0],
                vec![1.0, 0.0],
                vec![0.0, 1.0],
                vec![1.0, 0.0, 0.0, 0.0, 0.0],
            ]),
            0.6,
        );
        let route_input = input("refactor providers");
        let out = outcome(TaskStatus::Completed);
        let note = capture(
            &scripted(VALID),
            CaptureInput {
                root: &dir,
                model_id: "anthropic/claude".to_string(),
                author: "ana".to_string(),
                route_input: &route_input,
                decision: &decision,
                outcome: &out,
                files_changed: 2,
            },
        )
        .await
        .unwrap();
        assert!(note.starts_with("router: labeled for training"));
        let ds = dataset::read(&dataset::dataset_path(&dir)).unwrap();
        let r = &ds.records[0];
        assert_eq!(r.questions_version, questions_version());
        assert_eq!(r.teacher.as_ref().unwrap().model, "anthropic/claude");
        assert_eq!(r.laya["tier"], vec![0.0, 0.0, 1.0]);
        assert_eq!(r.outcome.files_changed, 2);
        assert!(r.state.contains("refactor providers"));
    }

    #[tokio::test]
    async fn secrets_are_never_stored() {
        let dir = root("secret");
        let decision = resolve_route(&route_questions(), Err("x".to_string()), 0.6);
        let route_input = input("use password hunter2 for the db");
        let out = outcome(TaskStatus::Completed);
        let note = capture(
            &scripted(VALID),
            CaptureInput {
                root: &dir,
                model_id: "m".to_string(),
                author: String::new(),
                route_input: &route_input,
                decision: &decision,
                outcome: &out,
                files_changed: 0,
            },
        )
        .await
        .unwrap();
        assert!(note.contains("secret"));
        assert!(!dataset::dataset_path(&dir).exists());
    }

    #[tokio::test]
    async fn invalid_teacher_answer_is_a_note_not_a_record() {
        let dir = root("invalid");
        let decision = resolve_route(&route_questions(), Err("x".to_string()), 0.6);
        let route_input = input("explain x");
        let out = outcome(TaskStatus::Completed);
        let note = capture(
            &scripted("I think heavy."),
            CaptureInput {
                root: &dir,
                model_id: "m".to_string(),
                author: String::new(),
                route_input: &route_input,
                decision: &decision,
                outcome: &out,
                files_changed: 0,
            },
        )
        .await
        .unwrap();
        assert!(note.starts_with("router: label skipped"));
        assert!(!dataset::dataset_path(&dir).exists());
    }

    #[tokio::test]
    async fn cancelled_tasks_are_not_labeled() {
        let dir = root("cancelled");
        let decision = resolve_route(&route_questions(), Err("x".to_string()), 0.6);
        let route_input = input("explain x");
        let out = outcome(TaskStatus::Cancelled);
        let note = capture(
            &scripted(VALID),
            CaptureInput {
                root: &dir,
                model_id: "m".to_string(),
                author: String::new(),
                route_input: &route_input,
                decision: &decision,
                outcome: &out,
                files_changed: 0,
            },
        )
        .await;
        assert!(note.is_none());
        assert!(!dataset::dataset_path(&dir).exists());
    }
}
