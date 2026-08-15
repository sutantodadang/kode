use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::path::resolve_in_workspace;
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

const MAX_UNBOUNDED_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the workspace, optionally by line range."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer", "description": "1-based starting line" },
                "limit": { "type": "integer", "description": "number of lines to read" }
            },
            "required": ["path"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: Args = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: e.to_string(),
        })?;

        let resolved = resolve_in_workspace(&ctx.workspace_root, &args.path)?;

        if args.offset.is_none() && args.limit.is_none() {
            let meta = tokio::fs::metadata(&resolved).await?;
            if meta.len() > MAX_UNBOUNDED_BYTES {
                return Err(ToolError::Failed(
                    "file too large, use offset/limit".to_string(),
                ));
            }
        }

        let content = tokio::fs::read_to_string(&resolved).await?;

        let content = if args.offset.is_some() || args.limit.is_some() {
            let offset = args.offset.unwrap_or(1).max(1);
            let limit = args.limit.unwrap_or(usize::MAX);
            content
                .lines()
                .skip(offset - 1)
                .take(limit)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            content
        };

        tracing::debug!(path = %resolved.display(), "read_file executed");
        Ok(ToolOutput { content })
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
            std::env::temp_dir().join(format!("kode-tools-read-{}-{}", std::process::id(), n));
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
    async fn roundtrip() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "hello world").unwrap();
        let tool = ReadFile;
        let out = tool
            .execute(serde_json::json!({"path": "a.txt"}), &ctx(dir))
            .await
            .unwrap();
        assert_eq!(out.content, "hello world");
    }

    #[tokio::test]
    async fn offset_limit_slice() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "l1\nl2\nl3\nl4\n").unwrap();
        let tool = ReadFile;
        let out = tool
            .execute(
                serde_json::json!({"path": "a.txt", "offset": 2, "limit": 2}),
                &ctx(dir),
            )
            .await
            .unwrap();
        assert_eq!(out.content, "l2\nl3");
    }

    #[tokio::test]
    async fn traversal_rejected() {
        let dir = temp_dir();
        let tool = ReadFile;
        let err = tool
            .execute(serde_json::json!({"path": "../../etc/passwd"}), &ctx(dir))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }
}
