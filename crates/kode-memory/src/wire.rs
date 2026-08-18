use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::NewMemory;

/// Format version of [`WireEntry`]. Bump when the JSON shape changes in a
/// way readers must know about.
pub const WIRE_VERSION: u32 = 1;

/// The single canonical path for the git-backed team-memory file, relative
/// to a repository root: `.kode/memory/team.jsonl`. Both `RememberTool`
/// (this crate) and the `kode` CLI's `remember`/`team_memory` modules build
/// this same path so there's exactly one place the layout is defined.
pub fn team_file_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".kode").join("memory").join("team.jsonl")
}

/// One line of `.kode/memory/team.jsonl` — the git-backed team-memory wire
/// format. Append-only: every `remember --team` writes exactly one of these
/// as a new line. See `docs/superpowers/specs/2026-08-18-team-memory-design.md`
/// for the format contract.
///
/// Unknown fields are ignored on parse (no `deny_unknown_fields`) so older
/// Kode versions tolerate entries written by newer ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireEntry {
    pub v: u32,
    pub id: String,
    pub hash: String,
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub repository: Option<String>,
    pub created_at: String,
    pub provenance: String,
}

impl WireEntry {
    /// Builds a wire entry from a [`NewMemory`] at remember-time: generates
    /// a fresh ULID id, hashes the canonical content (body, falling back to
    /// summary when body is empty — matching the fallback the Ingat adapter
    /// already uses), and stamps `created_at` as `now`.
    pub fn new(memory: &NewMemory, author: Option<String>) -> Self {
        let content = canonical_content(memory);
        let hash = hash_content(&content);
        Self {
            v: WIRE_VERSION,
            id: ulid::Ulid::new().to_string(),
            hash,
            kind: memory.kind.as_kebab().to_string(),
            content,
            tags: memory.tags.clone(),
            author: author.unwrap_or_default(),
            repository: memory.context.repository.clone(),
            created_at: now_rfc3339(),
            provenance: memory.provenance.as_kebab().to_string(),
        }
    }
}

/// The text a wire entry's `hash` is computed over: `body`, falling back to
/// `summary` when `body` is empty.
fn canonical_content(memory: &NewMemory) -> String {
    if memory.body.is_empty() {
        memory.summary.clone()
    } else {
        memory.body.clone()
    }
}

fn hash_content(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("sha256:{digest:x}")
}

/// Appends one [`WireEntry`] as a JSON line to `path`, creating parent
/// directories and the file itself as needed.
pub fn append_entry(path: &Path, entry: &WireEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Reads every wire entry from `path`. A missing file yields an empty
/// result (no error — nothing shared yet is a normal state). Each line that
/// fails to parse as JSON is skipped and counted in the second return value
/// — never panics, never aborts the read.
pub fn read_entries(path: &Path) -> (Vec<WireEntry>, usize) {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return (Vec::new(), 0),
    };

    let mut entries = Vec::new();
    let mut corrupt_skipped = 0;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<WireEntry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(_) => corrupt_skipped += 1,
        }
    }
    (entries, corrupt_skipped)
}

/// Formats `t` as an RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) with no
/// external date/time dependency. Uses Howard Hinnant's `civil_from_days`
/// algorithm (proleptic Gregorian, valid for the entire `SystemTime` range
/// we care about) to turn days-since-epoch into a calendar date.
fn format_rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn now_rfc3339() -> String {
    format_rfc3339(SystemTime::now())
}

/// Converts a day count since the Unix epoch (1970-01-01) into a
/// `(year, month, day)` civil date. http://howardhinnant.github.io/date_algorithms.html
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryContext, MemoryKind, Provenance};

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "kode-memory-wire-test-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn memory() -> NewMemory {
        NewMemory {
            kind: MemoryKind::ProjectRule,
            summary: "always prefix shell with rtk".to_string(),
            body: "always prefix shell commands with rtk for token savings".to_string(),
            tags: vec!["tooling".to_string()],
            provenance: Provenance::ExplicitUser,
            context: MemoryContext {
                repository: Some("kode".to_string()),
                ..Default::default()
            },
            team: true,
        }
    }

    #[test]
    fn format_rfc3339_epoch_zero() {
        assert_eq!(format_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_rfc3339_known_instant() {
        // 946684800 is the well-known 2000-01-01T00:00:00Z epoch second.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(946_684_800);
        assert_eq!(format_rfc3339(t), "2000-01-01T00:00:00Z");
    }

    #[test]
    fn hash_is_stable_for_same_content() {
        let m = memory();
        let a = WireEntry::new(&m, Some("alice".to_string()));
        let b = WireEntry::new(&m, Some("alice".to_string()));
        assert_eq!(a.hash, b.hash);
        assert!(a.hash.starts_with("sha256:"));
        // Ids are independently generated, so they differ even for
        // identical content.
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn new_falls_back_to_summary_when_body_empty() {
        let mut m = memory();
        m.body = String::new();
        let entry = WireEntry::new(&m, None);
        assert_eq!(entry.content, m.summary);
        assert_eq!(entry.author, "");
    }

    #[test]
    fn append_and_read_round_trips() {
        let path = temp_path("roundtrip");
        let entry = WireEntry::new(&memory(), Some("bob".to_string()));

        append_entry(&path, &entry).unwrap();
        let (entries, corrupt) = read_entries(&path);

        assert_eq!(corrupt, 0);
        assert_eq!(entries, vec![entry]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_accumulates_multiple_entries() {
        let path = temp_path("accumulate");
        let e1 = WireEntry::new(&memory(), Some("a".to_string()));
        let e2 = WireEntry::new(&memory(), Some("b".to_string()));

        append_entry(&path, &e1).unwrap();
        append_entry(&path, &e2).unwrap();

        let (entries, corrupt) = read_entries(&path);
        assert_eq!(corrupt, 0);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, e1.id);
        assert_eq!(entries[1].id, e2.id);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_entries_missing_file_yields_empty() {
        let path = temp_path("missing");
        let (entries, corrupt) = read_entries(&path);
        assert!(entries.is_empty());
        assert_eq!(corrupt, 0);
    }

    #[test]
    fn read_entries_skips_corrupt_lines_without_panicking() {
        let path = temp_path("corrupt");
        let good = WireEntry::new(&memory(), Some("carol".to_string()));
        let content = format!(
            "{}\nnot json at all\n{{\"partial\": true}}\n",
            serde_json::to_string(&good).unwrap()
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();

        let (entries, corrupt) = read_entries(&path);
        assert_eq!(entries, vec![good]);
        assert_eq!(corrupt, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_entries_ignores_unknown_fields() {
        let path = temp_path("unknown-fields");
        let good = WireEntry::new(&memory(), Some("dave".to_string()));
        let mut value = serde_json::to_value(&good).unwrap();
        value["future_field"] = serde_json::json!("from a newer Kode");
        std::fs::write(&path, format!("{}\n", value)).unwrap();

        let (entries, corrupt) = read_entries(&path);
        assert_eq!(corrupt, 0);
        assert_eq!(entries, vec![good]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn team_file_path_is_dot_kode_memory_team_jsonl() {
        let root = Path::new("/repo");
        assert_eq!(
            team_file_path(root),
            Path::new("/repo/.kode/memory/team.jsonl")
        );
    }

    #[test]
    fn appending_creates_missing_parent_dirs() {
        let path = temp_path("nested")
            .join(".kode")
            .join("memory")
            .join("team.jsonl");
        let entry = WireEntry::new(&memory(), None);
        append_entry(&path, &entry).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(path.ancestors().nth(3).unwrap());
    }
}
