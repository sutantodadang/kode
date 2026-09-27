//! In-process Ingat memory backend.
//!
//! Implements Kode's [`EngineeringMemory`] over `ingat-core`'s headless
//! `ContextService` backed by a Kode-owned SQLite store — no HTTP call, no
//! child process, no service, no port. Every blocking core operation runs on
//! `spawn_blocking`, holding an `Arc` so a canceled caller future cannot drop
//! the service mid-operation.

use std::path::Path;
use std::sync::Arc;

use ingat_core::application::dtos::{
    IngestContextRequest, SearchRequest, SearchResultDto, WireMemoryEntry,
};
use ingat_core::application::services::ContextService;
use ingat_core::domain::{ContextKind, ContextRecord, DomainError, MemoryScope, QueryFilters};
use ingat_core::{EmbeddedOptions, open_embedded};

use crate::EngineeringMemory;
use crate::error::{MemoryError, Result};
use crate::mapping;
use crate::types::{Memory, MemoryKind, MemoryQuery, NewMemory};
use crate::wire::WireEntry;
use crate::{ImportCounts, MemoryStats};

/// Reported as [`MemoryStats::version`] for the embedded backend. Tracks the
/// pinned `ingat-core` revision in `Cargo.toml`; bump with the pin.
const EMBEDDED_VERSION: &str = "ingat-core 0.2.0";

/// An in-process Ingat memory store at a Kode-owned SQLite path.
pub struct EmbeddedIngat {
    service: Arc<ContextService>,
}

impl EmbeddedIngat {
    /// Opens (creating if needed) the Kode-owned SQLite store at `path`.
    /// Never touches the desktop/service's live store.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MemoryError::Unavailable(format!(
                    "cannot create ingat store dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let service = open_embedded(EmbeddedOptions {
            database_path: path.to_path_buf(),
        })
        .map_err(map_domain_error)?;
        Ok(Self {
            service: Arc::new(service),
        })
    }

    /// Runs a blocking core operation on the blocking pool. The `Arc` keeps the
    /// service alive until the operation finishes even if the caller's future
    /// is canceled.
    async fn blocking<T, F>(&self, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&ContextService) -> std::result::Result<T, DomainError> + Send + 'static,
    {
        let service = self.service.clone();
        tokio::task::spawn_blocking(move || op(&service).map_err(map_domain_error))
            .await
            .map_err(|e| MemoryError::Unavailable(format!("ingat worker stopped: {e}")))?
    }

    /// Imports one legacy record, preserving its UUID, scope and metadata while
    /// re-embedding the content with the embedded model. Returns `false` when
    /// the UUID already exists (idempotent rerun), without overwriting it.
    pub async fn import_record(&self, record: ContextRecord) -> Result<bool> {
        self.blocking(move |service| service.import_record(&record))
            .await
    }
}

#[async_trait::async_trait]
impl EngineeringMemory for EmbeddedIngat {
    async fn health(&self) -> Result<()> {
        self.blocking(|service| service.health().map(|_| ())).await
    }

    async fn search(&self, query: &MemoryQuery) -> Result<Vec<Memory>> {
        let request = SearchRequest {
            prompt: query.text.clone(),
            filters: QueryFilters {
                project: query.repository.clone(),
                kind: query.kind.map(kind_to_core),
                tag: None,
                ide: None,
            },
            limit: query.limit as usize,
        };
        let response = self
            .blocking(move |service| service.search(request))
            .await?;
        Ok(response
            .results
            .into_iter()
            .map(map_search_result)
            .collect())
    }

    async fn remember(&self, memory: &NewMemory) -> Result<String> {
        let request = IngestContextRequest {
            project: mapping::remember_project(memory),
            ide: "kode".to_string(),
            file_path: memory.context.files.first().cloned(),
            language: None,
            summary: memory.summary.clone(),
            body: mapping::remember_body(memory),
            tags: mapping::remember_tags(memory),
            kind: kind_to_core(memory.kind),
            scope: if memory.team {
                MemoryScope::Team
            } else {
                MemoryScope::Personal
            },
        };
        // `ingest` persists before returning, so a successful `remember`
        // means the record is durably committed.
        let summary = self
            .blocking(move |service| service.ingest(request))
            .await?;
        Ok(summary.id.to_string())
    }

    async fn stats(&self) -> Result<MemoryStats> {
        let total = self.blocking(|service| service.record_count()).await?;
        Ok(MemoryStats {
            total,
            version: EMBEDDED_VERSION.to_string(),
        })
    }

    async fn import_team(&self, entries: &[WireEntry]) -> Result<ImportCounts> {
        let mapped = entries
            .iter()
            .map(map_wire_entry)
            .collect::<Result<Vec<_>>>()?;
        let response = self
            .blocking(move |service| service.import_memories(mapped))
            .await?;
        Ok(ImportCounts {
            imported: response.imported as u64,
            skipped: response.skipped as u64,
        })
    }
}

/// Maps a Kode [`MemoryKind`] onto `ingat-core`'s `ContextKind`.
fn kind_to_core(kind: MemoryKind) -> ContextKind {
    match mapping::kind_wire_name(kind) {
        "fix-history" => ContextKind::FixHistory,
        name => ContextKind::Other(name.to_string()),
    }
}

/// Best-effort reverse of [`kind_to_core`], used as a fallback when a result
/// carries no `kode-kind:` tag.
fn kind_from_core(kind: &ContextKind) -> Option<MemoryKind> {
    match kind {
        ContextKind::FixHistory => Some(MemoryKind::HistoricalSolution),
        ContextKind::Other(s) => mapping::kind_from_wire_name(s),
        _ => None,
    }
}

fn map_search_result(dto: SearchResultDto) -> Memory {
    let (kind_from_tag, provenance, tags) = mapping::split_tags(dto.tags);
    let kind = kind_from_tag.or_else(|| kind_from_core(&dto.kind));

    Memory {
        id: dto.id.to_string(),
        kind,
        summary: dto.summary,
        body: dto.body,
        tags,
        provenance,
        score: dto.score,
        project: dto.project,
        created_at: dto.created_at.to_rfc3339(),
    }
}

/// Maps a Kode team-wire entry onto `ingat-core`'s `WireMemoryEntry`,
/// preserving the id so re-import stays idempotent.
fn map_wire_entry(entry: &WireEntry) -> Result<WireMemoryEntry> {
    let created_at = chrono::DateTime::parse_from_rfc3339(&entry.created_at)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| MemoryError::Protocol(format!("invalid wire timestamp: {e}")))?;

    Ok(WireMemoryEntry {
        v: entry.v,
        id: entry.id.clone(),
        hash: Some(entry.hash.clone()),
        kind: entry.kind.clone(),
        content: entry.content.clone(),
        tags: entry.tags.clone(),
        author: (!entry.author.is_empty()).then(|| entry.author.clone()),
        repository: entry
            .repository
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        created_at,
        provenance: Some(entry.provenance.clone()),
    })
}

fn map_domain_error(err: DomainError) -> MemoryError {
    match err {
        DomainError::Validation(m) | DomainError::LimitExceeded(m) => MemoryError::Service {
            code: "VALIDATION".to_string(),
            message: m,
        },
        DomainError::NotFound(m) => MemoryError::Service {
            code: "NOT_FOUND".to_string(),
            message: m,
        },
        DomainError::Storage(m) => MemoryError::Unavailable(m),
        DomainError::Embedding(m) => MemoryError::Protocol(format!("embedding: {m}")),
        DomainError::Other(m) => MemoryError::Protocol(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryContext, Provenance};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "kode-memory-embedded-{label}-{}-{nanos}.sqlite3",
            std::process::id()
        ))
    }

    fn memory(kind: MemoryKind, team: bool) -> NewMemory {
        NewMemory {
            kind,
            summary: "use rtk for shell".to_string(),
            body: "always prefix shell commands with rtk".to_string(),
            tags: vec!["tooling".to_string()],
            provenance: Provenance::ExplicitUser,
            context: MemoryContext {
                repository: Some("kode".to_string()),
                branch: Some("main".to_string()),
                commit: Some("abc123".to_string()),
                files: vec!["src/lib.rs".to_string()],
                symbols: vec!["Foo::bar".to_string()],
            },
            team,
        }
    }

    #[tokio::test]
    async fn remember_commits_and_stats_counts_records() {
        let db = temp_db("stats");
        let adapter = EmbeddedIngat::open(&db).unwrap();

        assert_eq!(adapter.stats().await.unwrap().total, 0);
        let id = adapter
            .remember(&memory(MemoryKind::ProjectRule, false))
            .await
            .unwrap();
        assert!(!id.is_empty());
        assert_eq!(adapter.stats().await.unwrap().total, 1);

        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn remember_then_search_maps_kind_provenance_and_user_tags() {
        let db = temp_db("search");
        let adapter = EmbeddedIngat::open(&db).unwrap();
        adapter
            .remember(&memory(MemoryKind::Convention, false))
            .await
            .unwrap();

        let results = adapter
            .search(&MemoryQuery {
                text: "rtk shell".to_string(),
                repository: None,
                kind: None,
                limit: 8,
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        let m = &results[0];
        assert_eq!(m.kind, Some(MemoryKind::Convention));
        assert_eq!(m.provenance, Some(Provenance::ExplicitUser));
        assert_eq!(m.tags, vec!["tooling".to_string()]);
        assert_eq!(m.project, "kode");

        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn import_team_is_idempotent_by_id() {
        let db = temp_db("import");
        let adapter = EmbeddedIngat::open(&db).unwrap();
        let entry = WireEntry::new(
            &memory(MemoryKind::Convention, true),
            Some("alice".to_string()),
        );

        let first = adapter
            .import_team(std::slice::from_ref(&entry))
            .await
            .unwrap();
        assert_eq!(first.imported, 1);
        assert_eq!(first.skipped, 0);

        let second = adapter.import_team(&[entry]).await.unwrap();
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 1);

        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn health_ok_on_fresh_store() {
        let db = temp_db("health");
        let adapter = EmbeddedIngat::open(&db).unwrap();
        adapter.health().await.unwrap();
        let _ = std::fs::remove_file(&db);
    }
}
