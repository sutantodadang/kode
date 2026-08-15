pub mod error;
pub mod mock;
pub mod types;
pub mod zindeks;

pub use error::{IntelError, Result};
pub use mock::MockCodeIntelligence;
pub use types::{
    CodeContext, CodeContextRequest, CodeSearchResult, FileOutline, IntelHealth, OutlineSymbol,
};
pub use zindeks::ZindeksAdapter;

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
}
