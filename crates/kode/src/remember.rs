use std::path::Path;

use kode_core::config::KodeConfig;
use kode_memory::{
    EngineeringMemory, IngatAdapter, MemoryContext, MemoryKind, NewMemory, Provenance,
};

/// Runs `kode remember <text> [--kind <kind>] [--tag <tag> ...]`.
///
/// Explicit user memory is written directly to Ingat with no permission
/// prompt (per product spec §31) — the user asked for it by name.
pub async fn run(text: &str, kind: &str, tags: Vec<String>, cwd: &Path) -> anyhow::Result<()> {
    let kind = MemoryKind::from_kebab(kind).ok_or_else(|| {
        let valid: Vec<&str> = MemoryKind::ALL.iter().map(MemoryKind::as_kebab).collect();
        anyhow::anyhow!(
            "invalid --kind {kind:?}; valid values: {}",
            valid.join(", ")
        )
    })?;

    let config = KodeConfig::load(cwd)?;
    if !config.ingat.enabled {
        anyhow::bail!("ingat disabled in config");
    }

    let context = gather_context(cwd).await;
    let summary = truncate_summary(text, 100);

    let memory = NewMemory {
        kind,
        summary,
        body: text.to_string(),
        tags,
        provenance: Provenance::ExplicitUser,
        context,
    };

    let adapter = IngatAdapter::new(&config.ingat);
    match adapter.remember(&memory).await {
        Ok(id) => {
            println!("remembered ({}): {id}", kind.as_kebab());
            Ok(())
        }
        Err(kode_memory::MemoryError::Unavailable(_)) => {
            anyhow::bail!(
                "ingat unavailable — start the Ingat service (mcp-service) on {}",
                config.ingat.url
            )
        }
        Err(err) => Err(anyhow::anyhow!(err)),
    }
}

/// Truncates `text` to at most `max_chars` characters (not bytes), safe on
/// any UTF-8 boundary.
fn truncate_summary(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Best-effort repository/branch/commit context: repository name from the
/// cwd directory name, branch/commit from `git`. Any failure (not a git
/// repo, git missing) silently yields `None` for that field — this is
/// metadata, not something worth failing `remember` over.
async fn gather_context(cwd: &Path) -> MemoryContext {
    let repository = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string());
    let branch = git_output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let commit = git_output(cwd, &["rev-parse", "--short", "HEAD"]).await;

    MemoryContext {
        repository,
        branch,
        commit,
        files: Vec::new(),
        symbols: Vec::new(),
    }
}

async fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}
