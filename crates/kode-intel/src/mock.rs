use crate::CodeIntelligence;
use crate::error::{IntelError, Result};
use crate::types::{CodeContext, CodeContextRequest, CodeSearchResult, FileOutline, IntelHealth};

/// A canned [`CodeIntelligence`] implementation for tests. Defaults to an
/// empty-but-healthy backend; override the fields to script specific
/// responses.
pub struct MockCodeIntelligence {
    pub health: IntelHealth,
    pub context: CodeContext,
    /// When set, `get_context` returns `Err(IntelError::Unavailable(_))`
    /// with this message instead of `Ok(self.context.clone())`.
    pub context_error: Option<String>,
    pub search_results: Vec<CodeSearchResult>,
    pub outline: FileOutline,
}

impl Default for MockCodeIntelligence {
    fn default() -> Self {
        Self {
            health: IntelHealth {
                status: "healthy".to_string(),
                documents: 0,
                symbols: 0,
                edges: 0,
            },
            context: CodeContext {
                text: String::new(),
                token_estimate: 0,
            },
            context_error: None,
            search_results: Vec::new(),
            outline: FileOutline {
                path: String::new(),
                symbols: Vec::new(),
            },
        }
    }
}

#[async_trait::async_trait]
impl CodeIntelligence for MockCodeIntelligence {
    async fn health(&self) -> Result<IntelHealth> {
        Ok(self.health.clone())
    }

    async fn get_context(&self, _request: CodeContextRequest) -> Result<CodeContext> {
        if let Some(message) = &self.context_error {
            return Err(IntelError::Unavailable(message.clone()));
        }
        Ok(self.context.clone())
    }

    async fn search(&self, _query: &str, _limit: u32) -> Result<Vec<CodeSearchResult>> {
        Ok(self.search_results.clone())
    }

    async fn file_outline(&self, _path: &str) -> Result<FileOutline> {
        Ok(self.outline.clone())
    }
}
