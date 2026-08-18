use std::sync::Arc;

use serde::Deserialize;

use kode_tools::error::ToolError;
use kode_tools::{RequiredPermission, Tool, ToolContext, ToolOutput};

use crate::EngineeringMemory;
use crate::policy::{self, PolicyDecision};
use crate::types::{MemoryContext, MemoryKind, NewMemory, Provenance};
use crate::wire::{self, WireEntry};

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
    /// Explicit opt-in to share this memory with the team via the
    /// git-backed `.kode/memory/team.jsonl` file, in addition to the normal
    /// (personal) Ingat write. Defaults to `false` — never inferred.
    #[serde(default)]
    team: bool,
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
         guesses, hypotheses, or anything you are not confident about. Set \
         `team: true` to also share it with the whole team via the git-backed \
         `.kode/memory/team.jsonl` file (visible to everyone with repo access) — \
         only do this when the user explicitly asked to share it."
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
                "symbols": { "type": "array", "items": { "type": "string" } },
                "team": {
                    "type": "boolean",
                    "description": "Share with the whole team (git-backed team.jsonl), not just this machine. Defaults to false."
                }
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
        ctx: &ToolContext,
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
            team: args.team,
        };

        let id = self
            .memory
            .remember(&new_memory)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;

        let mut content = format!("remembered ({}): {id}", new_memory.kind.as_kebab());
        if new_memory.team {
            let entry = WireEntry::new(&new_memory, None);
            let path = wire::team_file_path(&ctx.workspace_root);
            wire::append_entry(&path, &entry)
                .map_err(|e| ToolError::Failed(format!("team memory write failed: {e}")))?;
            content.push_str(&format!(" (shared: {})", path.display()));
        }

        Ok(ToolOutput { content })
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

    /// A fresh, unique workspace root for tests that write files (the
    /// shared `ctx()` root above is fine for tests that only exercise
    /// `MockEngineeringMemory`, which never touches disk).
    fn temp_workspace(label: &str) -> ToolContext {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-memory-tool-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        ToolContext {
            workspace_root: dir,
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

    #[tokio::test]
    async fn team_true_defaults_to_false_when_omitted() {
        let mock = Arc::new(MockEngineeringMemory::default());
        let tool = RememberTool::new(mock.clone(), None);

        tool.execute(
            serde_json::json!({
                "summary": "We use cargo-nextest for all test runs",
                "kind": "project-rule",
            }),
            &ctx(),
        )
        .await
        .unwrap();

        let recorded = mock.remembered_snapshot().await;
        assert!(!recorded[0].team);
    }

    #[tokio::test]
    async fn team_true_writes_to_ingat_and_appends_team_jsonl() {
        let mock = Arc::new(MockEngineeringMemory::default());
        let tool = RememberTool::new(mock.clone(), Some("kode".to_string()));
        let ctx = temp_workspace("team-dual-write");

        let out = tool
            .execute(
                serde_json::json!({
                    "summary": "always squash-merge feature branches",
                    "kind": "convention",
                    "team": true,
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(out.content.contains("shared:"));

        let recorded = mock.remembered_snapshot().await;
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].team);

        let path = crate::wire::team_file_path(&ctx.workspace_root);
        let (entries, corrupt) = crate::wire::read_entries(&path);
        assert_eq!(corrupt, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "always squash-merge feature branches");
        assert_eq!(entries[0].kind, "convention");

        let _ = std::fs::remove_dir_all(&ctx.workspace_root);
    }
}
