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
}
