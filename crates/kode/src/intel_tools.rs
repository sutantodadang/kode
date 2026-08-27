use std::sync::Arc;

use kode_intel::CodeIntelligence;
use kode_tools::path::resolve_in_workspace;
use kode_tools::{RequiredPermission, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;

pub(crate) struct CodeSearchTool {
    intel: Arc<dyn CodeIntelligence>,
}

impl CodeSearchTool {
    pub(crate) fn new(intel: Arc<dyn CodeIntelligence>) -> Self {
        Self { intel }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<u32>,
}

#[async_trait::async_trait]
impl Tool for CodeSearchTool {
    fn name(&self) -> &str {
        "code_search"
    }

    fn description(&self) -> &str {
        "Search the indexed repository through the zindeks code graph. Prefer this for symbols, concepts, implementations, and call-site discovery. Use git grep only for exact literal matching or if this tool is unavailable."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Symbol, concept, behavior, or code relationship to find"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "default": 8
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> kode_tools::Result<ToolOutput> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: error.to_string(),
            })?;
        if args.query.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "query must not be empty".to_string(),
            });
        }

        let rows = self
            .intel
            .search(args.query.trim(), args.limit.unwrap_or(8).clamp(1, 20))
            .await
            .map_err(|error| ToolError::Failed(format!("zindeks search failed: {error}")))?;
        let matches = rows
            .into_iter()
            .map(|row| {
                serde_json::json!({
                    "path": row.path,
                    "score": row.score,
                    "snippet": row.snippet,
                })
            })
            .collect::<Vec<_>>();
        let content = serde_json::to_string_pretty(&serde_json::json!({
            "query": args.query.trim(),
            "matches": matches,
        }))
        .map_err(|error| ToolError::Failed(error.to_string()))?;
        Ok(ToolOutput { content })
    }
}

pub(crate) struct FileOutlineTool {
    intel: Arc<dyn CodeIntelligence>,
}

impl FileOutlineTool {
    pub(crate) fn new(intel: Arc<dyn CodeIntelligence>) -> Self {
        Self { intel }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutlineArgs {
    path: String,
}

#[async_trait::async_trait]
impl Tool for FileOutlineTool {
    fn name(&self) -> &str {
        "file_outline"
    }

    fn description(&self) -> &str {
        "Return the zindeks symbol outline for one repository file. Use before reading an entire file when its structure or symbol locations are what you need."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> kode_tools::Result<ToolOutput> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let args: OutlineArgs =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: error.to_string(),
            })?;
        let resolved = resolve_in_workspace(&ctx.workspace_root, &args.path)?;
        let relative = resolved
            .strip_prefix(&ctx.workspace_root)
            .unwrap_or(&resolved)
            .to_string_lossy()
            .replace('\\', "/");

        let outline = self
            .intel
            .file_outline(&relative)
            .await
            .map_err(|error| ToolError::Failed(format!("zindeks outline failed: {error}")))?;
        let symbols = outline
            .symbols
            .into_iter()
            .map(|symbol| {
                serde_json::json!({
                    "name": symbol.name,
                    "kind": symbol.kind,
                    "line": symbol.line,
                    "line_end": symbol.line_end,
                })
            })
            .collect::<Vec<_>>();
        let content = serde_json::to_string_pretty(&serde_json::json!({
            "path": outline.path,
            "symbols": symbols,
        }))
        .map_err(|error| ToolError::Failed(error.to_string()))?;
        Ok(ToolOutput { content })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_intel::{CodeSearchResult, FileOutline, MockCodeIntelligence, OutlineSymbol};

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            workspace_root: root,
            cancel: kode_core::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn search_returns_structured_ranked_matches() {
        let intel = MockCodeIntelligence {
            search_results: vec![CodeSearchResult {
                path: "src/main.rs".to_string(),
                snippet: "fn main()".to_string(),
                score: 0.91,
            }],
            ..Default::default()
        };
        let tool = CodeSearchTool::new(Arc::new(intel));
        let output = tool
            .execute(
                serde_json::json!({"query": "application entry point", "limit": 5}),
                &ctx(std::env::temp_dir()),
            )
            .await
            .unwrap();

        assert!(output.content.contains("src/main.rs"));
        assert!(output.content.contains("fn main()"));
        assert!(output.content.contains("0.91"));
    }

    #[tokio::test]
    async fn outline_uses_workspace_relative_safe_path() {
        let root = std::env::temp_dir().join(format!(
            "kode-intel-tool-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn run() {}\n").unwrap();
        let intel = MockCodeIntelligence {
            outline: FileOutline {
                path: "src/lib.rs".to_string(),
                symbols: vec![OutlineSymbol {
                    name: "run".to_string(),
                    kind: "function".to_string(),
                    line: 1,
                    line_end: 1,
                }],
            },
            ..Default::default()
        };
        let tool = FileOutlineTool::new(Arc::new(intel));
        let output = tool
            .execute(
                serde_json::json!({"path": "src/lib.rs"}),
                &ctx(root.clone()),
            )
            .await
            .unwrap();

        assert!(output.content.contains("src/lib.rs"));
        assert!(output.content.contains("run"));
        let error = tool
            .execute(
                serde_json::json!({"path": "../../outside.rs"}),
                &ctx(root.clone()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::PathOutsideWorkspace(_)));
        std::fs::remove_dir_all(root).unwrap();
    }
}
