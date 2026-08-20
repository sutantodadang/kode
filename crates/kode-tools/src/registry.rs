use std::sync::Arc;

use kode_core::config::PermissionMode;
use kode_model::ToolSpec;

use crate::error::{Result, ToolError};
use crate::permission::{Decision, PermissionHandler, decide};
use crate::tools::{
    ApplyPatch, FetchUrl, GitDiff, GitStatus, ReadFile, RunCommand, WebSearch, WriteFile,
};
use crate::{Tool, ToolContext, ToolOutput};

pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ReadFile));
        registry.register(Arc::new(WriteFile));
        registry.register(Arc::new(ApplyPatch));
        registry.register(Arc::new(RunCommand));
        registry.register(Arc::new(GitStatus));
        registry.register(Arc::new(GitDiff));
        registry.register(Arc::new(FetchUrl));
        registry.register(Arc::new(WebSearch));
        registry
    }
}

pub struct ToolRuntime {
    registry: ToolRegistry,
    mode: PermissionMode,
    handler: Arc<dyn PermissionHandler>,
}

impl ToolRuntime {
    pub fn new(
        registry: ToolRegistry,
        mode: PermissionMode,
        handler: Arc<dyn PermissionHandler>,
    ) -> Self {
        Self {
            registry,
            mode,
            handler,
        }
    }

    pub fn builtin_runtime(mode: PermissionMode, handler: Arc<dyn PermissionHandler>) -> Self {
        Self::new(ToolRegistry::with_builtins(), mode, handler)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.registry.specs()
    }

    pub fn required_permission(&self, name: &str) -> Option<crate::RequiredPermission> {
        self.registry.get(name).map(|t| t.required_permission())
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;

        match decide(self.mode, tool.required_permission()) {
            Decision::Allow => {}
            Decision::Deny => {
                return Err(ToolError::Denied(format!(
                    "{name} denied by permission mode"
                )));
            }
            Decision::Ask => {
                let summary = format!("{name} {args}");
                if !self.handler.confirm(&summary).await {
                    return Err(ToolError::Denied(format!("{name} denied by user")));
                }
            }
        }

        tool.execute(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{AutoApprove, AutoDeny};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            cancel: kode_core::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let runtime = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let err = runtime
            .execute("does_not_exist", serde_json::json!({}), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)));
    }

    #[test]
    fn specs_returns_all_builtins() {
        let registry = ToolRegistry::with_builtins();
        let specs = registry.specs();
        assert_eq!(specs.len(), 8);
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "read_file",
            "write_file",
            "apply_patch",
            "run_command",
            "git_status",
            "git_diff",
            "fetch_url",
            "web_search",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn mutating_with_auto_deny_is_denied() {
        let runtime = ToolRuntime::builtin_runtime(PermissionMode::Ask, Arc::new(AutoDeny));
        let err = runtime
            .execute(
                "write_file",
                serde_json::json!({"path": "x.txt", "content": "y"}),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)));
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[tokio::test]
    async fn mutating_with_auto_approve_executes() {
        let dir = std::env::temp_dir().join(format!(
            "kode-tools-registry-{}-{}",
            std::process::id(),
            nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = ToolRuntime::builtin_runtime(PermissionMode::Ask, Arc::new(AutoApprove));
        let out = runtime
            .execute(
                "write_file",
                serde_json::json!({"path": "x.txt", "content": "y"}),
                &ToolContext {
                    workspace_root: dir,
                    cancel: kode_core::CancellationToken::new(),
                },
            )
            .await
            .unwrap();
        assert!(out.content.contains("wrote"));
    }

    struct CountingHandler {
        called: AtomicBool,
    }

    #[async_trait::async_trait]
    impl PermissionHandler for CountingHandler {
        async fn confirm(&self, _summary: &str) -> bool {
            self.called.store(true, Ordering::SeqCst);
            true
        }
    }

    #[tokio::test]
    async fn read_only_never_prompts() {
        let handler = Arc::new(CountingHandler {
            called: AtomicBool::new(false),
        });
        let runtime = ToolRuntime::builtin_runtime(PermissionMode::Deny, handler.clone());
        // git_status is ReadOnly; even with Deny mode it should execute without confirm.
        let dir = std::env::temp_dir();
        let _ = runtime
            .execute(
                "git_status",
                serde_json::json!({}),
                &ToolContext {
                    workspace_root: dir,
                    cancel: kode_core::CancellationToken::new(),
                },
            )
            .await;
        assert!(!handler.called.load(Ordering::SeqCst));
    }
}
