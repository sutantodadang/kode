use std::path::Path;
use std::time::Duration;

use kode_core::KodeConfig;

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
            Err(_) => "ingat: unavailable — timed out".to_string(),
        };
        println!("{line}");
    } else {
        println!("ingat: disabled");
    }

    Ok(())
}

/// Opens the native memory backend and reports health + count.
async fn ingat_status(config: &KodeConfig) -> String {
    let backend = match crate::memory_backend::connect(&config.ingat).await {
        Ok(Some(backend)) => backend,
        Ok(None) => return "ingat: disabled".to_string(),
        Err(err) => return format!("ingat: unavailable — {err}"),
    };

    let unavailable = || "ingat: unavailable — memory store could not be read".to_string();

    if backend.health().await.is_err() {
        return unavailable();
    }

    match backend.stats().await {
        Ok(stats) => format!(
            "ingat: healthy — {} memories (v{})",
            stats.total, stats.version
        ),
        Err(_) => unavailable(),
    }
}

/// Connects to the code-intelligence backend, binds the project (only if
/// already indexed), and reports health. Never auto-indexes an unindexed
/// repository — that requires `kode index`.
async fn zindeks_status(cwd: &Path, config: &KodeConfig) -> String {
    let backend = match crate::intel_backend::connect(&config.zindeks, cwd).await {
        Ok(Some(backend)) => backend,
        Ok(None) => return "zindeks: disabled".to_string(),
        Err(err) => return format!("zindeks: unavailable — {err}"),
    };

    if let Err(err) = backend.ensure_bound().await {
        return match err {
            kode_intel::IntelError::NotIndexed(_) => {
                "zindeks: not indexed — run: kode index".to_string()
            }
            other => format!("zindeks: unavailable — {other}"),
        };
    }

    match backend.health().await {
        Ok(health) => {
            let sqlite = health
                .sqlite_version
                .as_deref()
                .map(|v| format!(", sqlite {v}"))
                .unwrap_or_default();
            format!(
                "zindeks: healthy — {} files, {} symbols indexed{sqlite}",
                health.documents, health.symbols
            )
        }
        Err(err) => format!("zindeks: unavailable — {err}"),
    }
}
