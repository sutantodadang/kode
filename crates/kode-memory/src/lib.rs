pub mod error;
pub mod ingat;
pub mod mock;
pub mod policy;
pub mod tool;
pub mod types;
pub mod wire;

pub use error::{MemoryError, Result};
pub use ingat::IngatAdapter;
pub use mock::MockEngineeringMemory;
pub use tool::RememberTool;
pub use types::{Memory, MemoryContext, MemoryKind, MemoryQuery, NewMemory, Provenance};
pub use wire::WireEntry;

/// Domain-level access to a local engineering-memory backend (Ingat).
///
/// Implementors translate this narrow surface into whatever wire protocol
/// the backend speaks; callers never see Ingat-specific JSON shapes.
#[async_trait::async_trait]
pub trait EngineeringMemory: Send + Sync {
    /// Backend reachability check.
    async fn health(&self) -> Result<()>;

    /// Ranked search over stored memories.
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<Memory>>;

    /// Persists a new memory, returning its id.
    async fn remember(&self, memory: &NewMemory) -> Result<String>;

    /// Backend-wide stats (total memory count, backend version).
    async fn stats(&self) -> Result<MemoryStats>;

    /// Idempotently imports team-shared wire entries (upsert by `id`).
    /// Returns [`MemoryError::Unsupported`] when the backend predates this
    /// operation (e.g. Ingat without the `/import` endpoint) — callers
    /// should degrade gracefully rather than treat that as a hard failure.
    async fn import_team(&self, entries: &[WireEntry]) -> Result<ImportCounts>;
}

/// Backend-wide stats reported by [`EngineeringMemory::stats`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStats {
    pub total: u64,
    pub version: String,
}

/// Result of [`EngineeringMemory::import_team`]: how many entries were
/// newly imported vs. already present (idempotent upsert by id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportCounts {
    pub imported: u64,
    pub skipped: u64,
}
