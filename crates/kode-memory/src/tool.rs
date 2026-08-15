use std::sync::Arc;

use serde::Deserialize;

use kode_tools::error::ToolError;
use kode_tools::{RequiredPermission, Tool, ToolContext, ToolOutput};

use crate::EngineeringMemory;
use crate::policy::{self, PolicyDecision};
use crate::types::{MemoryContext, MemoryKind, NewMemory, Provenance};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    summary: String,
    #[serde(default)]
    body: String,
    kind: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    symbols: Vec<String>,
}

/// A tool exposed to the model that lets it write durable engineering
/// memory (project rules, conventions, decisions, known issues, solutions)
/// as it works. Every write here carries [`Provenance::AgentInference`] and
/// is gated by [`policy::evaluate`] — unlike `kode remember` (explicit user
/// intent), the model's own claims need a plausibility check before they
/// become durable, shared knowledge.
///
/// `required_permission()` is `Mutating`, so the existing permission
/// pipeline (Ask/Allow/Deny) is the confirmation mechanism for now.
// ponytail: a per-tool "always confirm remember, regardless of global mode"
// toggle would be a reasonable upgrade if agent-initiated memory writes turn
// out to need stricter confirmation than other mutating tools — not built
// here since the existing Ask/Allow/Deny gate already covers it today.
pub struct RememberTool {
    memory: Arc<dyn EngineeringMemory>,
    repository: Option<String>,
}

impl RememberTool {
    pub fn new(memory: Arc<dyn EngineeringMemory>, repository: Option<String>) -> Self {
        Self { memory, repository }
    }
}

#[async_trait::async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Store a DURABLE engineering fact you have verified — a project rule, \
         convention, architecture decision, known issue, build fact, rejected \
         approach, user preference, or historical solution. Do not use this for \
         guesses, hypotheses, or anything you are not confident about."
    }

    fn parameters(&self) -> serde_json::Value {
        let kinds: Vec<&str> = MemoryKind::ALL.iter().map(MemoryKind::as_kebab).collect();
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "One-line summary of the fact." },
                "body": { "type": "string", "description": "Optional longer explanation." },
                "kind": { "type": "string", "enum": kinds },
                "tags": { "type": "array", "items": { "type": "string" } },
                "files": { "type": "array", "items": { "type": "string" } },
                "symbols": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["summary", "kind"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::Mutating
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> kode_tools::error::Result<ToolOutput> {
        let args: Args = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: e.to_string(),
        })?;

        let kind = MemoryKind::from_kebab(&args.kind).ok_or_else(|| {
            let valid: Vec<&str> = MemoryKind::ALL.iter().map(MemoryKind::as_kebab).collect();
            ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: format!(
                    "invalid kind {:?}; valid values: {}",
                    args.kind,
                    valid.join(", ")
                ),
            }
        })?;

        match policy::evaluate(&args.summary, &args.body) {
            PolicyDecision::Reject(reason) => {
                return Err(ToolError::Failed(format!("memory rejected: {reason}")));
            }
            PolicyDecision::Accept => {}
        }

        let body = if args.body.is_empty() {
            args.summary.clone()
        } else {
            args.body.clone()
        };

        let new_memory = NewMemory {
            kind,
            summary: args.summary,
            body,
            tags: args.tags,
            provenance: Provenance::AgentInference,
            context: MemoryContext {
                repository: self.repository.clone(),
                files: args.files,
                symbols: args.symbols,
                ..Default::default()
            },
        };

        let id = self
            .memory
            .remember(&new_memory)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;

        Ok(ToolOutput {
            content: format!("remembered ({}): {id}", new_memory.kind.as_kebab()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockEngineeringMemory;
    use kode_core::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn stores_memory_with_agent_inference_provenance() {
        let mock = Arc::new(MockEngineeringMemory::default());
        let tool = RememberTool::new(mock.clone(), Some("kode".to_string()));

        let out = tool
            .execute(
                serde_json::json!({
                    "summary": "We use cargo-nextest for all test runs",
                    "kind": "project-rule",
                }),
                &ctx(),
            )
            .await
            .unwrap();

        assert!(out.content.contains("remembered (project-rule)"));

        let recorded = mock.remembered_snapshot().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].provenance, Provenance::AgentInference);
        assert_eq!(recorded[0].kind, MemoryKind::ProjectRule);
        assert_eq!(recorded[0].context.repository, Some("kode".to_string()));
    }

    #[tokio::test]
    async fn hedged_summary_is_rejected() {
        let mock = Arc::new(MockEngineeringMemory::default());
        let tool = RememberTool::new(mock, None);

        let err = tool
            .execute(
                serde_json::json!({
                    "summary": "I think this might be the right convention",
                    "kind": "convention",
                }),
                &ctx(),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("uncertain"));
    }

    #[tokio::test]
    async fn secret_body_is_rejected() {
        let mock = Arc::new(MockEngineeringMemory::default());
        let tool = RememberTool::new(mock, None);

        let err = tool
            .execute(
                serde_json::json!({
                    "summary": "deployment config",
                    "body": "the api_key for staging is stored in .env",
                    "kind": "build-knowledge",
                }),
                &ctx(),
            )
            .await
            .unwrap_err();

        assert!(err.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn invalid_kind_is_rejected() {
        let mock = Arc::new(MockEngineeringMemory::default());
        let tool = RememberTool::new(mock, None);

        let err = tool
            .execute(
                serde_json::json!({
                    "summary": "some durable fact worth keeping",
                    "kind": "not-a-real-kind",
                }),
                &ctx(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }
}
