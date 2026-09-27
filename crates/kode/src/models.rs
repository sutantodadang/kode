use std::path::Path;

use kode_core::config::KodeConfig;
use kode_model::catalog;

/// `kode models` — lists the model catalog for the currently configured
/// provider, marking the currently-selected model (if any) with `*`.
pub async fn run(cwd: &Path) -> anyhow::Result<()> {
    let config = KodeConfig::load(cwd)?;
    let provider = config.model.provider.clone();
    let current = config.model.model.clone();

    match catalog::list_catalog(&provider, None).await {
        Ok(catalog) => {
            if let Some(note) = catalog.note {
                println!("({note})");
            }
            let models = catalog.models;
            if models.is_empty() {
                println!("(no models found for provider '{provider}')");
            } else {
                for m in models {
                    if !current.is_empty() && m == current {
                        println!("* {m}");
                    } else {
                        println!("  {m}");
                    }
                }
            }
        }
        Err(e) => {
            println!("(could not fetch models for provider '{provider}': {e})");
        }
    }

    Ok(())
}
