use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::path::resolve_in_workspace;
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

pub struct ApplyPatch;

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Replace an exact substring in a file (old_string -> new_string)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean" }
            },
            "required": ["path", "old_string", "new_string"]
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

        if args.old_string.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "old_string must not be empty".to_string(),
            });
        }

        let resolved = resolve_in_workspace(&ctx.workspace_root, &args.path)?;
        let original = tokio::fs::read_to_string(&resolved).await?;

        let replace_all = args.replace_all.unwrap_or(false);
        let match_count = original.matches(args.old_string.as_str()).count();

        let (updated, replaced) = if replace_all {
            if match_count == 0 {
                return Err(ToolError::Failed("old_string not found".to_string()));
            }
            (
                original.replace(&args.old_string, &args.new_string),
                match_count,
            )
        } else {
            match match_count {
                0 => return Err(ToolError::Failed("old_string not found".to_string())),
                1 => (original.replacen(&args.old_string, &args.new_string, 1), 1),
                n => {
                    return Err(ToolError::Failed(format!(
                        "old_string is not unique; {n} matches"
                    )));
                }
            }
        };

        tokio::fs::write(&resolved, &updated).await?;

        tracing::debug!(path = %resolved.display(), replaced, "apply_patch executed");
        Ok(ToolOutput {
            content: format!(
                "applied {replaced} replacement(s) to {}",
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
            std::env::temp_dir().join(format!("kode-tools-patch-{}-{}", std::process::id(), n));
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
    async fn unique_replace_works() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo bar baz").unwrap();
        let tool = ApplyPatch;
        let out = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "bar", "new_string": "qux"}),
                &ctx(dir.clone()),
            )
            .await
            .unwrap();
        assert!(out.content.contains("applied 1"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "foo qux baz"
        );
    }

    #[tokio::test]
    async fn not_found_fails() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo bar baz").unwrap();
        let tool = ApplyPatch;
        let err = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "nope", "new_string": "x"}),
                &ctx(dir),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
    }

    #[tokio::test]
    async fn duplicate_without_replace_all_fails() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo foo").unwrap();
        let tool = ApplyPatch;
        let err = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "bar"}),
                &ctx(dir),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
    }

    #[tokio::test]
    async fn replace_all_replaces_both() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo foo").unwrap();
        let tool = ApplyPatch;
        let out = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "bar", "replace_all": true}),
                &ctx(dir.clone()),
            )
            .await
            .unwrap();
        assert!(out.content.contains("applied 2"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "bar bar"
        );
    }
}
