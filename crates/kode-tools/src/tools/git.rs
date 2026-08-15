use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

const GIT_TIMEOUT_SECS: u64 = 60;

async fn run_git(root: &Path, args: &[&str]) -> Result<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn()?;
    let output = tokio::time::timeout(
        Duration::from_secs(GIT_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| ToolError::Timeout(Duration::from_secs(GIT_TIMEOUT_SECS)))??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(ToolError::Failed(format!("git {args:?} failed: {stderr}")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub struct GitStatus;

#[async_trait::async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show the working tree status (git status --porcelain=v1 --branch)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(&self, _args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let stdout = run_git(
            &ctx.workspace_root,
            &["status", "--porcelain=v1", "--branch"],
        )
        .await?;
        let content = if stdout.trim().is_empty() {
            "clean".to_string()
        } else {
            stdout
        };
        Ok(ToolOutput { content })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitDiffArgs {
    staged: Option<bool>,
}

pub struct GitDiff;

#[async_trait::async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show the working tree diff (git diff, or git diff --cached if staged)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "staged": { "type": "boolean" }
            }
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: GitDiffArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: e.to_string(),
            })?;

        let diff_args: &[&str] = if args.staged.unwrap_or(false) {
            &["diff", "--cached"]
        } else {
            &["diff"]
        };

        let stdout = run_git(&ctx.workspace_root, diff_args).await?;
        let content = if stdout.trim().is_empty() {
            "no diff".to_string()
        } else {
            stdout
        };
        Ok(ToolOutput { content })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_repo() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("kode-tools-git-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();

        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap();
            assert!(status.success());
        };

        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);

        std::fs::write(dir.join("tracked.txt"), "line1\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-q", "-m", "init"]);

        dir
    }

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            workspace_root: root,
            cancel: kode_core::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn status_shows_new_file() {
        let dir = temp_repo();
        std::fs::write(dir.join("new.txt"), "hi").unwrap();
        let tool = GitStatus;
        let out = tool
            .execute(serde_json::json!({}), &ctx(dir))
            .await
            .unwrap();
        assert!(out.content.contains("new.txt"));
    }

    #[tokio::test]
    async fn diff_shows_unstaged_modification() {
        let dir = temp_repo();
        std::fs::write(dir.join("tracked.txt"), "line1\nline2\n").unwrap();
        let tool = GitDiff;
        let out = tool
            .execute(serde_json::json!({}), &ctx(dir))
            .await
            .unwrap();
        assert!(out.content.contains("tracked.txt"));
        assert!(out.content.contains("line2"));
    }
}
