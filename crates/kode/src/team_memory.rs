use std::path::{Path, PathBuf};

use kode_memory::EngineeringMemory;
use kode_memory::wire::{self, WireEntry};

/// Path to the git-backed team-memory file for a repository:
/// `.kode/memory/team.jsonl`. Thin re-export of `kode_memory::wire`'s
/// canonical path builder — this crate never redefines the layout, so
/// `kode remember --team`, `RememberTool`'s `team: true` writes, and this
/// module's import-on-start all agree on where the file lives.
pub fn team_file_path(cwd: &Path) -> PathBuf {
    wire::team_file_path(cwd)
}

/// Appends `entry` to the repo's team-memory file (creating it and its
/// parent directories if needed). Shared by `kode remember --team` and the
/// TUI's team-share flow.
pub fn share(cwd: &Path, entry: &WireEntry) -> std::io::Result<()> {
    wire::append_entry(&team_file_path(cwd), entry)
}

/// Outcome of [`import_on_start`] — what happened when Kode tried to import
/// the repo's team-memory file into the local Ingat backend at session
/// start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    /// Entries Ingat reported as newly imported (idempotent upsert by id —
    /// re-importing already-known entries reports `0` for them, not an
    /// error).
    pub new: u64,
    /// Lines in `team.jsonl` that failed to parse as a wire entry and were
    /// skipped.
    pub corrupt_skipped: usize,
    /// `true` when the local Ingat build predates the `/import` endpoint
    /// (404) — degrade gracefully rather than treat this as a hard error.
    pub unsupported: bool,
}

impl ImportSummary {
    /// The knowledge-band/transcript note to show for this summary, if
    /// any. `None` when there's nothing worth telling the user: zero new
    /// entries, zero corrupt lines, and Ingat supports import fine.
    pub fn note(&self) -> Option<String> {
        if self.unsupported {
            Some("Ingat needs an update — run `kode setup`".to_string())
        } else if self.new > 0 {
            Some(format!("{} new team memories", self.new))
        } else {
            None
        }
    }
}

/// Reads the repo's team-memory file and imports every entry into `memory`.
/// Import is idempotent (upsert by `id`), so calling this on every session
/// start is safe — already-known entries just come back as `skipped`.
///
/// A missing file, or one with zero parseable entries, is a normal, silent
/// no-op (`new: 0`). Corrupt lines are skipped and counted, never fatal.
/// Backend errors other than [`kode_memory::MemoryError::Unsupported`]
/// degrade to `new: 0` rather than propagating — engineering memory is a
/// nice-to-have at session start, not something worth failing startup over.
pub async fn import_on_start(memory: &dyn EngineeringMemory, cwd: &Path) -> ImportSummary {
    let path = team_file_path(cwd);
    let (entries, corrupt_skipped) = wire::read_entries(&path);
    if entries.is_empty() {
        return ImportSummary {
            new: 0,
            corrupt_skipped,
            unsupported: false,
        };
    }

    match memory.import_team(&entries).await {
        Ok(counts) => ImportSummary {
            new: counts.imported,
            corrupt_skipped,
            unsupported: false,
        },
        Err(kode_memory::MemoryError::Unsupported(_)) => ImportSummary {
            new: 0,
            corrupt_skipped,
            unsupported: true,
        },
        Err(_) => ImportSummary {
            new: 0,
            corrupt_skipped,
            unsupported: false,
        },
    }
}

/// `kode memory status`: prints team.jsonl entry/corrupt counts. Whether
/// the local Ingat build supports `/import` isn't probed here — a health
/// round-trip just to answer that isn't worth it for a status line, so v1
/// reports it as "unknown" (per the design doc's "keep minimal" note).
pub fn print_status(cwd: &Path) {
    let path = team_file_path(cwd);
    let (entries, corrupt) = wire::read_entries(&path);
    println!("team memory file: {}", path.display());
    println!("entries: {}", entries.len());
    println!("corrupt lines skipped: {corrupt}");
    println!("ingat import support: unknown");
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_memory::{
        ImportCounts, MemoryContext, MemoryError, MemoryKind, MockEngineeringMemory, NewMemory,
        Provenance,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-team-memory-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_memory() -> NewMemory {
        NewMemory {
            kind: MemoryKind::Convention,
            summary: "always squash-merge feature branches".to_string(),
            body: String::new(),
            tags: vec![],
            provenance: Provenance::ExplicitUser,
            context: MemoryContext::default(),
            team: true,
        }
    }

    #[test]
    fn team_file_path_matches_wire_module() {
        let dir = temp_dir("path");
        assert_eq!(team_file_path(&dir), wire::team_file_path(&dir));
    }

    #[test]
    fn share_appends_to_team_file() {
        let dir = temp_dir("share");
        let entry = WireEntry::new(&sample_memory(), Some("alice".to_string()));
        share(&dir, &entry).unwrap();

        let (entries, corrupt) = wire::read_entries(&team_file_path(&dir));
        assert_eq!(corrupt, 0);
        assert_eq!(entries, vec![entry]);
    }

    #[tokio::test]
    async fn import_on_start_missing_file_is_silent_no_op() {
        let dir = temp_dir("missing");
        let mock = MockEngineeringMemory::default();

        let summary = import_on_start(&mock, &dir).await;

        assert_eq!(summary.new, 0);
        assert_eq!(summary.corrupt_skipped, 0);
        assert!(!summary.unsupported);
        assert!(summary.note().is_none());
    }

    #[tokio::test]
    async fn import_on_start_reports_new_count_and_note() {
        let dir = temp_dir("new");
        let entry = WireEntry::new(&sample_memory(), Some("alice".to_string()));
        share(&dir, &entry).unwrap();

        let mock = MockEngineeringMemory {
            import_counts: ImportCounts {
                imported: 1,
                skipped: 0,
            },
            ..Default::default()
        };

        let summary = import_on_start(&mock, &dir).await;

        assert_eq!(summary.new, 1);
        assert_eq!(summary.note(), Some("1 new team memories".to_string()));
        assert_eq!(mock.imported.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn import_on_start_zero_new_entries_has_no_note() {
        let dir = temp_dir("zero-new");
        let entry = WireEntry::new(&sample_memory(), None);
        share(&dir, &entry).unwrap();

        let mock = MockEngineeringMemory::default(); // import_counts defaults to 0/0

        let summary = import_on_start(&mock, &dir).await;

        assert_eq!(summary.new, 0);
        assert!(summary.note().is_none());
    }

    #[tokio::test]
    async fn import_on_start_unsupported_backend_maps_to_setup_note() {
        let dir = temp_dir("unsupported");
        let entry = WireEntry::new(&sample_memory(), None);
        share(&dir, &entry).unwrap();

        let mock = MockEngineeringMemory {
            import_error: Some(MemoryError::Unsupported("no /import endpoint".to_string())),
            ..Default::default()
        };

        let summary = import_on_start(&mock, &dir).await;

        assert!(summary.unsupported);
        assert_eq!(
            summary.note(),
            Some("Ingat needs an update — run `kode setup`".to_string())
        );
    }

    #[tokio::test]
    async fn import_on_start_counts_corrupt_lines_without_blocking_good_ones() {
        let dir = temp_dir("corrupt");
        let path = team_file_path(&dir);
        let good = WireEntry::new(&sample_memory(), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}\nnot json at all\n",
                serde_json::to_string(&good).unwrap()
            ),
        )
        .unwrap();

        let mock = MockEngineeringMemory {
            import_counts: ImportCounts {
                imported: 1,
                skipped: 0,
            },
            ..Default::default()
        };

        let summary = import_on_start(&mock, &dir).await;

        assert_eq!(summary.corrupt_skipped, 1);
        assert_eq!(summary.new, 1);
    }

    #[tokio::test]
    async fn import_on_start_other_errors_degrade_silently() {
        let dir = temp_dir("other-error");
        let entry = WireEntry::new(&sample_memory(), None);
        share(&dir, &entry).unwrap();

        let mock = MockEngineeringMemory {
            import_error: Some(MemoryError::Unavailable("connection refused".to_string())),
            ..Default::default()
        };

        let summary = import_on_start(&mock, &dir).await;

        assert_eq!(summary.new, 0);
        assert!(!summary.unsupported);
        assert!(summary.note().is_none());
    }

    #[test]
    fn print_status_does_not_panic_on_missing_file() {
        let dir = temp_dir("status-missing");
        print_status(&dir);
    }
}
