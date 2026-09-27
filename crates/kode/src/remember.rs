use std::path::Path;

use kode_core::config::KodeConfig;
use kode_memory::wire::WireEntry;
use kode_memory::{MemoryContext, MemoryKind, NewMemory, Provenance};

use crate::team_memory;

/// Runs `kode remember <text> [--kind <kind>] [--tag <tag> ...] [--team]`.
///
/// Explicit user memory is written directly to Ingat with no permission
/// prompt (per product spec §31) — the user asked for it by name. `--team`
/// additionally appends the memory to the git-backed
/// `.kode/memory/team.jsonl` file so teammates pick it up on their next
/// session start (see `docs/superpowers/specs/2026-08-18-team-memory-design.md`).
pub async fn run(
    text: &str,
    kind: &str,
    tags: Vec<String>,
    team: bool,
    cwd: &Path,
) -> anyhow::Result<()> {
    let kind = MemoryKind::from_kebab(kind).ok_or_else(|| {
        let valid: Vec<&str> = MemoryKind::ALL.iter().map(MemoryKind::as_kebab).collect();
        anyhow::anyhow!(
            "invalid --kind {kind:?}; valid values: {}",
            valid.join(", ")
        )
    })?;

    let config = KodeConfig::load(cwd)?;
    if !config.ingat.enabled {
        anyhow::bail!("ingat disabled in config");
    }

    let context = gather_context(cwd).await;
    let author = git_output(cwd, &["config", "user.name"]).await;
    let summary = truncate_summary(text, 100);

    let memory = NewMemory {
        kind,
        summary,
        body: text.to_string(),
        tags,
        provenance: Provenance::ExplicitUser,
        context,
        team,
    };

    let backend = match crate::memory_backend::connect(&config.ingat).await? {
        Some(backend) => backend,
        None => anyhow::bail!("memory is disabled in config — enable [ingat] to remember"),
    };

    match backend.remember(&memory).await {
        Ok(id) => {
            println!("remembered ({}): {id}", kind.as_kebab());
            if team {
                let entry = WireEntry::new(&memory, author);
                team_memory::share(cwd, &entry)?;
                println!(
                    "shared with team: {}",
                    team_memory::team_file_path(cwd).display()
                );
            }
            Ok(())
        }
        Err(kode_memory::MemoryError::Unavailable(msg)) => {
            anyhow::bail!("memory store unavailable: {msg}")
        }
        Err(err) => Err(anyhow::anyhow!(err)),
    }
}

/// Truncates `text` to at most `max_chars` characters (not bytes), safe on
/// any UTF-8 boundary.
fn truncate_summary(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Best-effort repository/branch/commit context: repository name from the
/// cwd directory name, branch/commit from `git`. Any failure (not a git
/// repo, git missing) silently yields `None` for that field — this is
/// metadata, not something worth failing `remember` over.
async fn gather_context(cwd: &Path) -> MemoryContext {
    let repository = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string());
    let branch = git_output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let commit = git_output(cwd, &["rev-parse", "--short", "HEAD"]).await;

    MemoryContext {
        repository,
        branch,
        commit,
        files: Vec::new(),
        symbols: Vec::new(),
    }
}

async fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-remember-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a `[ingat]` config pointing the native store at a temp file
    /// under `dir`, so tests never touch the real `~/.kode` store.
    fn write_ingat_config(dir: &Path) {
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        let db = dir
            .join("memory.sqlite3")
            .display()
            .to_string()
            .replace('\\', "/");
        std::fs::write(
            kode_dir.join("config.toml"),
            format!("[ingat]\nstore_path = \"{db}\"\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn remember_team_appends_wire_entry_and_prints_share_path() {
        let dir = temp_dir("team");
        write_ingat_config(&dir);

        run(
            "always squash-merge feature branches",
            "convention",
            vec![],
            true,
            &dir,
        )
        .await
        .unwrap();

        let path = team_memory::team_file_path(&dir);
        let (entries, corrupt) = kode_memory::wire::read_entries(&path);
        assert_eq!(corrupt, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "always squash-merge feature branches");
        assert_eq!(entries[0].kind, "convention");
        assert_eq!(entries[0].provenance, "explicit-user");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn remember_without_team_does_not_write_team_file() {
        let dir = temp_dir("no-team");
        write_ingat_config(&dir);

        run(
            "a personal-only note about local setup",
            "project-rule",
            vec![],
            false,
            &dir,
        )
        .await
        .unwrap();

        let path = team_memory::team_file_path(&dir);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_summary_is_char_safe() {
        assert_eq!(truncate_summary("hello world", 5), "hello");
        assert_eq!(truncate_summary("hi", 5), "hi");
    }
}
