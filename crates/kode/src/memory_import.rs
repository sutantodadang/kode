//! `kode memory import --from <export.jsonl>`: import a legacy
//! `ingat_export` JSONL file into Kode's native memory store.
//!
//! All entries are validated before the writable store is opened, and import
//! proceeds one record at a time so a failure reports exact partial counts and
//! can be resumed (UUID idempotence).

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use kode_core::config::KodeConfig;
use kode_memory::{EmbeddedIngat, LegacyExportLine};

/// Maximum accepted export file size.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum accepted single JSONL line.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Imports `from` into the store selected by `root`'s config.
pub async fn run(root: &Path, from: &Path) -> Result<()> {
    let config = KodeConfig::load(root)?;
    if !config.ingat.enabled {
        anyhow::bail!("memory is disabled in config — enable [ingat] to import");
    }
    let store_path = match &config.ingat.store_path {
        Some(path) => path.clone(),
        None => kode_core::default_ingat_store_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine Kode home for the memory store"))?,
    };

    let metadata = std::fs::metadata(from)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", from.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        anyhow::bail!("import file exceeds the 64 MiB limit");
    }
    let contents = std::fs::read_to_string(from)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", from.display()))?;

    // Parse and validate every record before touching the writable store.
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in contents.lines().enumerate() {
        let lineno = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > MAX_LINE_BYTES {
            anyhow::bail!("line {lineno}: exceeds the 1 MiB line limit");
        }
        let parsed: LegacyExportLine = serde_json::from_str(trimmed)
            .map_err(|_| anyhow::anyhow!("line {lineno}: not a valid export record"))?;
        if parsed.v != 1 {
            anyhow::bail!("line {lineno}: unsupported export version {}", parsed.v);
        }
        if !seen.insert(parsed.record.id) {
            anyhow::bail!("line {lineno}: duplicate record id within the input");
        }
        let text = format!("{} {}", parsed.record.summary, parsed.record.body);
        if kode_core::secrets::looks_like_secret(&text.to_lowercase()) {
            anyhow::bail!("line {lineno}: possible secret — refusing to import");
        }
        records.push(parsed.record);
    }

    let backend = EmbeddedIngat::open(&store_path)?;
    let mut imported = 0u64;
    let mut skipped = 0u64;
    for (index, record) in records.into_iter().enumerate() {
        match backend.import_record(record).await {
            Ok(true) => imported += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                anyhow::bail!(
                    "import failed after {imported} imported, {skipped} skipped (record {}): {e}",
                    index + 1
                );
            }
        }
    }

    println!("imported {imported} · skipped {skipped}");
    Ok(())
}
