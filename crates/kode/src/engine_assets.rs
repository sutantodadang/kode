//! Resolution of the in-process zindeks shared library and its index store.
//!
//! Normal operation never searches `PATH`: the library must be a verified
//! asset installed under `~/.kode/runtime/zindeks/<revision>/`. Development
//! builds can point Kode at a locally built library through the explicit
//! `KODE_ZINDEKS_DYLIB` override, which doctor surfaces as an override.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kode_core::config::ZindeksConfig;

/// Explicit development override for the embedded zindeks library.
pub const DYLIB_ENV: &str = "KODE_ZINDEKS_DYLIB";

/// Platform file name of the zindeks shared library.
pub fn dylib_file_name() -> &'static str {
    if cfg!(windows) {
        "zindeks.dll"
    } else if cfg!(target_os = "macos") {
        "libzindeks.dylib"
    } else {
        "libzindeks.so"
    }
}

/// The `KODE_ZINDEKS_DYLIB` override path, if set.
pub fn override_path() -> Option<PathBuf> {
    std::env::var_os(DYLIB_ENV).map(PathBuf::from)
}

/// Resolves the verified pinned library, or the explicit override.
pub fn zindeks_library(_cfg: &ZindeksConfig) -> Result<PathBuf> {
    if let Some(path) = override_path() {
        if !path.is_file() {
            anyhow::bail!(
                "{DYLIB_ENV} points at {}, which is not a file",
                path.display()
            );
        }
        return Ok(path);
    }

    let root = kode_core::zindeks_runtime_dir()
        .context("cannot determine Kode home for the zindeks library")?;
    let revision = newest_install(&root);
    let path = revision.join(dylib_file_name());
    if !path.is_file() {
        anyhow::bail!(
            "embedded zindeks library not found at {} — run: kode setup",
            path.display()
        );
    }
    Ok(path)
}

/// Resolves the embedded index store root: `[zindeks].store_root` when set,
/// else `~/.kode/zindeks/`.
pub fn store_root(cfg: &ZindeksConfig) -> Result<PathBuf> {
    if let Some(path) = &cfg.store_root {
        return Ok(path.clone());
    }
    kode_core::default_zindeks_store_root()
        .context("cannot determine Kode home for the embedded zindeks store")
}

/// Directory of the newest installed revision under `root`, or `root` itself
/// when nothing is installed yet (so the error names a concrete path).
fn newest_install(root: &Path) -> PathBuf {
    let mut best: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                best = Some(match best {
                    Some(current) if current >= path => current,
                    _ => path,
                });
            }
        }
    }
    best.unwrap_or_else(|| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn dylib_file_name_matches_the_platform() {
        let name = dylib_file_name();
        if cfg!(windows) {
            assert_eq!(name, "zindeks.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libzindeks.dylib");
        } else {
            assert_eq!(name, "libzindeks.so");
        }
    }

    #[test]
    fn store_root_prefers_the_config_override() {
        let cfg = ZindeksConfig {
            store_root: Some(PathBuf::from("/custom/zindeks")),
            ..ZindeksConfig::default()
        };
        assert_eq!(store_root(&cfg).unwrap(), PathBuf::from("/custom/zindeks"));
    }

    #[test]
    fn store_root_defaults_under_kode_home() {
        if kode_core::default_zindeks_store_root().is_none() {
            return; // no HOME/APPDATA in this environment
        }
        let cfg = ZindeksConfig::default();
        let root = store_root(&cfg)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            root.ends_with(".kode/zindeks"),
            "unexpected default store root: {root}"
        );
    }
}
