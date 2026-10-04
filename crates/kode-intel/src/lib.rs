pub mod embedded;
pub mod error;
pub mod ffi;
pub mod mapping;
pub mod mock;
pub mod types;

pub use embedded::EmbeddedZindeks;
pub use error::{IntelError, Result};
pub use mock::MockCodeIntelligence;
pub use types::{
    ArchSymbol, ArchitectureSummary, CodeContext, CodeContextRequest, CodeSearchResult,
    FileOutline, GraphSymbol, IntelHealth, OutlineSymbol, TraceDirection, TraceNode,
};

/// Domain-level access to a local code intelligence backend (zindeks).
///
/// Implementors translate this narrow surface into whatever wire protocol
/// the backend speaks; callers never see zindeks-specific JSON shapes.
#[async_trait::async_trait]
pub trait CodeIntelligence: Send + Sync {
    /// Backend health / index counts.
    async fn health(&self) -> Result<IntelHealth>;

    /// Assemble a token-budgeted, task-scoped context blob for `request`.
    async fn get_context(&self, request: CodeContextRequest) -> Result<CodeContext>;

    /// Ranked keyword/semantic search across the indexed repository.
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<CodeSearchResult>>;

    /// Symbol outline for a single file.
    async fn file_outline(&self, path: &str) -> Result<FileOutline>;

    /// Repo-wide shape: totals plus entry points and fan-out/fan-in leaders,
    /// at most `limit` per list.
    async fn architecture(&self, _limit: u32) -> Result<ArchitectureSummary> {
        Err(IntelError::Tool(
            "architecture is not supported by this backend".to_string(),
        ))
    }

    /// Exact-name symbol matches, optionally only in repo-relative `path`
    /// (`/`-separated). Sorted by degree descending then path; at most 200.
    async fn exact_symbols(&self, _name: &str, _path: Option<&str>) -> Result<Vec<GraphSymbol>> {
        Err(IntelError::Tool(
            "exact_symbols is not supported by this backend".into(),
        ))
    }

    /// BFS over `calls` edges from the given symbol ids. Returns nodes at
    /// depth 1..=`depth` (roots excluded), each symbol once at its minimum
    /// depth, at most 500 nodes.
    async fn trace_ids(
        &self,
        _ids: &[i64],
        _direction: TraceDirection,
        _depth: u32,
    ) -> Result<Vec<TraceNode>> {
        Err(IntelError::Tool(
            "trace_ids is not supported by this backend".into(),
        ))
    }

    /// Apply on-disk changes to the index incrementally.
    async fn refresh(&self) -> Result<()> {
        Err(IntelError::Tool(
            "refresh is not supported by this backend".into(),
        ))
    }

    /// Bind the selected repository for this session without performing a
    /// first-time index. Backends with nothing to bind may leave this default.
    async fn ensure_bound(&self) -> Result<()> {
        Ok(())
    }

    /// Perform an explicit, user-requested index of the selected repository.
    /// Only `kode index` calls this; the default backend does not support it.
    async fn index_repository(&self) -> Result<()> {
        Err(IntelError::Tool(
            "index_repository is not supported by this backend".to_string(),
        ))
    }

    /// Whether the backend applies index updates in the background (watcher
    /// on) so callers can skip their post-task refresh.
    fn watching(&self) -> bool {
        false
    }
}
