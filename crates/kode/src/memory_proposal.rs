//! Memory as byproduct: decide (deterministically) when a turn is worth
//! remembering, draft one sentence, and gate it before offering it.

use std::time::Duration;

use kode_memory::MemoryKind;
use kode_memory::policy::{self, PolicyDecision};
use kode_memory::{EngineeringMemory, MemoryQuery};
use kode_model::{Message, Model, ModelRequest, collect_response};

use crate::ledger::{LedgerEntry, TurnSignals, VerifyOutcome};
use crate::session::Turn;

const MAX_DRAFT_CHARS: usize = 160;
const REJECT_OVER_CHARS: usize = 200;
const DRAFT_TIMEOUT: Duration = Duration::from_secs(30);
const DRAFT_MAX_TOKENS: u32 = 80;
/// ponytail: assumes Ingat scores in [0, 1]; revisit if the store reports raw BM25.
const DUPLICATE_SCORE: f32 = 0.9;

#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    FixedAfterFailure { checks: Vec<String> },
    Reverted { paths: Vec<String> },
    Steered { messages: Vec<String> },
    RepeatedFailure { tool: String, error: String },
}

impl Trigger {
    pub fn kind(&self) -> MemoryKind {
        match self {
            Trigger::FixedAfterFailure { .. } | Trigger::RepeatedFailure { .. } => {
                MemoryKind::KnownIssue
            }
            Trigger::Reverted { .. } => MemoryKind::RejectedApproach,
            Trigger::Steered { .. } => MemoryKind::UserPreference,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Trigger::FixedAfterFailure { checks } => format!(
                "checks failed, then passed after a fix: {}",
                checks.join(", ")
            ),
            Trigger::Reverted { paths } => {
                format!("an earlier change was undone in: {}", paths.join(", "))
            }
            Trigger::Steered { messages } => {
                format!("the user redirected the agent: {}", messages.join(" | "))
            }
            Trigger::RepeatedFailure { tool, error } => {
                format!("`{tool}` failed repeatedly with: {error}")
            }
        }
    }
}

fn fixed_checks(turn: &Turn) -> Vec<String> {
    let mut failed: Vec<&str> = Vec::new();
    let mut fixed: Vec<String> = Vec::new();
    for entry in &turn.ledger {
        if let LedgerEntry::Verify { name, outcome, .. } = entry {
            match outcome {
                VerifyOutcome::Failed => failed.push(name),
                VerifyOutcome::Passed
                    if failed.contains(&name.as_str()) && !fixed.contains(name) =>
                {
                    fixed.push(name.clone())
                }
                _ => {}
            }
        }
    }
    fixed
}

fn reverted_from_earlier(turn: &Turn, earlier: &[Turn]) -> Vec<String> {
    let touched_before = |path: &str| {
        earlier.iter().flat_map(|t| &t.ledger).any(|e| {
            matches!(e, LedgerEntry::Change { files, .. } if files.iter().any(|f| f.path == path))
        })
    };
    turn.ledger
        .iter()
        .filter_map(|e| match e {
            LedgerEntry::Change { reverted, .. } => Some(reverted),
            _ => None,
        })
        .flatten()
        .filter(|p| touched_before(p))
        .cloned()
        .collect()
}

pub fn detect(turn: &Turn, earlier: &[Turn], signals: &TurnSignals) -> Option<Trigger> {
    let checks = fixed_checks(turn);
    if !checks.is_empty() {
        return Some(Trigger::FixedAfterFailure { checks });
    }
    let paths = reverted_from_earlier(turn, earlier);
    if !paths.is_empty() {
        return Some(Trigger::Reverted { paths });
    }
    if !signals.steering.is_empty() {
        return Some(Trigger::Steered {
            messages: signals.steering.clone(),
        });
    }
    signals.tool_errors.iter().find_map(|(tool, error)| {
        let count = signals
            .tool_errors
            .iter()
            .filter(|(t, e)| t == tool && e == error)
            .count();
        (count >= 2).then(|| Trigger::RepeatedFailure {
            tool: tool.clone(),
            error: error.clone(),
        })
    })
}

pub fn draft_prompt(trigger: &Trigger, task: &str) -> String {
    format!(
        "You keep a short engineering memory for this repository.\n\
         Something in the last task may be worth remembering next time.\n\n\
         Task: {task}\n\
         What happened: {}\n\n\
         Write ONE durable, imperative sentence (at most 160 characters) that a teammate should \
         know before doing similar work, or reply exactly NONE if nothing durable was learned. \
         No hedging words, no secrets.",
        trigger.describe()
    )
}

pub fn parse_draft(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    // `NONE`, `None.`, `NONE - nothing durable` all mean no proposal;
    // `NONEXISTENT config ...` and `None of the tests may ...` are real sentences.
    let upper = line.to_ascii_uppercase();
    let is_none = upper.strip_prefix("NONE").is_some_and(|rest| {
        rest.trim_start()
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric())
    });
    if is_none || line.is_empty() || line.chars().count() > REJECT_OVER_CHARS {
        return None;
    }
    Some(line.chars().take(MAX_DRAFT_CHARS).collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub kind: MemoryKind,
    pub text: String,
    pub files: Vec<String>,
}

fn changed_files(turn: &Turn) -> Vec<String> {
    turn.ledger
        .iter()
        .filter_map(|e| match e {
            LedgerEntry::Change { files, .. } => Some(files.iter().map(|f| f.path.clone())),
            _ => None,
        })
        .flatten()
        .collect()
}

/// One drafting request, then the gates. `None` on any miss; failures are
/// debug-logged only — the task outcome is already final.
pub async fn propose(
    model: &dyn Model,
    memory: Option<&dyn EngineeringMemory>,
    repository: Option<String>,
    trigger: &Trigger,
    turn: &Turn,
) -> Option<Proposal> {
    let request = ModelRequest {
        messages: vec![Message::User(draft_prompt(trigger, &turn.task))],
        max_tokens: Some(DRAFT_MAX_TOKENS),
        ..Default::default()
    };
    let reply = tokio::time::timeout(DRAFT_TIMEOUT, async {
        let stream = model.stream(request).await.ok()?;
        collect_response(stream).await.ok()
    })
    .await
    .ok()
    .flatten();
    let Some(reply) = reply else {
        tracing::debug!("memory proposal: drafting request failed or timed out");
        return None;
    };
    let text = parse_draft(&reply.content)?;
    if let PolicyDecision::Reject(reason) = policy::evaluate(&text, "") {
        tracing::debug!(%reason, "memory proposal rejected by policy");
        return None;
    }
    if let Some(memory) = memory {
        let query = MemoryQuery {
            text: text.clone(),
            repository,
            kind: None,
            limit: 1,
        };
        if let Ok(hits) = memory.search(&query).await
            && hits.first().is_some_and(|m| m.score >= DUPLICATE_SCORE)
        {
            return None;
        }
    }
    Some(Proposal {
        kind: trigger.kind(),
        text,
        files: changed_files(turn),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{ChangeRecord, LedgerEntry, TurnSignals, VerifyOutcome};
    use crate::session::Turn;
    use kode_model::{FinishReason, MockModel, StreamEvent};

    fn turn(ledger: Vec<LedgerEntry>) -> Turn {
        Turn {
            ts: "t".into(),
            task: "fix flaky tests".into(),
            images: vec![],
            response: "r".into(),
            tool_calls: 1,
            ledger,
        }
    }

    fn verify(name: &str, outcome: VerifyOutcome) -> LedgerEntry {
        LedgerEntry::Verify {
            name: name.into(),
            outcome,
            duration_ms: 1,
        }
    }

    #[test]
    fn no_trigger_no_proposal() {
        let t = turn(vec![verify("test", VerifyOutcome::Passed)]);
        assert_eq!(detect(&t, &[], &TurnSignals::default()), None);
    }

    #[test]
    fn fixed_after_failure_needs_fail_then_pass_of_same_check() {
        let t = turn(vec![
            verify("test", VerifyOutcome::Failed),
            verify("test", VerifyOutcome::Passed),
        ]);
        assert_eq!(
            detect(&t, &[], &TurnSignals::default()),
            Some(Trigger::FixedAfterFailure {
                checks: vec!["test".into()]
            })
        );
        let still_failing = turn(vec![
            verify("test", VerifyOutcome::Failed),
            verify("test", VerifyOutcome::Failed),
        ]);
        assert_eq!(detect(&still_failing, &[], &TurnSignals::default()), None);
    }

    #[test]
    fn reverted_needs_an_earlier_turn_that_changed_the_file() {
        let earlier = turn(vec![LedgerEntry::Change {
            files: vec![ChangeRecord {
                path: "retry.rs".into(),
                added: 9,
                removed: 0,
            }],
            reverted: vec![],
        }]);
        let now = turn(vec![LedgerEntry::Change {
            files: vec![],
            reverted: vec!["retry.rs".into(), "other.rs".into()],
        }]);
        assert_eq!(
            detect(&now, &[earlier], &TurnSignals::default()),
            Some(Trigger::Reverted {
                paths: vec!["retry.rs".into()]
            })
        );
        assert_eq!(detect(&now, &[], &TurnSignals::default()), None);
    }

    #[test]
    fn precedence_and_signals() {
        let signals = TurnSignals {
            steering: vec!["use tokio::time::timeout".into()],
            tool_errors: vec![
                ("run_command".into(), "port in use".into()),
                ("run_command".into(), "port in use".into()),
            ],
        };
        assert_eq!(
            detect(&turn(vec![]), &[], &signals),
            Some(Trigger::Steered {
                messages: vec!["use tokio::time::timeout".into()]
            })
        );
        let only_errors = TurnSignals {
            steering: vec![],
            ..signals.clone()
        };
        assert_eq!(
            detect(&turn(vec![]), &[], &only_errors),
            Some(Trigger::RepeatedFailure {
                tool: "run_command".into(),
                error: "port in use".into()
            })
        );
        let fixed = turn(vec![
            verify("clippy", VerifyOutcome::Failed),
            verify("clippy", VerifyOutcome::Passed),
        ]);
        assert!(matches!(
            detect(&fixed, &[], &signals),
            Some(Trigger::FixedAfterFailure { .. })
        ));
    }

    #[test]
    fn kinds_follow_the_spec() {
        assert_eq!(
            Trigger::FixedAfterFailure { checks: vec![] }.kind(),
            MemoryKind::KnownIssue
        );
        assert_eq!(
            Trigger::Reverted { paths: vec![] }.kind(),
            MemoryKind::RejectedApproach
        );
        assert_eq!(
            Trigger::Steered { messages: vec![] }.kind(),
            MemoryKind::UserPreference
        );
        assert_eq!(
            Trigger::RepeatedFailure {
                tool: String::new(),
                error: String::new()
            }
            .kind(),
            MemoryKind::KnownIssue
        );
    }

    #[test]
    fn parse_draft_normalizes() {
        assert_eq!(parse_draft("  NONE "), None);
        assert_eq!(parse_draft("none."), None);
        assert_eq!(parse_draft("none"), None);
        assert_eq!(parse_draft("None."), None);
        assert_eq!(parse_draft("NONE - nothing durable was learned."), None);
        assert_eq!(
            parse_draft("NONEXISTENT config breaks builds"),
            Some("NONEXISTENT config breaks builds".into())
        );
        assert_eq!(
            parse_draft("None of the integration tests may run in parallel."),
            Some("None of the integration tests may run in parallel.".into())
        );
        assert_eq!(
            parse_draft("\"Run tests serially on Windows.\"\nextra"),
            Some("Run tests serially on Windows.".into())
        );
        assert_eq!(parse_draft(&"x".repeat(201)), None);
        assert_eq!(parse_draft(&"é".repeat(170)).unwrap().chars().count(), 160);
        assert_eq!(parse_draft(""), None);
    }

    #[test]
    fn prompt_contains_trigger_task_and_rules() {
        let p = draft_prompt(
            &Trigger::FixedAfterFailure {
                checks: vec!["test".into()],
            },
            "fix flaky tests",
        );
        assert!(p.contains("Task: fix flaky tests"));
        assert!(p.contains("failed, then passed after a fix: test"));
        assert!(p.contains("or reply exactly NONE"));
    }

    fn scripted_model(text: &str) -> MockModel {
        let model = MockModel::new();
        model.push_script(vec![
            StreamEvent::TextDelta(text.to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);
        model
    }

    async fn run_propose(model_text: &str, existing: Vec<kode_memory::Memory>) -> Option<Proposal> {
        let model = scripted_model(model_text);
        let mem = kode_memory::MockEngineeringMemory {
            search_results: existing,
            ..Default::default()
        };
        let t = turn(vec![
            verify("test", VerifyOutcome::Failed),
            verify("test", VerifyOutcome::Passed),
        ]);
        let trigger = detect(&t, &[], &TurnSignals::default()).unwrap();
        propose(&model, Some(&mem), Some("kode".into()), &trigger, &t).await
    }

    #[tokio::test]
    async fn accepted_draft_becomes_a_proposal() {
        let p = run_propose(
            "Run integration tests with --test-threads=1 on Windows; ports clash.",
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(p.kind, MemoryKind::KnownIssue);
        assert!(p.text.starts_with("Run integration tests"));
    }

    #[tokio::test]
    async fn policy_rejection_suppresses_offer() {
        assert!(
            run_propose("Maybe use a longer timeout here.", vec![])
                .await
                .is_none()
        );
        assert!(
            run_propose(
                "Set API_KEY=sk-live-0123456789abcdef0123 before tests.",
                vec![]
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn near_duplicate_is_dropped() {
        let dup = kode_memory::Memory {
            id: "m1".into(),
            kind: Some(MemoryKind::KnownIssue),
            summary: "Run tests serially on Windows".into(),
            body: String::new(),
            tags: vec![],
            provenance: None,
            score: 0.93,
            project: "kode".into(),
            created_at: String::new(),
        };
        assert!(
            run_propose("Run tests serially on Windows.", vec![dup])
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn none_reply_is_no_proposal() {
        assert!(run_propose("NONE", vec![]).await.is_none());
    }
}
