use tokio::sync::Mutex;

use crate::EngineeringMemory;
use crate::MemoryStats;
use crate::error::{MemoryError, Result};
use crate::types::{Memory, MemoryQuery, NewMemory};
use crate::{ImportCounts, WireEntry};

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
    /// When set, `import_team` returns this error instead of recording.
    pub import_error: Option<MemoryError>,
    /// Every batch of entries passed to `import_team`, for inspection.
    pub imported: Mutex<Vec<WireEntry>>,
    /// Counts returned by the next `import_team` call (when `import_error`
    /// is `None`).
    pub import_counts: ImportCounts,
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
            import_error: None,
            imported: Mutex::new(Vec::new()),
            import_counts: ImportCounts::default(),
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

    async fn import_team(&self, entries: &[WireEntry]) -> Result<ImportCounts> {
        if let Some(err) = &self.import_error {
            return Err(clone_error(err));
        }
        self.imported.lock().await.extend_from_slice(entries);
        Ok(self.import_counts)
    }
}

/// [`MemoryError`] doesn't derive `Clone` (it wraps a `reqwest`-flavored
/// error family via `thiserror`), so scripted `import_error` values are
/// cloned field-by-field for each call instead.
fn clone_error(err: &MemoryError) -> MemoryError {
    match err {
        MemoryError::Unavailable(m) => MemoryError::Unavailable(m.clone()),
        MemoryError::Service { code, message } => MemoryError::Service {
            code: code.clone(),
            message: message.clone(),
        },
        MemoryError::Protocol(m) => MemoryError::Protocol(m.clone()),
        MemoryError::Timeout => MemoryError::Timeout,
        MemoryError::Unsupported(m) => MemoryError::Unsupported(m.clone()),
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
            team: false,
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

    fn wire_entry() -> WireEntry {
        WireEntry::new(
            &NewMemory {
                kind: MemoryKind::Convention,
                summary: "use rtk".to_string(),
                body: String::new(),
                tags: vec![],
                provenance: Provenance::ExplicitUser,
                context: MemoryContext::default(),
                team: true,
            },
            Some("alice".to_string()),
        )
    }

    #[tokio::test]
    async fn import_team_records_entries_and_returns_scripted_counts() {
        let mock = MockEngineeringMemory {
            import_counts: ImportCounts {
                imported: 1,
                skipped: 0,
            },
            ..Default::default()
        };
        let entry = wire_entry();

        let counts = mock
            .import_team(std::slice::from_ref(&entry))
            .await
            .unwrap();
        assert_eq!(counts.imported, 1);

        let recorded = mock.imported.lock().await;
        assert_eq!(recorded.as_slice(), &[entry]);
    }

    #[tokio::test]
    async fn import_team_error_is_reported() {
        let mock = MockEngineeringMemory {
            import_error: Some(MemoryError::Unsupported("no /import endpoint".to_string())),
            ..Default::default()
        };
        let err = mock.import_team(&[wire_entry()]).await.unwrap_err();
        assert!(matches!(err, MemoryError::Unsupported(_)));
    }
}
