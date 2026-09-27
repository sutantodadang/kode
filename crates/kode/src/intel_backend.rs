//! Bin-level factory that constructs the code-intelligence backend.
//!
//! Code intelligence is native: Kode loads its pinned zindeks shared library
//! in-process. This only *constructs* the backend — it never performs a
//! first-time index (that is `kode index`). Returns `None` only when zindeks
//! is disabled; a library that cannot be loaded is an error, never a silent
//! fallback to another transport.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use kode_core::config::ZindeksConfig;
use kode_intel::{CodeIntelligence, EmbeddedZindeks};

/// Builds the embedded backend, or `None` when zindeks is disabled.
pub async fn connect(
    cfg: &ZindeksConfig,
    root: &Path,
) -> Result<Option<Arc<dyn CodeIntelligence>>> {
    if !cfg.enabled {
        return Ok(None);
    }

    let library = crate::engine_assets::zindeks_library(cfg)?;
    let store_root = crate::engine_assets::store_root(cfg)?;
    Ok(Some(Arc::new(EmbeddedZindeks::open(
        &library,
        root,
        &store_root,
        cfg.watch,
    )?)))
}
