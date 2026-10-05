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
pub const ZINDEKS_VERSION: &str = "0.10.4";
pub const ZINDEKS_REVISION: &str = "a860491eaee9334869d96e1e25bb341396e99866";

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
    let revision = pinned_install(&root).with_context(|| format!(
        "embedded zindeks v{ZINDEKS_VERSION} ({ZINDEKS_REVISION}) not installed at {} — run: kode setup",
        root.display()
    ))?;
    Ok(revision.join(dylib_file_name()))
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

/// Commit hashes do not sort chronologically. Only load the release this
/// build pins; an older installed asset must not prevent setup from upgrading.
fn pinned_install(root: &Path) -> Option<PathBuf> {
    let path = root.join(ZINDEKS_REVISION);
    let text = std::fs::read_to_string(path.join("metadata.json")).ok()?;
    let metadata: serde_json::Value = serde_json::from_str(&text).ok()?;
    (metadata.get("abi_version")?.as_u64()? == 1
        && metadata.get("zindeks_version")?.as_str()? == ZINDEKS_VERSION
        && metadata.get("source_revision")?.as_str()? == ZINDEKS_REVISION
        && path.join(dylib_file_name()).is_file())
    .then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn pinned_runtime_wins_over_older_lexicographically_larger_revision() {
        let root = std::env::temp_dir().join(format!(
            "kode-runtime-pin-{}",
            kode_local::dataset::new_id()
        ));
        for (revision, version) in [
            ("e79bc89a3b68870448db5c69d4420ec509484959", "0.10.2"),
            (ZINDEKS_REVISION, ZINDEKS_VERSION),
        ] {
            let dir = root.join(revision);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(dylib_file_name()), "fixture").unwrap();
            std::fs::write(
                dir.join("metadata.json"),
                serde_json::json!({
                    "abi_version": 1, "source_revision": revision, "zindeks_version": version,
                })
                .to_string(),
            )
            .unwrap();
        }
        assert_eq!(pinned_install(&root), Some(root.join(ZINDEKS_REVISION)));
        std::fs::remove_dir_all(root.join(ZINDEKS_REVISION)).unwrap();
        assert_eq!(
            pinned_install(&root),
            None,
            "old asset must trigger upgrade"
        );
        let _ = std::fs::remove_dir_all(root);
    }

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
