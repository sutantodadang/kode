use std::sync::Arc;

use kode_core::config::PermissionMode;
use kode_model::ToolSpec;

use crate::error::{Result, ToolError};
use crate::path::WriteScope;
use crate::permission::{Decision, PermissionHandler, decide};
use crate::tools::{
    ApplyPatch, FetchUrl, GitDiff, GitStatus, ReadFile, RunCommand, WebSearch, WriteFile,
};
use crate::{Tool, ToolContext, ToolOutput};

#[derive(Clone)]
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

    /// Replaces the tool with the same name in place (keeping the order
    /// the model sees, which keeps prompt caches warm), or appends it.
    pub fn replace(&mut self, tool: Arc<dyn Tool>) {
        match self.tools.iter_mut().find(|t| t.name() == tool.name()) {
            Some(slot) => *slot = tool,
            None => self.tools.push(tool),
        }
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

    /// Child-safe builtins: no arbitrary command execution. Mutations are
    /// further constrained by [`WriteScope`] in [`ToolRuntime`].
    pub fn with_subagent_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ReadFile));
        registry.register(Arc::new(WriteFile));
        registry.register(Arc::new(ApplyPatch));
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
    write_scope: Option<WriteScope>,
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
            write_scope: None,
        }
    }

    pub fn with_write_scope(mut self, write_scope: WriteScope) -> Self {
        self.write_scope = Some(write_scope);
        self
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

        if tool.required_permission() == crate::RequiredPermission::Mutating
            && let Some(scope) = &self.write_scope
        {
            if !matches!(name, "write_file" | "apply_patch") {
                return Err(ToolError::Denied(format!(
                    "{name} is not available inside a scoped subagent"
                )));
            }
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs {
                    tool: name.to_string(),
                    message: "path must be a string".to_string(),
                })?;
            scope.ensure_contains(&ctx.workspace_root, path)?;
        }

        match decide(self.mode, tool.required_permission()) {
            Decision::Allow => {}
            Decision::Deny => {
                return Err(ToolError::Denied(format!(
                    "{name} denied by permission mode"
                )));
            }
            Decision::Ask => {
                let mut summary = format!("{name} {args}");
                if let Some(note) = tool.permission_note(&args, ctx).await {
                    summary.push('\n');
                    summary.push_str(&note);
                }
                if !self.handler.confirm(&summary).await {
                    return Err(ToolError::Denied(format!("{name} denied by user")));
                }
            }
        }

        tool.execute(args, ctx).await
    }

    /// Executes a tool and returns its dynamic mutation effect. This differs
    /// from [`Self::required_permission`] for composite tools such as native
    /// delegation, whose read-only and writable invocations share one schema.
    pub async fn execute_with_effect(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<(ToolOutput, bool)> {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        let mut output = self.execute(name, args, ctx).await?;
        // Read the flag from the full result: composite tools encode it in
        // content that clipping may remove.
        let mutated = tool.output_mutated(&output);
        output.content = crate::output::clip(&output.content, crate::output::MAX_TOOL_OUTPUT_BYTES);
        Ok((output, mutated))
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

    struct Noted;

    #[async_trait::async_trait]
    impl Tool for Noted {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "noted"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn required_permission(&self) -> crate::RequiredPermission {
            crate::RequiredPermission::Mutating
        }
        async fn permission_note(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Option<String> {
            Some("impact: f ← 2 callers".into())
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: "ok".into(),
            })
        }
    }

    struct RecordingHandler(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl PermissionHandler for RecordingHandler {
        async fn confirm(&self, summary: &str) -> bool {
            self.0.lock().unwrap().push(summary.to_string());
            true
        }
    }

    #[test]
    fn replace_keeps_position_and_swaps_the_tool() {
        let mut registry = ToolRegistry::with_builtins();
        let before: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        registry.replace(Arc::new(Noted));
        let after: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(before, after);
        assert_eq!(registry.get("write_file").unwrap().description(), "noted");
    }

    #[tokio::test]
    async fn permission_note_is_added_to_the_prompt() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Noted));
        let handler = Arc::new(RecordingHandler(std::sync::Mutex::new(vec![])));
        let runtime = ToolRuntime::new(registry, PermissionMode::Ask, handler.clone());
        runtime
            .execute("write_file", serde_json::json!({"path": "a"}), &ctx())
            .await
            .unwrap();
        let prompts = handler.0.lock().unwrap();
        assert!(prompts[0].ends_with("\nimpact: f ← 2 callers"));
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

    #[test]
    fn subagent_builtins_exclude_arbitrary_command_execution() {
        let names = ToolRegistry::with_subagent_builtins()
            .specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(!names.contains(&"run_command".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"apply_patch".to_string()));
    }

    #[tokio::test]
    async fn scoped_runtime_rejects_write_outside_ownership() {
        let dir = std::env::temp_dir().join(format!(
            "kode-tools-scope-{}-{}",
            std::process::id(),
            nanos()
        ));
        std::fs::create_dir_all(dir.join("owned")).unwrap();
        let scope = WriteScope::new(&dir, &["owned".to_string()]).unwrap();
        let runtime = ToolRuntime::new(
            ToolRegistry::with_subagent_builtins(),
            PermissionMode::Allow,
            Arc::new(AutoApprove),
        )
        .with_write_scope(scope);

        let error = runtime
            .execute(
                "write_file",
                serde_json::json!({"path": "elsewhere.txt", "content": "no"}),
                &ToolContext {
                    workspace_root: dir,
                    cancel: kode_core::CancellationToken::new(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Denied(_)));
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

    /// Returns a large result whose mutation flag sits in the middle, where
    /// clipping removes it.
    struct BigResult;

    #[async_trait::async_trait]
    impl Tool for BigResult {
        fn name(&self) -> &str {
            "big_result"
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn required_permission(&self) -> crate::RequiredPermission {
            crate::RequiredPermission::ReadOnly
        }

        fn output_mutated(&self, output: &ToolOutput) -> bool {
            output.content.contains("MUTATED_FLAG")
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: format!(
                    "{}MUTATED_FLAG\n{}",
                    "head line\n".repeat(3_000),
                    "tail line\n".repeat(3_000)
                ),
            })
        }
    }

    #[tokio::test]
    async fn runtime_clips_results_after_reading_the_mutation_flag() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(BigResult));
        let runtime = ToolRuntime::new(registry, PermissionMode::Allow, Arc::new(AutoApprove));

        let (output, mutated) = runtime
            .execute_with_effect("big_result", serde_json::json!({}), &ctx())
            .await
            .unwrap();

        assert!(mutated, "flag must be read from the full output");
        assert!(output.content.len() <= crate::output::MAX_TOOL_OUTPUT_BYTES);
        assert!(!output.content.contains("MUTATED_FLAG"));
        assert!(output.content.starts_with("head line\n"));
        assert!(output.content.ends_with("tail line\n"));
        assert!(output.content.contains("omitted"));
    }
}
