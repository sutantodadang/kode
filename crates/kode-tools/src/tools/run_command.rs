use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::{Result, ToolError};
use crate::output::clip;
use crate::path::resolve_in_workspace;
use crate::proc::{scrub_env, spawn_managed};
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Bytes kept from the start and from the end of each stream while the
/// child runs. The middle of a very long stream is dropped as it arrives, so
/// memory stays bounded however much the program prints.
const CAPTURE_HEAD_BYTES: usize = 16_000;
const CAPTURE_TAIL_BYTES: usize = 16_000;
/// Share of the tool result given to each of stdout and stderr.
const STREAM_OUTPUT_BYTES: usize = 3_600;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    program: String,
    args: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    cwd: Option<String>,
}

pub struct RunCommand;

/// Drains `reader` to the end, keeping its first [`CAPTURE_HEAD_BYTES`] and
/// last [`CAPTURE_TAIL_BYTES`], then shapes the text to
/// [`STREAM_OUTPUT_BYTES`]. Failure summaries sit at the end of long output,
/// so the tail matters as much as the head.
async fn read_bounded<R: AsyncRead + Unpin>(mut reader: R) -> std::io::Result<String> {
    let mut head: Vec<u8> = Vec::with_capacity(8192);
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut dropped = 0_usize;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        let to_head = (CAPTURE_HEAD_BYTES - head.len()).min(chunk.len());
        head.extend_from_slice(&chunk[..to_head]);
        tail.extend(&chunk[to_head..]);
        if tail.len() > CAPTURE_TAIL_BYTES {
            let excess = tail.len() - CAPTURE_TAIL_BYTES;
            tail.drain(..excess);
            dropped += excess;
        }
    }

    let tail: Vec<u8> = tail.into();
    let text = if dropped == 0 {
        // Contiguous bytes: decode once so a character split between the two
        // buffers is not damaged.
        head.extend_from_slice(&tail);
        String::from_utf8_lossy(&head).into_owned()
    } else {
        format!(
            "{}\n[... {dropped} bytes not captured ...]\n{}",
            String::from_utf8_lossy(&head),
            String::from_utf8_lossy(&tail)
        )
    };
    Ok(clip(&text, STREAM_OUTPUT_BYTES))
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
                let hint = match program.as_str() {
                    "rg" | "grep" | "ag" | "ack" | "findstr" => {
                        " Use the `code_search` tool when offered; otherwise use program `git` with args [\"grep\", \"-n\", \"<exact-pattern>\"]."
                    }
                    "cat" | "head" | "tail" | "less" | "more" | "type" => {
                        " Use the `read_file` tool instead."
                    }
                    "find" | "ls" | "dir" | "tree" => {
                        " To list tracked files use program `git` with args [\"ls-files\"]."
                    }
                    _ => "",
                };
                ToolError::Failed(format!(
                    "program not found: '{program}' — run_command uses no shell: pass the executable in `program` and its arguments in `args` (shell builtins, pipes and redirects are not available).{hint}"
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
        let mut stdout_task = tokio::spawn(read_bounded(stdout));
        let mut stderr_task = tokio::spawn(read_bounded(stderr));

        // Race the child's exit AND the output drain together. A detached
        // grandchild (e.g. `cmd /c start /b server.exe`) exits its parent
        // immediately while inheriting the pipe handles, so the drain can
        // block forever; it must stay under the timeout and cancel guards,
        // not just the wait.
        enum Outcome {
            Done(std::io::Result<(std::process::ExitStatus, String, String)>),
            TimedOut,
            Cancelled,
        }
        let combined = async {
            let status = child.wait().await?;
            let stdout = (&mut stdout_task).await.map_err(std::io::Error::other)??;
            let stderr = (&mut stderr_task).await.map_err(std::io::Error::other)??;
            Ok((status, stdout, stderr))
        };
        let outcome = tokio::select! {
            result = combined => Outcome::Done(result),
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            _ = ctx.cancel.cancelled() => Outcome::Cancelled,
        };
        match outcome {
            Outcome::Done(Ok((status, stdout, stderr))) => {
                let exit_code = status.code().unwrap_or(-1);
                tracing::debug!(program = %program, exit_code, "run_command executed");
                Ok(ToolOutput {
                    content: format!(
                        "exit code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                    ),
                })
            }
            Outcome::Done(Err(e)) => Err(ToolError::Io(e)),
            Outcome::TimedOut => {
                tree.kill_tree();
                stdout_task.abort();
                stderr_task.abort();
                Err(ToolError::Timeout(timeout))
            }
            Outcome::Cancelled => {
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
    async fn short_stream_is_captured_exactly() {
        let captured = read_bounded(&b"hello\nworld\n"[..]).await.unwrap();
        assert_eq!(captured, "hello\nworld\n");
    }

    #[tokio::test]
    async fn long_stream_keeps_first_and_last_lines() {
        let text: String = (1..=50_000).map(|n| format!("line {n}\n")).collect();
        let captured = read_bounded(text.as_bytes()).await.unwrap();

        assert!(captured.len() <= STREAM_OUTPUT_BYTES, "{}", captured.len());
        assert!(captured.starts_with("line 1\n"));
        assert!(captured.ends_with("line 50000\n"));
        assert!(captured.contains("omitted"));
    }

    #[tokio::test]
    async fn multibyte_stream_stays_valid_at_the_seam() {
        let text = "é".repeat(100_000);
        let captured = read_bounded(text.as_bytes()).await.unwrap();

        assert!(captured.len() <= STREAM_OUTPUT_BYTES);
        assert!(captured.starts_with('é'));
        assert!(captured.ends_with('é'));
    }

    #[tokio::test]
    async fn stream_just_under_capture_size_has_no_seam_damage() {
        // Fits in head + tail capture. The leading ASCII byte shifts every
        // two-byte character so the head/tail split lands mid-character.
        let text = format!("a{}", "é".repeat((CAPTURE_HEAD_BYTES + 1_000) / 2));
        let captured = read_bounded(text.as_bytes()).await.unwrap();
        assert!(!captured.contains('\u{FFFD}'));
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

    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_covers_output_drain_held_by_detached_grandchild() {
        // Regression: `cmd /c start /b <server>` exits cmd immediately while
        // the grandchild inherits the stdout/stderr pipe handles. The drain
        // used to block forever AFTER the child exited — past the timeout
        // and cancel guards — leaving the tool uninterruptible.
        let tool = RunCommand;
        let started = std::time::Instant::now();
        let err = tool
            .execute(
                serde_json::json!({
                    "program": "cmd",
                    "args": ["/C", "start", "/b", "ping", "-n", "30", "127.0.0.1"],
                    "timeout_secs": 1
                }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
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
