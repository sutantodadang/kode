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

/// One file's line-count delta from `git diff --numstat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatRow {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

/// Working-tree dirty flag + per-file `git diff --numstat` rows. Polled
/// lazily (TUI start + after each task completes) rather than on a fixed
/// interval — see `kode::tui::spawn_git_poll`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoState {
    /// True when `git status --porcelain` reports anything at all
    /// (tracked or untracked).
    pub dirty: bool,
    pub numstat: Vec<NumstatRow>,
}

/// Captures the working tree's dirty flag and per-file diff stat for
/// `root`: `git status --porcelain=v1` (dirty) and `git diff --numstat`,
/// falling back to `git diff --cached --numstat` when the unstaged diff is
/// empty (mirrors `git_state`'s staged fallback). `None` on any failure —
/// git missing, `root` not a repo, non-zero exit, timeout.
pub async fn repo_state(root: &Path) -> Option<RepoState> {
    let status = run_git(root, &["status", "--porcelain=v1"]).await?;
    let dirty = !status.trim().is_empty();

    let mut numstat_raw = run_git(root, &["diff", "--numstat"]).await?;
    if numstat_raw.trim().is_empty() {
        numstat_raw = run_git(root, &["diff", "--cached", "--numstat"]).await?;
    }

    Some(RepoState {
        dirty,
        numstat: parse_numstat(&numstat_raw),
    })
}

/// Parses `git diff --numstat` output (`<added>\t<deleted>\t<path>` per
/// line). Binary files report `-\t-\t<path>` — skipped, since they have no
/// meaningful +/- line count.
pub fn parse_numstat(raw: &str) -> Vec<NumstatRow> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?.parse::<u32>().ok()?;
            let deleted = parts.next()?.parse::<u32>().ok()?;
            let path = parts.next()?.trim();
            if path.is_empty() {
                return None;
            }
            Some(NumstatRow {
                path: path.to_string(),
                added,
                deleted,
            })
        })
        .collect()
}

/// One file's state against `HEAD`: line counts plus a content fingerprint,
/// so edits that keep the same counts still register as a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileDelta {
    /// Lines added against `HEAD`.
    pub added: u32,
    /// Lines deleted against `HEAD`.
    pub deleted: u32,
    /// Hash of the file's current content (0 when missing/unreadable).
    pub fingerprint: u64,
}

/// Per-file [`FileDelta`] against `HEAD`, keyed by `root`-relative path,
/// untracked files included as all-added.
pub type ChangeSnapshot = std::collections::BTreeMap<String, FileDelta>;

/// Largest untracked file whose lines are counted; bigger ones count as 0.
const UNTRACKED_COUNT_LIMIT: u64 = 4 * 1024 * 1024;

/// Largest file whose bytes are hashed for the fingerprint.
const FINGERPRINT_CONTENT_LIMIT: u64 = 16 * 1024 * 1024;

/// Content fingerprint of `path`; 0 when missing or unreadable.
fn file_fingerprint(path: &Path) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let mut hasher = DefaultHasher::new();
    if meta.len() <= FINGERPRINT_CONTENT_LIMIT {
        match std::fs::read(path) {
            Ok(bytes) => bytes.hash(&mut hasher),
            Err(_) => return 0,
        }
    } else {
        // ponytail: huge files hash (len, mtime) instead of content; an edit
        // that preserves both within the clock granularity goes unseen.
        meta.len().hash(&mut hasher);
        meta.modified().ok().hash(&mut hasher);
    }
    hasher.finish()
}

/// Snapshot of what differs from `HEAD` right now. `None` when git fails or
/// `root` is not a repository. A repository without commits has no `HEAD`,
/// so the staged diff is used instead.
pub async fn change_snapshot(root: &Path) -> Option<ChangeSnapshot> {
    let raw = match run_git(
        root,
        &["diff", "HEAD", "--numstat", "--relative", "--no-renames"],
    )
    .await
    {
        Some(raw) => raw,
        None => {
            run_git(
                root,
                &[
                    "diff",
                    "--cached",
                    "--numstat",
                    "--relative",
                    "--no-renames",
                ],
            )
            .await?
        }
    };
    let mut snapshot: ChangeSnapshot = parse_numstat(&raw)
        .into_iter()
        .map(|row| {
            let fingerprint = file_fingerprint(&root.join(&row.path));
            (
                row.path,
                FileDelta {
                    added: row.added,
                    deleted: row.deleted,
                    fingerprint,
                },
            )
        })
        .collect();
    let untracked = run_git(root, &["ls-files", "--others", "--exclude-standard"]).await?;
    for path in untracked.lines().map(str::trim).filter(|p| !p.is_empty()) {
        let full = root.join(path);
        let lines = match std::fs::metadata(&full) {
            Ok(meta) if meta.len() <= UNTRACKED_COUNT_LIMIT => std::fs::read_to_string(&full)
                .map(|text| text.lines().count() as u32)
                .unwrap_or(0),
            _ => 0,
        };
        snapshot.insert(
            path.to_string(),
            FileDelta {
                added: lines,
                deleted: 0,
                fingerprint: file_fingerprint(&full),
            },
        );
    }
    Some(snapshot)
}

/// Files whose delta against `HEAD` moved between two snapshots, reported
/// with their `after` counts. Files restored to their `HEAD` content are
/// omitted: there is nothing left to show.
pub fn change_set(before: &ChangeSnapshot, after: &ChangeSnapshot) -> Vec<NumstatRow> {
    after
        .iter()
        .filter(|(path, delta)| before.get(*path) != Some(*delta))
        .map(|(path, delta)| NumstatRow {
            path: path.clone(),
            added: delta.added,
            deleted: delta.deleted,
        })
        .collect()
}

/// Files that differed from `HEAD` before and match it now: the task
/// undid them.
pub fn reverted_paths(before: &ChangeSnapshot, after: &ChangeSnapshot) -> Vec<String> {
    before
        .keys()
        .filter(|p| !after.contains_key(*p))
        .cloned()
        .collect()
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

    #[test]
    fn parse_numstat_reads_added_deleted_and_path() {
        let raw = "3\t1\tsrc/foo.rs\n0\t5\tsrc/bar.rs\n";
        let rows = parse_numstat(raw);
        assert_eq!(
            rows,
            vec![
                NumstatRow {
                    path: "src/foo.rs".to_string(),
                    added: 3,
                    deleted: 1,
                },
                NumstatRow {
                    path: "src/bar.rs".to_string(),
                    added: 0,
                    deleted: 5,
                },
            ]
        );
    }

    #[test]
    fn parse_numstat_skips_binary_rows_gracefully() {
        let raw = "3\t1\tsrc/foo.rs\n-\t-\tassets/logo.png\n2\t0\tsrc/baz.rs\n";
        let rows = parse_numstat(raw);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "src/foo.rs");
        assert_eq!(rows[1].path, "src/baz.rs");
    }

    #[test]
    fn parse_numstat_empty_input_yields_empty_rows() {
        assert!(parse_numstat("").is_empty());
    }

    #[tokio::test]
    async fn repo_state_non_repo_returns_none() {
        let dir = temp_dir("repo-state-non-repo");
        assert!(repo_state(&dir).await.is_none());
    }

    #[tokio::test]
    async fn repo_state_clean_tree_is_not_dirty_with_empty_numstat() {
        let dir = temp_dir("repo-state-clean");
        init_repo(&dir);
        std::fs::write(dir.join("tracked.txt"), "line1\n").unwrap();
        git(&dir, &["add", "tracked.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        let state = repo_state(&dir).await.unwrap();
        assert!(!state.dirty);
        assert!(state.numstat.is_empty());
    }

    #[tokio::test]
    async fn repo_state_modified_file_is_dirty_with_numstat_row() {
        let dir = temp_dir("repo-state-modified");
        init_repo(&dir);
        std::fs::write(dir.join("tracked.txt"), "line1\n").unwrap();
        git(&dir, &["add", "tracked.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        std::fs::write(dir.join("tracked.txt"), "line1\nline2\n").unwrap();

        let state = repo_state(&dir).await.unwrap();
        assert!(state.dirty);
        assert_eq!(state.numstat.len(), 1);
        assert_eq!(state.numstat[0].path, "tracked.txt");
        assert_eq!(state.numstat[0].added, 1);
    }

    #[tokio::test]
    async fn repo_state_untracked_file_is_dirty_with_no_numstat() {
        let dir = temp_dir("repo-state-untracked");
        init_repo(&dir);
        std::fs::write(dir.join("new.txt"), "hi").unwrap();

        let state = repo_state(&dir).await.unwrap();
        assert!(state.dirty);
        assert!(state.numstat.is_empty());
    }

    #[tokio::test]
    async fn change_snapshot_non_repo_is_none() {
        let dir = temp_dir("snap-non-repo");
        assert!(change_snapshot(&dir).await.is_none());
    }

    #[tokio::test]
    async fn change_snapshot_counts_tracked_and_untracked() {
        let dir = temp_dir("snap-mixed");
        init_repo(&dir);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("new.txt"), "x\ny\n").unwrap();

        let snap = change_snapshot(&dir).await.unwrap();
        let a = snap.get("a.txt").unwrap();
        assert_eq!((a.added, a.deleted), (2, 0));
        let n = snap.get("new.txt").unwrap();
        assert_eq!((n.added, n.deleted), (2, 0));
    }

    #[tokio::test]
    async fn change_snapshot_without_commits_uses_staged() {
        let dir = temp_dir("snap-no-head");
        init_repo(&dir);
        std::fs::write(dir.join("s.txt"), "a\nb\n").unwrap();
        git(&dir, &["add", "s.txt"]);

        let snap = change_snapshot(&dir).await.unwrap();
        let s = snap.get("s.txt").unwrap();
        assert_eq!((s.added, s.deleted), (2, 0));
    }

    fn fd(added: u32, deleted: u32) -> FileDelta {
        FileDelta {
            added,
            deleted,
            fingerprint: 0,
        }
    }

    #[tokio::test]
    async fn change_set_sees_edit_that_keeps_line_counts() {
        let dir = temp_dir("snap-same-counts");
        init_repo(&dir);
        std::fs::write(dir.join("a.rs"), "let x = 1;\n").unwrap();
        git(&dir, &["add", "a.rs"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.rs"), "let x = 2;\n").unwrap();
        let before = change_snapshot(&dir).await.unwrap();
        std::fs::write(dir.join("a.rs"), "let x = 3;\n").unwrap();
        let after = change_snapshot(&dir).await.unwrap();
        let rows = change_set(&before, &after);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "a.rs");
    }

    #[test]
    fn reverted_paths_are_files_back_at_head() {
        let mut before = ChangeSnapshot::new();
        before.insert("tried.rs".into(), fd(4, 0));
        before.insert("kept.rs".into(), fd(1, 0));
        let mut after = ChangeSnapshot::new();
        after.insert("kept.rs".into(), fd(2, 0));
        assert_eq!(
            reverted_paths(&before, &after),
            vec!["tried.rs".to_string()]
        );
    }

    #[test]
    fn change_set_reports_only_files_whose_delta_moved() {
        let mut before = ChangeSnapshot::new();
        before.insert("dirty-before.rs".into(), fd(3, 1));
        before.insert("untouched.rs".into(), fd(5, 0));
        let mut after = before.clone();
        after.insert("dirty-before.rs".into(), fd(7, 1));
        after.insert("new.rs".into(), fd(10, 0));

        let rows = change_set(&before, &after);
        assert_eq!(
            rows,
            vec![
                NumstatRow {
                    path: "dirty-before.rs".into(),
                    added: 7,
                    deleted: 1
                },
                NumstatRow {
                    path: "new.rs".into(),
                    added: 10,
                    deleted: 0
                },
            ]
        );
    }
}
