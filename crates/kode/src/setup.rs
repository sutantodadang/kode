use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kode_core::config::{IngatConfig, KodeConfig, ZindeksConfig};
use kode_memory::{EngineeringMemory, IngatAdapter};

const ZINDEKS_RELEASES_BASE: &str =
    "https://github.com/sutantodadang/zindeks/releases/latest/download";
const INGAT_RELEASES_URL: &str = "https://github.com/sutantodadang/Ingat/releases";
const INGAT_LATEST_API: &str = "https://api.github.com/repos/sutantodadang/Ingat/releases/latest";

/// Runs `kode setup`: a consent-gated installer/bootstrapper for the two
/// engines Kode leans on (zindeks for code intelligence, Ingat for
/// engineering memory). Never downloads or installs anything without an
/// explicit `y` (or `--yes`).
pub async fn run(yes: bool, cwd: &Path) -> anyhow::Result<()> {
    let config = KodeConfig::load(cwd)?;

    setup_zindeks(&config.zindeks, yes).await?;
    setup_ingat(&config.ingat, yes).await?;

    println!("setup complete — run: kode status");
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

// --- zindeks -----------------------------------------------------------

async fn setup_zindeks(cfg: &ZindeksConfig, yes: bool) -> anyhow::Result<()> {
    if let Some(version) = probe_version(&cfg.command).await {
        println!("zindeks: found ({version})");
        return Ok(());
    }

    let managed_dir = kode_core::managed_bin_dir().ok_or_else(|| {
        anyhow::anyhow!("cannot determine managed bin dir (no HOME/LOCALAPPDATA set)")
    })?;
    let managed_bin = managed_dir.join(if cfg!(windows) {
        "zindeks.exe"
    } else {
        "zindeks"
    });

    if let Some(version) = probe_version(&managed_bin).await {
        println!("zindeks: found ({version})");
        return Ok(());
    }

    if !confirm(
        &format!("install zindeks (latest) to {}?", managed_dir.display()),
        yes,
    )
    .await
    {
        println!(
            "zindeks: skipped — install manually or set [zindeks] command in .kode/config.toml"
        );
        return Ok(());
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let asset = zindeks_asset(os, arch)
        .ok_or_else(|| anyhow::anyhow!("unsupported platform for zindeks: {os}/{arch}"))?;

    let tmp_dir = std::env::temp_dir().join(format!("kode-setup-{}", std::process::id()));
    tokio::fs::create_dir_all(&tmp_dir).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    let asset_url = format!("{ZINDEKS_RELEASES_BASE}/{asset}");
    let sums_url = format!("{ZINDEKS_RELEASES_BASE}/SHA256SUMS");

    let archive_bytes = client
        .get(&asset_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let sums_text = client
        .get(&sums_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let expected_hex = find_sha256(&sums_text, asset)
        .ok_or_else(|| anyhow::anyhow!("SHA256SUMS has no entry for {asset}"))?;
    let actual_hex = sha256_hex(&archive_bytes);
    if !actual_hex.eq_ignore_ascii_case(&expected_hex) {
        anyhow::bail!("checksum mismatch — aborting install");
    }

    let archive_path = tmp_dir.join(asset);
    tokio::fs::write(&archive_path, &archive_bytes).await?;

    tokio::fs::create_dir_all(&managed_dir).await?;
    let output = tokio::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&managed_dir)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "tar extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    match probe_version(&managed_bin).await {
        Some(version) => {
            println!("zindeks: installed ({version})");
            Ok(())
        }
        None => anyhow::bail!("zindeks installed but `--version` failed to run"),
    }
}

/// Runs `cmd --version` with a 5s timeout; returns the first line of stdout
/// on success, `None` on any failure (not found, timed out, nonzero exit).
pub(crate) async fn probe_version(cmd: impl AsRef<std::ffi::OsStr>) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(cmd).arg("--version").output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

/// Maps (os, arch) — as reported by `std::env::consts::{OS,ARCH}` — to the
/// matching zindeks release asset name. `None` for unsupported platforms.
fn zindeks_asset(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("zindeks-windows-x86_64.zip"),
        ("windows", "aarch64") => Some("zindeks-windows-aarch64.zip"),
        ("macos", "x86_64") => Some("zindeks-macos-x86_64.tar.gz"),
        ("macos", "aarch64") => Some("zindeks-macos-aarch64.tar.gz"),
        ("linux", "x86_64") => Some("zindeks-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Some("zindeks-linux-aarch64.tar.gz"),
        _ => None,
    }
}

/// Finds the hex digest for `asset_name` in a `SHA256SUMS` file (standard
/// `<hex>  <filename>` format, optionally with a leading `*` on the
/// filename for binary mode).
fn find_sha256(sums_text: &str, asset_name: &str) -> Option<String> {
    for line in sums_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let hex = parts.next()?;
        let filename = parts.next().unwrap_or("").trim().trim_start_matches('*');
        if filename == asset_name || filename.ends_with(asset_name) {
            return Some(hex.to_lowercase());
        }
    }
    None
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

// --- Ingat ---------------------------------------------------------------

async fn setup_ingat(cfg: &IngatConfig, yes: bool) -> anyhow::Result<()> {
    let adapter = IngatAdapter::new(cfg);
    let healthy = matches!(
        tokio::time::timeout(Duration::from_secs(3), adapter.health()).await,
        Ok(Ok(()))
    );
    if healthy {
        println!("ingat: service running");
        return Ok(());
    }

    #[cfg(windows)]
    {
        windows_setup_ingat(cfg, yes).await
    }

    #[cfg(not(windows))]
    {
        let _ = yes;
        println!("ingat: unavailable — install manually: {INGAT_RELEASES_URL}");
        Ok(())
    }
}

#[cfg(windows)]
async fn windows_setup_ingat(cfg: &IngatConfig, yes: bool) -> anyhow::Result<()> {
    if let Some(path) = find_mcp_service() {
        try_start_ingat(cfg, &path, yes).await;
        return Ok(());
    }

    if !confirm("download and run the Ingat installer (GUI, ~20MB)?", yes).await {
        println!("ingat: skipped — install manually: {INGAT_RELEASES_URL}");
        return Ok(());
    }

    install_ingat().await?;

    match find_mcp_service() {
        Some(path) => try_start_ingat(cfg, &path, yes).await,
        None => println!(
            "ingat: installer finished but mcp_service.exe was not found — start Ingat manually"
        ),
    }
    Ok(())
}

#[cfg(windows)]
async fn try_start_ingat(cfg: &IngatConfig, path: &Path, yes: bool) {
    if !confirm(&format!("start Ingat service ({})?", path.display()), yes).await {
        println!("ingat: skipped — start it manually or set [ingat] url in .kode/config.toml");
        return;
    }

    match spawn_detached(path) {
        Ok(()) => {
            if wait_for_health(cfg, Duration::from_secs(10)).await {
                println!("ingat: service running");
            } else {
                println!("ingat: started but not healthy yet — check Ingat logs");
            }
        }
        Err(e) => println!("ingat: failed to start service: {e}"),
    }
}

async fn wait_for_health(cfg: &IngatConfig, budget: Duration) -> bool {
    let adapter = IngatAdapter::new(cfg);
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Ok(Ok(())) = tokio::time::timeout(Duration::from_secs(2), adapter.health()).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Searches `%LOCALAPPDATA%\Programs\*ingat*` (case-insensitive) up to
/// depth 3 for `mcp_service.exe`.
#[cfg(windows)]
fn find_mcp_service() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let programs_dir = PathBuf::from(local_app_data).join("Programs");
    let entries = std::fs::read_dir(&programs_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        if !name.contains("ingat") {
            continue;
        }
        if let Some(found) = find_file_named(&path, "mcp_service.exe", 3) {
            return Some(found);
        }
    }
    None
}

/// Downloads and launches the Ingat NSIS installer (interactive; waits for
/// the user to click through it).
#[cfg(windows)]
async fn install_ingat() -> anyhow::Result<()> {
    let api_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let release: serde_json::Value = api_client
        .get(INGAT_LATEST_API)
        .header("User-Agent", "kode-setup")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let (name, url) = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|a| {
            let name = a.get("name")?.as_str()?;
            if name.ends_with("-setup.exe") {
                let url = a.get("browser_download_url")?.as_str()?;
                Some((name.to_string(), url.to_string()))
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("no *-setup.exe asset found in latest Ingat release"))?;

    let dl_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let bytes = dl_client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let tmp_dir = std::env::temp_dir().join(format!("kode-setup-{}", std::process::id()));
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let installer_path = tmp_dir.join(&name);
    tokio::fs::write(&installer_path, &bytes).await?;

    println!("ingat: launching installer — follow the on-screen steps");
    let status = tokio::process::Command::new(&installer_path)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("Ingat installer exited with {status}");
    }
    Ok(())
}

#[cfg(windows)]
fn spawn_detached(path: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NO_WINDOW
    const CREATION_FLAGS: u32 = 0x0800_0008;
    std::process::Command::new(path)
        .creation_flags(CREATION_FLAGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Depth-limited search for a file named `filename` (case-insensitive)
/// under `root`. `max_depth` is measured in directories descended below
/// `root` (0 = only `root` itself).
fn find_file_named(root: &Path, filename: &str, max_depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if path
                .file_name()
                .map(|n| n.to_string_lossy().eq_ignore_ascii_case(filename))
                .unwrap_or(false)
            {
                return Some(path);
            }
        } else if path.is_dir() {
            subdirs.push(path);
        }
    }
    if max_depth == 0 {
        return None;
    }
    for subdir in subdirs {
        if let Some(found) = find_file_named(&subdir, filename, max_depth - 1) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zindeks_asset_covers_all_supported_platforms() {
        assert_eq!(
            zindeks_asset("windows", "x86_64"),
            Some("zindeks-windows-x86_64.zip")
        );
        assert_eq!(
            zindeks_asset("windows", "aarch64"),
            Some("zindeks-windows-aarch64.zip")
        );
        assert_eq!(
            zindeks_asset("macos", "x86_64"),
            Some("zindeks-macos-x86_64.tar.gz")
        );
        assert_eq!(
            zindeks_asset("macos", "aarch64"),
            Some("zindeks-macos-aarch64.tar.gz")
        );
        assert_eq!(
            zindeks_asset("linux", "x86_64"),
            Some("zindeks-linux-x86_64.tar.gz")
        );
        assert_eq!(
            zindeks_asset("linux", "aarch64"),
            Some("zindeks-linux-aarch64.tar.gz")
        );
    }

    #[test]
    fn zindeks_asset_rejects_unsupported_platform() {
        assert_eq!(zindeks_asset("freebsd", "x86_64"), None);
        assert_eq!(zindeks_asset("windows", "arm"), None);
    }

    #[test]
    fn find_sha256_matches_known_vector() {
        let sums = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  hello.txt\n\
                     deadbeef00000000000000000000000000000000000000000000000000000000  other.zip\n";
        assert_eq!(
            find_sha256(sums, "hello.txt"),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string())
        );
    }

    #[test]
    fn find_sha256_missing_entry_returns_none() {
        let sums = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  hello.txt\n";
        assert_eq!(find_sha256(sums, "nope.zip"), None);
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-setup-test-{label}-{}-{}",
            std::process::id(),
            nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_file_named_locates_nested_file_within_depth() {
        let root = temp_test_dir("found");
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("mcp_service.exe"), b"stub").unwrap();

        let found = find_file_named(&root, "mcp_service.exe", 3);
        assert_eq!(found, Some(nested.join("mcp_service.exe")));
    }

    #[test]
    fn find_file_named_respects_max_depth() {
        let root = temp_test_dir("toodeep");
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("mcp_service.exe"), b"stub").unwrap();

        // mcp_service.exe is 3 dirs below root; max_depth 1 can't reach it.
        let found = find_file_named(&root, "mcp_service.exe", 1);
        assert_eq!(found, None);
    }

    #[test]
    fn find_file_named_not_found_returns_none() {
        let root = temp_test_dir("missing");
        std::fs::create_dir_all(root.join("a")).unwrap();

        assert_eq!(find_file_named(&root, "mcp_service.exe", 3), None);
    }
}
