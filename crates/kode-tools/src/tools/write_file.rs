use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::path::resolve_in_workspace;
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    content: String,
}

pub struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write (create or overwrite) a text file in the workspace."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::Mutating
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: Args = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: e.to_string(),
        })?;

        let resolved = resolve_in_workspace(&ctx.workspace_root, &args.path)?;

        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&resolved, &args.content).await?;

        tracing::debug!(path = %resolved.display(), "write_file executed");
        Ok(ToolOutput {
            content: format!(
                "wrote {} bytes to {}",
                args.content.len(),
                resolved.display()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("kode-tools-write-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            workspace_root: root,
            cancel: kode_core::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn creates_nested_dirs_and_file() {
        let dir = temp_dir();
        let runtime = crate::registry::ToolRuntime::builtin_runtime(
            kode_core::config::PermissionMode::Allow,
            std::sync::Arc::new(crate::permission::AutoApprove),
        );
        let out = runtime
            .execute(
                "write_file",
                serde_json::json!({"path": "nested/dir/file.txt", "content": "hi"}),
                &ctx(dir.clone()),
            )
            .await
            .unwrap();
        assert!(out.content.contains("wrote 2 bytes"));
        let written =
            std::fs::read_to_string(dir.join("nested").join("dir").join("file.txt")).unwrap();
        assert_eq!(written, "hi");
    }
}
