use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::{Result, ToolError};
use crate::path::resolve_in_workspace;
use crate::proc::{scrub_env, spawn_managed};
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_CHARS: usize = 50_000;
const MAX_CAPTURE_BYTES: usize = MAX_OUTPUT_CHARS * 4;
const OUTPUT_TRUNCATED_MARKER: &str = "\n[truncated]";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    program: String,
    args: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    cwd: Option<String>,
}

pub struct RunCommand;

async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    max_bytes: usize,
) -> std::io::Result<String> {
    let mut captured = Vec::with_capacity(max_bytes.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        let remaining = max_bytes.saturating_sub(captured.len());
        if remaining > 0 {
            captured.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }

    let mut output = String::from_utf8_lossy(&captured).into_owned();
    output = truncate_chars(output);
    if truncated && !output.ends_with(OUTPUT_TRUNCATED_MARKER) {
        output.push_str(OUTPUT_TRUNCATED_MARKER);
    }
    Ok(output)
}

fn truncate_chars(s: String) -> String {
    if s.chars().count() > MAX_OUTPUT_CHARS {
        let mut truncated: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        truncated.push_str(OUTPUT_TRUNCATED_MARKER);
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

        // Friendly: `program: "cargo test"` with no `args` is split on
        // whitespace instead of failing with "program not found".
        let (program, extra_args): (String, Vec<String>) = match &args.args {
            None if args.program.contains(char::is_whitespace) => {
                let mut parts = args.program.split_whitespace().map(str::to_string);
                let p = parts.next().unwrap_or_default();
                (p, parts.collect())
            }
            _ => (args.program.clone(), args.args.clone().unwrap_or_default()),
        };

        let mut command = tokio::process::Command::new(&program);
        command
            .args(&extra_args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_env(&mut command);

        let managed = spawn_managed(&mut command).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::Failed(format!(
                    "program not found: '{program}' — run_command uses no shell: pass the executable in `program` and its arguments in `args` (shell builtins, pipes and redirects are not available)"
                ))
            } else {
                ToolError::Io(e)
            }
        })?;
        let (mut child, mut tree) = managed.into_parts();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("child stderr was not piped"))?;
        let stdout_task = tokio::spawn(read_bounded(stdout, MAX_CAPTURE_BYTES));
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_CAPTURE_BYTES));

        tokio::select! {
            result = child.wait() => {
                let status = result?;
                let stdout = stdout_task.await.map_err(std::io::Error::other)??;
                let stderr = stderr_task.await.map_err(std::io::Error::other)??;
                let exit_code = status.code().unwrap_or(-1);
                tracing::debug!(program = %program, exit_code, "run_command executed");
                Ok(ToolOutput {
                    content: format!(
                        "exit code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                    ),
                })
            }
            _ = tokio::time::sleep(timeout) => {
                tree.kill_tree();
                stdout_task.abort();
                stderr_task.abort();
                Err(ToolError::Timeout(timeout))
            }
            _ = ctx.cancel.cancelled() => {
                tree.kill_tree();
                stdout_task.abort();
                stderr_task.abort();
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

    #[cfg(windows)]
    #[tokio::test]
    async fn credential_env_vars_are_scrubbed() {
        // The guard is scoped tightly around each env mutation and dropped
        // before the `.await` below — clippy (rightly) flags a std Mutex
        // guard held across an await point.
        {
            let _guard = crate::test_support::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // SAFETY: test-only; serialized via ENV_LOCK.
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", "super-secret");
            }
        }

        let tool = RunCommand;
        let out = tool
            .execute(
                serde_json::json!({
                    "program": "cmd",
                    "args": ["/C", "echo %ANTHROPIC_API_KEY%"]
                }),
                &ctx(),
            )
            .await
            .unwrap();

        {
            let _guard = crate::test_support::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // SAFETY: test-only; serialized via ENV_LOCK.
            unsafe {
                std::env::remove_var("ANTHROPIC_API_KEY");
            }
        }

        assert!(!out.content.contains("super-secret"));
        // Unexpanded on Windows cmd.exe when the var isn't set in the child.
        assert!(out.content.contains("%ANTHROPIC_API_KEY%"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn credential_env_vars_are_scrubbed() {
        // See the windows variant above for why the guard is scoped tightly
        // around each env mutation rather than held across the `.await`.
        {
            let _guard = crate::test_support::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // SAFETY: test-only; serialized via ENV_LOCK.
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", "super-secret");
            }
        }

        let tool = RunCommand;
        let out = tool
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo ${ANTHROPIC_API_KEY:-unset}"]
                }),
                &ctx(),
            )
            .await
            .unwrap();

        {
            let _guard = crate::test_support::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // SAFETY: test-only; serialized via ENV_LOCK.
            unsafe {
                std::env::remove_var("ANTHROPIC_API_KEY");
            }
        }

        assert!(!out.content.contains("super-secret"));
        assert!(out.content.contains("unset"));
    }
}
