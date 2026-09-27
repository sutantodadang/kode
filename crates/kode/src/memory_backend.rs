//! Bin-level factory that constructs the engineering-memory backend.
//!
//! Memory is native: Kode opens its own in-process SQLite store at
//! `[ingat].store_path` (or `~/.kode/ingat/memory.sqlite3`). Returns `None`
//! only when memory is disabled; a store that cannot be opened is an error,
//! never a silent fallback.

use std::sync::Arc;

use anyhow::{Context, Result};
use kode_core::config::IngatConfig;
use kode_memory::{EmbeddedIngat, EngineeringMemory};

/// Opens the native memory backend, or `None` when disabled.
pub async fn connect(cfg: &IngatConfig) -> Result<Option<Arc<dyn EngineeringMemory>>> {
    if !cfg.enabled {
        return Ok(None);
    }

    let path = match &cfg.store_path {
        Some(path) => path.clone(),
        None => kode_core::default_ingat_store_path()
            .context("cannot determine Kode home for the memory store")?,
    };

    Ok(Some(Arc::new(EmbeddedIngat::open(&path)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_backend_opens_nothing() {
        let cfg = IngatConfig {
            enabled: false,
            ..IngatConfig::default()
        };
        assert!(connect(&cfg).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn embedded_backend_opens_store() {
        let path = std::env::temp_dir().join(format!(
            "kode-mem-backend-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cfg = IngatConfig {
            store_path: Some(path.clone()),
            ..IngatConfig::default()
        };
        assert!(connect(&cfg).await.unwrap().is_some());
        let _ = std::fs::remove_file(&path);
    }
}
