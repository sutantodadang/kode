use std::path::Path;
use std::time::Duration;

use kode_core::KodeConfig;
use kode_intel::{CodeIntelligence, ZindeksAdapter};
use kode_memory::{EngineeringMemory, IngatAdapter};

/// Prints Kode's current status to stdout. Always succeeds unless config I/O
/// fails unexpectedly.
pub async fn run(cwd: &Path) -> anyhow::Result<()> {
    println!("Kode v{}", env!("CARGO_PKG_VERSION"));
    println!("working directory: {}", cwd.display());

    let git_dir = cwd.join(".git");
    println!(
        "git repository: {}",
        if git_dir.is_dir() { "yes" } else { "no" }
    );

    let config_path = KodeConfig::config_path(cwd);
    let config = match KodeConfig::load(cwd) {
        Ok(cfg) => {
            if config_path.is_file() {
                println!("config: {} (loaded)", config_path.display());
            } else {
                println!("config: defaults (no .kode/config.toml)");
            }
            cfg
        }
        Err(err) => {
            println!("config: error loading {}: {err}", config_path.display());
            KodeConfig::default()
        }
    };

    let model_name = if config.model.model.is_empty() {
        "(unset)"
    } else {
        config.model.model.as_str()
    };
    println!(
        "model: provider={} model={model_name}",
        config.model.provider
    );

    if config.zindeks.enabled {
        let line = match tokio::time::timeout(Duration::from_secs(10), zindeks_status(cwd, &config))
            .await
        {
            Ok(line) => line,
            Err(_) => "zindeks: unavailable — timed out — run: kode setup".to_string(),
        };
        println!("{line}");
    } else {
        println!("zindeks: disabled");
    }

    if config.ingat.enabled {
        let line = match tokio::time::timeout(Duration::from_secs(5), ingat_status(&config)).await {
            Ok(line) => line,
            Err(_) => format!(
                "ingat: unavailable — run: kode setup (or start the Ingat app) [{}]",
                config.ingat.url
            ),
        };
        println!("{line}");
    } else {
        println!("ingat: disabled");
    }

    Ok(())
}

/// Connects to the Ingat REST service and reports health + memory count.
/// Never touches Ingat's storage directly — REST only.
async fn ingat_status(config: &KodeConfig) -> String {
    let adapter = IngatAdapter::new(&config.ingat);
    let unavailable = || {
        format!(
            "ingat: unavailable — run: kode setup (or start the Ingat app) [{}]",
            config.ingat.url
        )
    };

    if adapter.health().await.is_err() {
        return unavailable();
    }

    match adapter.stats().await {
        Ok(stats) => format!(
            "ingat: healthy — {} memories (v{})",
            stats.total, stats.version
        ),
        Err(_) => unavailable(),
    }
}

/// Connects to zindeks, binds the project (only if already indexed), and
/// reports health. Never auto-indexes an unindexed repository — that
/// requires the user to explicitly run `zindeks index .`.
async fn zindeks_status(cwd: &Path, config: &KodeConfig) -> String {
    let adapter = match ZindeksAdapter::connect(&config.zindeks, cwd).await {
        Ok(adapter) => adapter,
        Err(err) => return format!("zindeks: unavailable — {err} — run: kode setup"),
    };

    if let Err(err) = adapter.ensure_bound().await {
        return match err {
            kode_intel::IntelError::NotIndexed(_) => {
                "zindeks: not indexed — run: zindeks index .".to_string()
            }
            other => format!("zindeks: unavailable — {other} — run: kode setup"),
        };
    }

    match adapter.health().await {
        Ok(health) => format!(
            "zindeks: healthy — {} files, {} symbols indexed",
            health.documents, health.symbols
        ),
        Err(err) => format!("zindeks: unavailable — {err} — run: kode setup"),
    }
}
