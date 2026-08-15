use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const DIFF_TRUNCATE_CHARS: usize = 24_000;
const DIFF_TRUNCATE_SUFFIX: &str = "\n[diff truncated]";

/// Snapshot of the working tree's uncommitted state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GitState {
    pub status: String,
    pub diff: String,
}

/// Captures `git status` + `git diff` (falling back to `git diff --cached`
/// when the unstaged diff is empty) for `root`.
///
/// Any failure — git missing, `root` not a repo, non-zero exit, timeout —
/// yields `None`. A clean tree (no status, no diff) yields
/// `Some(GitState::default())`; callers decide whether to render that.
pub async fn git_state(root: &Path) -> Option<GitState> {
    let status = run_git(root, &["status", "--porcelain=v1"]).await?;
    let mut diff = run_git(root, &["diff"]).await?;
    if diff.trim().is_empty() {
        diff = run_git(root, &["diff", "--cached"]).await?;
    }

    Some(GitState {
        status: status.trim_end().to_string(),
        diff: truncate_diff(diff.trim_end()),
    })
}

fn truncate_diff(diff: &str) -> String {
    if diff.chars().count() <= DIFF_TRUNCATE_CHARS {
        return diff.to_string();
    }
    let mut truncated: String = diff.chars().take(DIFF_TRUNCATE_CHARS).collect();
    truncated.push_str(DIFF_TRUNCATE_SUFFIX);
    truncated
}

async fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn().ok()?;
    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-context-git-{label}-{}-{}-{}",
            std::process::id(),
            nanos(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
    }

    #[tokio::test]
    async fn non_repo_returns_none() {
        let dir = temp_dir("non-repo");
        assert!(git_state(&dir).await.is_none());
    }

    #[tokio::test]
    async fn untracked_file_shows_in_status() {
        let dir = temp_dir("untracked");
        init_repo(&dir);
        std::fs::write(dir.join("new.txt"), "hi").unwrap();

        let state = git_state(&dir).await.unwrap();
        assert!(state.status.contains("new.txt"));
    }

    #[tokio::test]
    async fn modified_tracked_file_shows_in_diff() {
        let dir = temp_dir("modified");
        init_repo(&dir);
        std::fs::write(dir.join("tracked.txt"), "line1\n").unwrap();
        git(&dir, &["add", "tracked.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.join("tracked.txt"), "line1\nline2\n").unwrap();

        let state = git_state(&dir).await.unwrap();
        assert!(state.diff.contains("tracked.txt"));
        assert!(state.diff.contains("line2"));
    }

    #[tokio::test]
    async fn clean_tree_yields_empty_state() {
        let dir = temp_dir("clean");
        init_repo(&dir);
        std::fs::write(dir.join("tracked.txt"), "line1\n").unwrap();
        git(&dir, &["add", "tracked.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        let state = git_state(&dir).await.unwrap();
        assert!(state.status.is_empty());
        assert!(state.diff.is_empty());
    }
}
