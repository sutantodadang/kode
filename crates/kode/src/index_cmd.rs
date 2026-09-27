//! `kode index`: the explicit, user-requested first-time (or refresh) index of
//! the current repository in the selected code-intelligence backend.
//!
//! Task startup never triggers a first index on its own; this command is the
//! only entry point that does.

use std::path::Path;

use anyhow::Result;
use kode_core::config::KodeConfig;

/// Indexes `root` in the configured backend and prints the resulting counts.
pub async fn run(root: &Path) -> Result<()> {
    let config = KodeConfig::load(root)?;

    if !config.zindeks.enabled {
        anyhow::bail!(
            "zindeks is disabled in config — enable it under [zindeks] to index this repository"
        );
    }

    let backend = crate::intel_backend::connect(&config.zindeks, root)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no code-intelligence backend is enabled"))?;

    backend.index_repository().await?;

    match backend.health().await {
        Ok(health) => println!(
            "indexed {} — {} files, {} symbols, {} edges",
            root.display(),
            health.documents,
            health.symbols,
            health.edges
        ),
        Err(_) => println!("indexed {}", root.display()),
    }

    Ok(())
}
