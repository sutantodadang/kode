use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kode_core::config::{IngatConfig, KodeConfig, RouterConfig, ZindeksConfig};
use kode_local::manifest::{
    ManifestKind, Source, checkpoint_dir, read_manifest, verify_checkpoint,
};
use kode_local::models::{
    LocalPaths, download_verified, install_models, install_runtime, model_url, select_runtime,
};
use kode_local::pins::{MODEL_FILES, MODELS_REPO, MODELS_REVISION, ORT_VERSION, RUNTIMES};

const ZINDEKS_RELEASES_BASE: &str = "https://github.com/sutantodadang/zindeks/releases/download";

/// Runs `kode setup`: a consent-gated installer/bootstrapper for Kode's
/// engines (zindeks for code intelligence, native Ingat for engineering
/// memory). Never downloads or installs anything without an explicit `y`
/// (or `--yes`).
pub async fn run(yes: bool, cwd: &Path) -> anyhow::Result<()> {
    let config = KodeConfig::load(cwd)?;

    setup_zindeks(&config.zindeks, yes).await?;
    setup_ingat(&config.ingat, yes).await?;
    setup_local(&config.router, yes).await?;
    setup_team_model(cwd, yes).await?;

    println!("setup complete — run: kode status");
    Ok(())
}

// --- team router model --------------------------------------------------

/// The HF token for private team repos: `HF_TOKEN`, else the `hf` CLI's
/// stored login. Never printed.
pub(crate) fn hf_token() -> Option<String> {
    if let Ok(t) = std::env::var("HF_TOKEN")
        && !t.trim().is_empty()
    {
        return Some(t.trim().to_string());
    }
    let out = std::process::Command::new("hf")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(str::to_string)
}

pub(crate) fn hf_client() -> anyhow::Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = hf_token() {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse()?,
        );
    }
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .default_headers(headers)
        .build()?)
}

async fn setup_team_model(cwd: &Path, yes: bool) -> anyhow::Result<()> {
    let manifest = match read_manifest(cwd) {
        Ok(Some(m)) if m.kind == ManifestKind::Checkpoint => m,
        Ok(_) => return Ok(()),
        Err(e) => {
            println!("team router model: manifest unreadable ({e}) — skipped");
            return Ok(());
        }
    };
    let Some(source @ Source::Hf { repo, revision }) = manifest.source.as_ref() else {
        return Ok(()); // path / repo_path sources are read in place
    };
    let paths = LocalPaths::from_home()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.kode (no HOME/USERPROFILE)"))?;
    let dir = checkpoint_dir(source, cwd, &paths);
    let short = &revision[..revision.len().min(7)];
    if verify_checkpoint(&dir, &manifest.files).is_ok() {
        println!("team router model: present ({repo}@{short})");
        return Ok(());
    }
    let bytes: u64 = manifest.files.iter().map(|f| f.size).sum();
    if !confirm(
        &format!(
            "download team router model {repo}@{short} ({:.1} GB)?",
            bytes as f64 / 1e9
        ),
        yes,
    )
    .await
    {
        println!("team router model: skipped — Kode uses the pinned model");
        return Ok(());
    }
    let client = hf_client()?;
    for f in &manifest.files {
        println!("  downloading {}…", f.path);
        download_verified(
            &client,
            &model_url(repo, revision, &f.path),
            &dir.join(&f.path),
            &f.sha256,
            f.size,
        )
        .await?;
    }
    println!("team router model: ready ({repo}@{short})");
    Ok(())
}

// --- local router -------------------------------------------------------

async fn nvidia_present() -> bool {
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("nvidia-smi")
            .arg("-L")
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .is_some_and(|o| o.status.success())
}

async fn setup_local(cfg: &RouterConfig, yes: bool) -> anyhow::Result<()> {
    if !cfg.enabled {
        println!("local router: disabled in config — skipped");
        return Ok(());
    }
    if MODEL_FILES.is_empty() {
        println!("local router: this build pins no models — skipped");
        return Ok(());
    }
    let paths = LocalPaths::from_home()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.kode (no HOME/USERPROFILE)"))?;
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let Some(runtime) = select_runtime(RUNTIMES, os, arch, nvidia_present().await) else {
        println!("local router: no ONNX Runtime build for {os}/{arch} — static routing");
        return Ok(());
    };
    let bytes: u64 = runtime.archives.iter().map(|a| a.size).sum::<u64>()
        + MODEL_FILES.iter().map(|f| f.size).sum::<u64>();
    if !confirm(
        &format!(
            "download local router models + ONNX Runtime {ORT_VERSION} ({}, {:.1} GB) to {}?",
            runtime.variant,
            bytes as f64 / 1e9,
            paths.root.display()
        ),
        yes,
    )
    .await
    {
        println!("local router: skipped — Kode routes statically");
        return Ok(());
    }
    // No total timeout: the reranker is ~2.4 GB.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()?;
    let mut report = |line: String| println!("  downloading {line}…");
    install_runtime(&client, &paths, ORT_VERSION, runtime, &mut report).await?;
    install_models(
        &client,
        &paths,
        MODELS_REPO,
        MODELS_REVISION,
        MODEL_FILES,
        &mut report,
    )
    .await?;
    println!("local router: ready ({})", runtime.variant);
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
    setup_zindeks_embedded(cfg, yes).await
}

/// Installs the pinned in-process zindeks shared library under
/// `~/.kode/runtime/zindeks/<revision>/` (checksum-verified before install).
async fn setup_zindeks_embedded(cfg: &ZindeksConfig, yes: bool) -> anyhow::Result<()> {
    if let Some(path) = crate::engine_assets::override_path() {
        println!(
            "zindeks: using {} override ({})",
            crate::engine_assets::DYLIB_ENV,
            path.display()
        );
        return Ok(());
    }

    if let Ok(path) = crate::engine_assets::zindeks_library(cfg) {
        println!("zindeks: embedded library found ({})", path.display());
        return Ok(());
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let asset = zindeks_ffi_asset(os, arch)
        .ok_or_else(|| anyhow::anyhow!("unsupported platform for embedded zindeks: {os}/{arch}"))?;

    let runtime_root = kode_core::zindeks_runtime_dir().ok_or_else(|| {
        anyhow::anyhow!("cannot determine Kode runtime dir (no HOME/LOCALAPPDATA set)")
    })?;

    if !confirm(
        &format!(
            "install embedded zindeks library ({asset}) to {}?",
            runtime_root.display()
        ),
        yes,
    )
    .await
    {
        println!(
            "zindeks: skipped — rerun `kode setup`, or set {} to a local library",
            crate::engine_assets::DYLIB_ENV
        );
        return Ok(());
    }

    // Stage on the install volume: rename cannot cross Windows drive letters.
    let tmp = runtime_root
        .parent()
        .expect("zindeks runtime directory has a parent")
        .join(format!(
            "kode-setup-zindeks-{}",
            kode_local::dataset::new_id()
        ));
    tokio::fs::create_dir_all(&tmp).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    let release_url = format!(
        "{ZINDEKS_RELEASES_BASE}/v{}",
        crate::engine_assets::ZINDEKS_VERSION
    );
    let asset_url = format!("{release_url}/{asset}");
    let sums_url = format!("{release_url}/SHA256SUMS");

    let bytes = client
        .get(&asset_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let sums = client
        .get(&sums_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let expected = find_sha256(&sums, asset)
        .ok_or_else(|| anyhow::anyhow!("SHA256SUMS has no entry for {asset}"))?;
    if !sha256_hex(&bytes).eq_ignore_ascii_case(&expected) {
        anyhow::bail!("checksum mismatch — aborting install");
    }

    let archive = tmp.join(asset);
    tokio::fs::write(&archive, &bytes).await?;

    let extract = tmp.join("extract");
    tokio::fs::create_dir_all(&extract).await?;
    let output = tokio::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&extract)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "tar extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // The archive wraps everything in one top-level directory; the inner
    // `metadata.json` names the exact source revision to install under.
    let mut top: Option<PathBuf> = None;
    let mut entries = tokio::fs::read_dir(&extract).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            top = Some(entry.path());
        }
    }
    let top = top.ok_or_else(|| anyhow::anyhow!("archive contained no directory"))?;

    let meta_text = tokio::fs::read_to_string(top.join("metadata.json")).await?;
    let meta: serde_json::Value = serde_json::from_str(&meta_text)?;
    if meta.get("source_revision").and_then(|v| v.as_str())
        != Some(crate::engine_assets::ZINDEKS_REVISION)
        || meta.get("zindeks_version").and_then(|v| v.as_str())
            != Some(crate::engine_assets::ZINDEKS_VERSION)
        || meta.get("abi_version").and_then(|v| v.as_u64()) != Some(1)
    {
        anyhow::bail!("zindeks archive metadata does not match the pinned release/ABI");
    }
    let revision = crate::engine_assets::ZINDEKS_REVISION;

    let dest = runtime_root.join(revision);
    if dest.exists() {
        tokio::fs::remove_dir_all(&dest).await?;
    }
    tokio::fs::create_dir_all(&runtime_root).await?;
    tokio::fs::rename(&top, &dest).await?;
    let _ = tokio::fs::remove_dir_all(&tmp).await;

    println!(
        "zindeks: embedded library installed ({})",
        dest.join(crate::engine_assets::dylib_file_name()).display()
    );
    Ok(())
}

/// Maps (os, arch) to the embedded ABI asset name published with each zindeks
/// release. Matches the four supported library targets; `None` otherwise.
fn zindeks_ffi_asset(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("zindeks-ffi-windows-x86_64.tar.gz"),
        ("linux", "x86_64") => Some("zindeks-ffi-linux-x86_64.tar.gz"),
        ("linux", "aarch64") => Some("zindeks-ffi-linux-aarch64.tar.gz"),
        ("macos", "aarch64") => Some("zindeks-ffi-macos-aarch64.tar.gz"),
        _ => None,
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
    let line = strip_ansi(text.lines().next().unwrap_or(""));
    let line = line.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

/// Strips ANSI CSI escape sequences (e.g. `\x1b[1m`) — styled tools emit them
/// even when piped, and they'd otherwise leak into doctor/setup output.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.clone().next() == Some('[') {
                chars.next();
                for f in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&f) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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

async fn setup_ingat(cfg: &IngatConfig, _yes: bool) -> anyhow::Result<()> {
    let path = match &cfg.store_path {
        Some(path) => path.clone(),
        None => kode_core::default_ingat_store_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine Kode home for the memory store"))?,
    };
    match kode_memory::EmbeddedIngat::open(&path) {
        Ok(_) => println!("ingat: memory store ready ({})", path.display()),
        Err(e) => println!("ingat: memory store unavailable — {e}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(
            strip_ansi("\u{1b}[1mzindeks 0.9.2\u{1b}[0m"),
            "zindeks 0.9.2"
        );
    }

    #[test]
    fn strip_ansi_passes_plain_text_through() {
        assert_eq!(strip_ansi("zindeks 0.9.2"), "zindeks 0.9.2");
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

    #[test]
    fn zindeks_ffi_asset_covers_all_library_targets() {
        assert_eq!(
            zindeks_ffi_asset("windows", "x86_64"),
            Some("zindeks-ffi-windows-x86_64.tar.gz")
        );
        assert_eq!(
            zindeks_ffi_asset("linux", "x86_64"),
            Some("zindeks-ffi-linux-x86_64.tar.gz")
        );
        assert_eq!(
            zindeks_ffi_asset("linux", "aarch64"),
            Some("zindeks-ffi-linux-aarch64.tar.gz")
        );
        assert_eq!(
            zindeks_ffi_asset("macos", "aarch64"),
            Some("zindeks-ffi-macos-aarch64.tar.gz")
        );
        // Unsupported library targets stay unsupported (no asset pretended).
        assert_eq!(zindeks_ffi_asset("macos", "x86_64"), None);
        assert_eq!(zindeks_ffi_asset("windows", "aarch64"), None);
    }
}
