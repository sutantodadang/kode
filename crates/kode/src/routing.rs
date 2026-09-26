//! Per-task routing around the pipeline: build the route input, ask the
//! router (cancellable), apply its answers to a copy of the config, and log
//! the decision with the task outcome.

use std::path::Path;
use std::sync::Arc;

use kode_context::ContextReranker;
use kode_core::config::{KodeConfig, RouterConfig};
use kode_core::event::{EventBus, KodeEvent, RouteSource};
use kode_core::{CancellationToken, UserInput};
use kode_local::log::{Outcome, append, log_line};
use kode_local::route::{RouteDecision, RouteInput, StaticRouter, TaskRouter};

use crate::pipeline::TaskOutcome;

pub struct Applied {
    pub config: KodeConfig,
    pub plan_mode: bool,
    pub notes: Vec<String>,
}

pub struct Routed {
    pub applied: Applied,
    pub reranker: Option<Arc<dyn ContextReranker>>,
    pub decision: Option<RouteDecision>,
}

fn from_laya<'a>(d: &'a RouteDecision, key: &str) -> Option<&'a str> {
    d.answer(key)
        .filter(|a| a.source == RouteSource::Laya)
        .map(|a| a.value.as_str())
}

/// Applies model answers only; static answers leave the config untouched.
/// Effort changes only when `[model].effort` is set (the provider accepts
/// it); the router can turn plan mode on but never off.
pub fn apply_route(config: &KodeConfig, plan_mode: bool, d: &RouteDecision) -> Applied {
    let mut out = config.clone();
    let mut notes = Vec::new();
    if let Some(tier) = from_laya(d, "tier")
        && let Some(name) = config.router.tiers.get(tier)
    {
        match config.agent.subagents.models.get(name) {
            Some(model) => {
                out.model.provider = model.provider.clone();
                out.model.model = model.model.clone();
            }
            None => notes.push(format!(
                "router tier '{tier}' maps to '{name}', which is not in [agent.subagents.models] — using the root model"
            )),
        }
    }
    if let Some(effort) = from_laya(d, "effort")
        && !config.model.effort.is_empty()
    {
        out.model.effort = effort.to_string();
    }
    let plan_mode = plan_mode || from_laya(d, "plan") == Some("plan");
    Applied {
        config: out,
        plan_mode,
        notes,
    }
}

pub async fn route_with_cancel(
    router: &dyn TaskRouter,
    input: &RouteInput,
    cancel: &CancellationToken,
) -> RouteDecision {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => StaticRouter { reason: "cancelled".to_string() }.route(input).await,
        d = router.route(input) => d,
    }
}

async fn build_route_input(task: &str, cwd: &Path) -> RouteInput {
    let project = format!("{:?}", kode_verify::detect(cwd).kind).to_lowercase();
    let changed_files = kode_context::git::git_state(cwd)
        .await
        .map(|s| s.status.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    RouteInput {
        task: task.to_string(),
        project,
        changed_files,
    }
}

/// Routes one task. With `router.enabled = false` nothing is loaded and no
/// event is emitted.
pub async fn route_task(
    input: &UserInput,
    cwd: &Path,
    config: &KodeConfig,
    plan_mode: bool,
    events: &EventBus,
    cancel: &CancellationToken,
) -> Routed {
    if !config.router.enabled {
        return Routed {
            applied: Applied {
                config: config.clone(),
                plan_mode,
                notes: Vec::new(),
            },
            reranker: None,
            decision: None,
        };
    }
    let stack = crate::local::load(&config.router).await;
    for text in &stack.notes {
        events.emit(KodeEvent::Note { text: text.clone() });
    }
    let route_input = build_route_input(&input.text, cwd).await;
    let decision = route_with_cancel(stack.router.as_ref(), &route_input, cancel).await;
    events.emit(KodeEvent::RouterDecision {
        answers: decision.answers.clone(),
    });
    let applied = apply_route(config, plan_mode, &decision);
    for text in &applied.notes {
        events.emit(KodeEvent::Note { text: text.clone() });
    }
    Routed {
        applied,
        reranker: stack.reranker,
        decision: Some(decision),
    }
}

pub fn outcome_of(result: &anyhow::Result<TaskOutcome>) -> Outcome {
    match result {
        Ok(o) => Outcome {
            status: format!("{:?}", o.status).to_lowercase(),
            verification: format!("{:?}", o.verification).to_lowercase(),
            iterations: o.iterations,
            tool_calls: o.tool_calls,
        },
        Err(_) => Outcome {
            status: "failed".to_string(),
            verification: "not_run".to_string(),
            iterations: 0,
            tool_calls: 0,
        },
    }
}

/// Appends the router log line; returns a note instead of failing the task.
pub fn log_route(
    cwd: &Path,
    router: &RouterConfig,
    task: &str,
    decision: &RouteDecision,
    outcome: Outcome,
) -> Option<String> {
    let path = cwd.join(".kode").join("router-log.jsonl");
    append(&path, &log_line(decision, task, router.log_text, outcome))
        .err()
        .map(|e| format!("router log not written ({}): {e}", path.display()))
}

pub fn rerank_note(status: &str) -> Option<String> {
    (status.starts_with("skipped") || status.starts_with("failed"))
        .then(|| format!("rerank: {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_core::config::ModelTierConfig;
    use kode_local::route::{StaticRouter, resolve_route, route_questions};

    fn onehot(k: usize, i: usize) -> Vec<f32> {
        (0..k).map(|j| if j == i { 1.0 } else { 0.0 }).collect()
    }

    /// tier, effort, plan answers as indexes into their option lists.
    fn decision(tier: usize, effort: usize, plan: usize) -> RouteDecision {
        resolve_route(
            &route_questions(),
            Ok(vec![onehot(3, tier), onehot(3, effort), onehot(2, plan)]),
            0.6,
        )
    }

    fn base() -> KodeConfig {
        let mut c = KodeConfig::default();
        c.model.provider = "openai".to_string();
        c.model.model = "root-model".to_string();
        c.agent.subagents.models.insert(
            "terra".to_string(),
            ModelTierConfig {
                provider: "anthropic".to_string(),
                model: "big".to_string(),
            },
        );
        c.router
            .tiers
            .insert("heavy".to_string(), "terra".to_string());
        c
    }

    #[test]
    fn mapped_tier_switches_root_model() {
        let a = apply_route(&base(), false, &decision(2, 1, 1));
        assert_eq!(a.config.model.provider, "anthropic");
        assert_eq!(a.config.model.model, "big");
        assert!(a.notes.is_empty());
    }

    #[test]
    fn unmapped_tier_keeps_root_model() {
        let a = apply_route(&base(), false, &decision(0, 1, 1));
        assert_eq!(a.config.model.model, "root-model");
    }

    #[test]
    fn unknown_tier_name_keeps_root_model_with_note() {
        let mut c = base();
        c.router
            .tiers
            .insert("heavy".to_string(), "missing".to_string());
        let a = apply_route(&c, false, &decision(2, 1, 1));
        assert_eq!(a.config.model.model, "root-model");
        assert_eq!(a.notes.len(), 1);
        assert!(a.notes[0].contains("missing"));
    }

    #[test]
    fn effort_changes_only_when_already_configured() {
        let mut c = base();
        assert_eq!(
            apply_route(&c, false, &decision(1, 2, 1))
                .config
                .model
                .effort,
            ""
        );
        c.model.effort = "medium".to_string();
        assert_eq!(
            apply_route(&c, false, &decision(1, 2, 1))
                .config
                .model
                .effort,
            "high"
        );
    }

    #[test]
    fn plan_is_enabled_by_router_but_never_disabled() {
        assert!(apply_route(&base(), false, &decision(1, 1, 0)).plan_mode);
        assert!(!apply_route(&base(), false, &decision(1, 1, 1)).plan_mode);
        assert!(apply_route(&base(), true, &decision(1, 1, 1)).plan_mode);
    }

    #[test]
    fn static_answers_change_nothing() {
        let mut c = base();
        c.model.effort = "medium".to_string();
        let d = resolve_route(&route_questions(), Err("disabled".to_string()), 0.6);
        let a = apply_route(&c, false, &d);
        assert_eq!(a.config, c);
        assert!(!a.plan_mode);
    }

    struct SlowRouter;

    #[async_trait::async_trait]
    impl TaskRouter for SlowRouter {
        async fn route(&self, input: &RouteInput) -> RouteDecision {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            StaticRouter {
                reason: "never".to_string(),
            }
            .route(input)
            .await
        }
    }

    #[tokio::test]
    async fn cancel_during_routing_returns_static_immediately() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let input = RouteInput {
            task: "t".to_string(),
            project: "rust".to_string(),
            changed_files: 0,
        };
        let started = std::time::Instant::now();
        let d = route_with_cancel(&SlowRouter, &input, &cancel).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(
            d.answers
                .iter()
                .all(|a| a.source == RouteSource::Static("cancelled".to_string()))
        );
    }

    #[test]
    fn rerank_note_only_for_problems() {
        assert_eq!(rerank_note("off"), None);
        assert_eq!(rerank_note("ok"), None);
        assert_eq!(
            rerank_note("skipped: timeout").as_deref(),
            Some("rerank: skipped: timeout")
        );
        assert_eq!(
            rerank_note("failed: boom").as_deref(),
            Some("rerank: failed: boom")
        );
    }

    #[test]
    fn outcome_maps_errors_to_failed() {
        let o = outcome_of(&Err(anyhow::anyhow!("boom")));
        assert_eq!(o.status, "failed");
        assert_eq!(o.verification, "not_run");
    }

    #[test]
    fn log_route_failure_becomes_note() {
        let dir = std::env::temp_dir().join(format!("kode-routing-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".kode"), b"file, not dir").unwrap();
        let d = decision(1, 1, 1);
        let note = log_route(
            &dir,
            &RouterConfig::default(),
            "t",
            &d,
            outcome_of(&Err(anyhow::anyhow!("x"))),
        );
        assert!(note.unwrap().starts_with("router log not written"));
    }
}
