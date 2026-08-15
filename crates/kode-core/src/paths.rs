use std::path::PathBuf;

/// Directory Kode's `setup` installs managed engine binaries (zindeks) into.
///
/// Windows: `%LOCALAPPDATA%\kode\bin`. Elsewhere: `$HOME/.kode/bin`.
/// Returns `None` when the relevant environment variable isn't set.
pub fn managed_bin_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        let local_app_data = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(local_app_data).join("kode").join("bin"))
    } else {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".kode").join("bin"))
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_bin_dir_ends_with_expected_suffix() {
        let dir = managed_bin_dir().expect("environment should provide a home/appdata dir");
        let display = dir.to_string_lossy().replace('\\', "/");
        assert!(
            display.ends_with("kode/bin"),
            "unexpected managed bin dir: {display}"
        );
    }

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
