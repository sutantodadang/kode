use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::path::resolve_in_workspace;
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_CHARS: usize = 50_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    program: String,
    args: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    cwd: Option<String>,
}

pub struct RunCommand;

fn truncate(s: String) -> String {
    if s.chars().count() > MAX_OUTPUT_CHARS {
        let mut truncated: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        truncated.push_str("\n[truncated]");
        truncated
    } else {
        s
    }
}

#[async_trait::async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "Run a program with arguments in the workspace (no shell)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "program": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "timeout_secs": { "type": "integer" },
                "cwd": { "type": "string" }
            },
            "required": ["program"]
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

        let cwd = match &args.cwd {
            Some(c) => resolve_in_workspace(&ctx.workspace_root, c)?,
            None => ctx.workspace_root.clone(),
        };

        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let mut command = tokio::process::Command::new(&args.program);
        command
            .args(args.args.clone().unwrap_or_default())
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = command.spawn()?;

        tokio::select! {
            result = child.wait_with_output() => {
                let output = result?;
                let stdout = truncate(String::from_utf8_lossy(&output.stdout).into_owned());
                let stderr = truncate(String::from_utf8_lossy(&output.stderr).into_owned());
                let exit_code = output.status.code().unwrap_or(-1);
                tracing::debug!(program = %args.program, exit_code, "run_command executed");
                Ok(ToolOutput {
                    content: format!(
                        "exit code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                    ),
                })
            }
            _ = tokio::time::sleep(timeout) => {
                Err(ToolError::Timeout(timeout))
            }
            _ = ctx.cancel.cancelled() => {
                Err(ToolError::Cancelled)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            cancel: kode_core::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn git_version_succeeds() {
        let tool = RunCommand;
        let out = tool
            .execute(
                serde_json::json!({"program": "git", "args": ["--version"]}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("exit code: 0"));
        assert!(out.content.contains("git version"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_triggers() {
        let tool = RunCommand;
        let err = tool
            .execute(
                serde_json::json!({
                    "program": "ping",
                    "args": ["-n", "10", "127.0.0.1"],
                    "timeout_secs": 1
                }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_triggers() {
        let tool = RunCommand;
        let err = tool
            .execute(
                serde_json::json!({
                    "program": "sleep",
                    "args": ["10"],
                    "timeout_secs": 1
                }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }
}
