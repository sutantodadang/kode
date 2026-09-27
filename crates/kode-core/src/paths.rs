use std::path::PathBuf;

/// Kode's own home directory: `$USERPROFILE/.kode` (or `$HOME/.kode`
/// elsewhere). Returns `None` when neither environment variable is set.
pub fn kode_home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".kode"))
}

/// Kode's own credential store directory: `kode_home_dir()/auth`. This is
/// where `kode auth login` writes `codex.json` / `opencode.json` — Kode
/// never reads other tools' auth files (`~/.codex`, opencode's data dir).
pub fn auth_dir() -> Option<PathBuf> {
    Some(kode_home_dir()?.join("auth"))
}

/// Root for verified shared-engine assets Kode downloads (e.g. the pinned
/// zindeks shared library): `kode_home_dir()/runtime`.
pub fn kode_runtime_dir() -> Option<PathBuf> {
    Some(kode_home_dir()?.join("runtime"))
}

/// Parent for pinned zindeks shared libraries: `kode_runtime_dir()/zindeks`.
/// Each revision installs under its own `<revision>/` subdirectory.
pub fn zindeks_runtime_dir() -> Option<PathBuf> {
    Some(kode_runtime_dir()?.join("zindeks"))
}

/// Default in-process zindeks index store root: `kode_home_dir()/zindeks`.
/// Distinct from any standalone zindeks index so embedded writes never touch
/// a user's existing external index.
pub fn default_zindeks_store_root() -> Option<PathBuf> {
    Some(kode_home_dir()?.join("zindeks"))
}

/// Default embedded Ingat memory database:
/// `kode_home_dir()/ingat/memory.sqlite3`. Kode-owned; never the desktop or
/// service's live store.
pub fn default_ingat_store_path() -> Option<PathBuf> {
    Some(kode_home_dir()?.join("ingat").join("memory.sqlite3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kode_home_dir_ends_with_expected_suffix() {
        let dir = kode_home_dir().expect("environment should provide a home dir");
        let display = dir.to_string_lossy().replace('\\', "/");
        assert!(
            display.ends_with(".kode"),
            "unexpected kode home: {display}"
        );
    }

    #[test]
    fn auth_dir_ends_with_expected_suffix() {
        let dir = auth_dir().expect("environment should provide a home dir");
        let display = dir.to_string_lossy().replace('\\', "/");
        assert!(
            display.ends_with(".kode/auth"),
            "unexpected auth dir: {display}"
        );
    }
}
