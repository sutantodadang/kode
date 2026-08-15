use tokio::sync::Mutex;

use crate::EngineeringMemory;
use crate::MemoryStats;
use crate::error::{MemoryError, Result};
use crate::types::{Memory, MemoryQuery, NewMemory};

/// A canned [`EngineeringMemory`] implementation for tests. Defaults to an
/// empty-but-healthy backend; override the fields to script specific
/// responses. Every [`NewMemory`] passed to `remember` is recorded for
/// later inspection.
pub struct MockEngineeringMemory {
    pub health_error: Option<String>,
    pub search_results: Vec<Memory>,
    pub search_error: Option<String>,
    pub stats: MemoryStats,
    pub remembered: Mutex<Vec<NewMemory>>,
    /// The id returned by the next `remember` call.
    pub next_id: String,
    pub remember_error: Option<String>,
}

impl Default for MockEngineeringMemory {
    fn default() -> Self {
        Self {
            health_error: None,
            search_results: Vec::new(),
            search_error: None,
            stats: MemoryStats {
                total: 0,
                version: "mock".to_string(),
            },
            remembered: Mutex::new(Vec::new()),
            next_id: "mock-id".to_string(),
            remember_error: None,
        }
    }
}

impl MockEngineeringMemory {
    /// Snapshot of all [`NewMemory`] values passed to `remember` so far.
    pub async fn remembered_snapshot(&self) -> Vec<NewMemory> {
        self.remembered.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl EngineeringMemory for MockEngineeringMemory {
    async fn health(&self) -> Result<()> {
        match &self.health_error {
            Some(message) => Err(MemoryError::Unavailable(message.clone())),
            None => Ok(()),
        }
    }

    async fn search(&self, _query: &MemoryQuery) -> Result<Vec<Memory>> {
        match &self.search_error {
            Some(message) => Err(MemoryError::Unavailable(message.clone())),
            None => Ok(self.search_results.clone()),
        }
    }

    async fn remember(&self, memory: &NewMemory) -> Result<String> {
        if let Some(message) = &self.remember_error {
            return Err(MemoryError::Unavailable(message.clone()));
        }
        self.remembered.lock().await.push(memory.clone());
        Ok(self.next_id.clone())
    }

    async fn stats(&self) -> Result<MemoryStats> {
        Ok(self.stats.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryContext, MemoryKind, Provenance};

    #[tokio::test]
    async fn remember_records_memory_and_returns_next_id() {
        let mock = MockEngineeringMemory::default();
        let memory = NewMemory {
            kind: MemoryKind::ProjectRule,
            summary: "s".to_string(),
            body: "b".to_string(),
            tags: vec![],
            provenance: Provenance::ExplicitUser,
            context: MemoryContext::default(),
        };

        let id = mock.remember(&memory).await.unwrap();
        assert_eq!(id, "mock-id");

        let recorded = mock.remembered_snapshot().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], memory);
    }

    #[tokio::test]
    async fn health_error_is_reported() {
        let mock = MockEngineeringMemory {
            health_error: Some("down".to_string()),
            ..Default::default()
        };
        let err = mock.health().await.unwrap_err();
        assert!(matches!(err, MemoryError::Unavailable(_)));
    }
}
