use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;

const REPO_RELEASES_DOWNLOAD_BASE: &str = "https://github.com/sutantodadang/kode/releases/download";
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/sutantodadang/kode/releases/latest";

/// Runs `kode update`: self-updates the running binary from the latest
/// GitHub release. Consent-gated — nothing is downloaded or installed
/// without an explicit `y` (or `--yes`).
pub async fn run(yes: bool) -> anyhow::Result<()> {
    let current_version = env!("CARGO_PKG_VERSION");
    println!("kode: current version {current_version}");

    let current_exe = std::env::current_exe().context("failed to resolve current exe path")?;

    #[cfg(windows)]
    cleanup_stale_old(&current_exe);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("kode")
        .build()?;

    let release: serde_json::Value = client
        .get(LATEST_RELEASE_API)
        .send()
        .await
        .context("failed to reach GitHub releases API")?
        .error_for_status()
        .context("GitHub releases API returned an error")?
        .json()
        .await
        .context("failed to parse GitHub releases API response")?;

    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("release response missing tag_name"))?;
    let latest_version = tag.trim_start_matches('v');

    if !is_newer(latest_version, current_version) {
        println!("kode: already up to date ({current_version})");
        return Ok(());
    }

    let target = current_target().ok_or_else(|| {
        anyhow::anyhow!(
            "unsupported platform for self-update: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let asset = asset_name(target);

    if !confirm(&format!("update to v{latest_version}?"), yes).await {
        println!("kode: update cancelled");
        return Ok(());
    }

    let tmp_dir = std::env::temp_dir().join(format!("kode-update-{}", std::process::id()));
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .with_context(|| format!("failed to create {}", tmp_dir.display()))?;

    let result = do_update(
        &client,
        tag,
        &asset,
        &tmp_dir,
        &current_exe,
        latest_version,
        current_version,
    )
    .await;

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

    result
}

#[allow(clippy::too_many_arguments)]
async fn do_update(
    client: &reqwest::Client,
    tag: &str,
    asset: &str,
    tmp_dir: &Path,
    current_exe: &Path,
    latest_version: &str,
    current_version: &str,
) -> anyhow::Result<()> {
    let asset_url = format!("{REPO_RELEASES_DOWNLOAD_BASE}/{tag}/{asset}");
    let sidecar_url = format!("{asset_url}.sha256");

    let archive_bytes = client
        .get(&asset_url)
        .send()
        .await
        .with_context(|| format!("failed to download {asset_url}"))?
        .error_for_status()
        .with_context(|| format!("download failed: {asset_url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read response body for {asset_url}"))?;

    let sidecar_text = client
        .get(&sidecar_url)
        .send()
        .await
        .with_context(|| format!("failed to download {sidecar_url}"))?
        .error_for_status()
        .with_context(|| format!("download failed: {sidecar_url}"))?
        .text()
        .await
        .with_context(|| format!("failed to read response body for {sidecar_url}"))?;

    if !verify_sha256(&archive_bytes, &sidecar_text) {
        anyhow::bail!("kode: checksum mismatch for {asset} — aborting update");
    }

    let archive_path = tmp_dir.join(asset);
    tokio::fs::write(&archive_path, &archive_bytes)
        .await
        .with_context(|| format!("failed to write {}", archive_path.display()))?;

    let output = tokio::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(tmp_dir)
        .output()
        .await
        .context(
            "kode: `tar` not found or failed to run — install the release manually from \
             https://github.com/sutantodadang/kode/releases",
        )?;
    if !output.status.success() {
        anyhow::bail!(
            "kode: tar extraction failed ({}) — install manually from \
             https://github.com/sutantodadang/kode/releases",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let exe_name = if cfg!(windows) { "kode.exe" } else { "kode" };
    let new_exe = tmp_dir.join(exe_name);
    if !new_exe.exists() {
        anyhow::bail!("kode: extracted archive did not contain {exe_name}");
    }

    replace_binary(current_exe, &new_exe)?;

    println!("kode: updated {current_version} -> {latest_version}. restart kode to use it.");
    Ok(())
}

/// Prompts the user on stderr and reads a `y`/`yes` answer from stdin.
/// `--yes` short-circuits to `true` without prompting.
async fn confirm(prompt: &str, yes: bool) -> bool {
    if yes {
        return true;
    }
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let answer = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line
    })
    .await
    .unwrap_or_default();
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Maps the compile-time target_os/target_arch to the matching release
/// target triple. `None` for unsupported platforms.
fn current_target() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// Release asset filename for a given target triple.
fn asset_name(target: &str) -> String {
    if target.contains("windows") {
        format!("kode-{target}.zip")
    } else {
        format!("kode-{target}.tar.gz")
    }
}

/// Compares dot-separated numeric version triples. Non-numeric or missing
/// segments compare as `0`.
fn is_newer(latest: &str, current: &str) -> bool {
    parse_triple(latest) > parse_triple(current)
}

fn parse_triple(v: &str) -> (u64, u64, u64) {
    let mut parts = v.split('.').map(|s| s.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Verifies `bytes` hashes to the digest named by the first whitespace-
/// delimited token of `sidecar_text` (case-insensitive hex compare).
fn verify_sha256(bytes: &[u8], sidecar_text: &str) -> bool {
    let expected = match sidecar_text.split_whitespace().next() {
        Some(tok) => tok,
        None => return false,
    };
    sha256_hex(bytes).eq_ignore_ascii_case(expected)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(windows)]
fn windows_old_path(current_exe: &Path) -> std::path::PathBuf {
    let mut old_name = current_exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("kode.exe"));
    old_name.push(".old");
    current_exe.with_file_name(old_name)
}

/// Removes a leftover `<exe>.old` from a previous update, if present. Best
/// effort — the file is only ever in the way, never load-bearing.
#[cfg(windows)]
fn cleanup_stale_old(current_exe: &Path) {
    let old_path = windows_old_path(current_exe);
    let _ = std::fs::remove_file(&old_path);
}

#[cfg(windows)]
fn replace_binary(current_exe: &Path, new_exe: &Path) -> anyhow::Result<()> {
    let old_path = windows_old_path(current_exe);

    // Delete any stale `.old` first (best effort; cleanup_stale_old already
    // tried this at the start of the command).
    let _ = std::fs::remove_file(&old_path);

    std::fs::rename(current_exe, &old_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            current_exe.display(),
            old_path.display()
        )
    })?;

    std::fs::copy(new_exe, current_exe)
        .with_context(|| format!("failed to install new binary at {}", current_exe.display()))?;

    // Best-effort cleanup of the `.old` we just created — it's locked while
    // this process is running, so failure here is expected and ignored.
    // It'll be swept up by cleanup_stale_old() on the next `kode update`.
    let _ = std::fs::remove_file(&old_path);

    Ok(())
}

#[cfg(not(windows))]
fn replace_binary(current_exe: &Path, new_exe: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let exe_file_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("kode");
    let staging = current_exe.with_file_name(format!("{exe_file_name}.new-{}", std::process::id()));

    std::fs::copy(new_exe, &staging)
        .with_context(|| format!("failed to stage new binary at {}", staging.display()))?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to set permissions on {}", staging.display()))?;
    std::fs::rename(&staging, current_exe)
        .with_context(|| format!("failed to install new binary at {}", current_exe.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_true_when_greater() {
        assert!(is_newer("0.2.0", "0.1.0"));
    }

    #[test]
    fn is_newer_false_when_older() {
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn is_newer_false_when_equal() {
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn is_newer_handles_multi_digit_segments() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.9.9", "0.10.0"));
    }

    #[test]
    fn is_newer_treats_missing_segments_as_zero() {
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("1", "1.0.1"));
    }

    #[test]
    fn current_target_matches_running_platform() {
        let target = current_target();
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        assert_eq!(target, Some("x86_64-pc-windows-msvc"));
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(target, Some("x86_64-unknown-linux-gnu"));
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        assert_eq!(target, Some("aarch64-unknown-linux-gnu"));
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        assert_eq!(target, Some("x86_64-apple-darwin"));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(target, Some("aarch64-apple-darwin"));
    }

    #[test]
    fn asset_name_uses_zip_for_windows() {
        assert_eq!(
            asset_name("x86_64-pc-windows-msvc"),
            "kode-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn asset_name_uses_tar_gz_for_unix() {
        assert_eq!(
            asset_name("x86_64-unknown-linux-gnu"),
            "kode-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("aarch64-apple-darwin"),
            "kode-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn verify_sha256_matches_known_vector() {
        let hex = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(
            b"hello",
            &format!("{hex}  kode-archive.tar.gz")
        ));
    }

    #[test]
    fn verify_sha256_mismatch_returns_false() {
        let hex = "deadbeef00000000000000000000000000000000000000000000000000000000";
        assert!(!verify_sha256(
            b"hello",
            &format!("{hex}  kode-archive.tar.gz")
        ));
    }

    #[test]
    fn verify_sha256_accepts_uppercase_hex() {
        let hex = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_sha256(
            b"hello",
            &format!("{hex}  kode-archive.tar.gz")
        ));
    }
}
