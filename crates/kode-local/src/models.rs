//! Pinned artefacts (ONNX Runtime + model files), their layout under
//! `~/.kode`, sha256 verification with a `.verified` stamp, and the
//! streaming installer behind `kode setup`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::error::LocalError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedFile {
    /// Path inside the models repo, e.g. `laya-multilingual/model.onnx`.
    pub path: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Archive {
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimePin {
    /// `std::env::consts::OS` / `ARCH` values.
    pub os: &'static str,
    pub arch: &'static str,
    /// `cpu` | `gpu` | `directml` | `coreml`.
    pub variant: &'static str,
    pub archives: &'static [Archive],
    /// ONNX Runtime shared library, relative to the runtime dir.
    pub dylib: &'static str,
    /// Extra extracted files copied next to the dylib (DirectML.dll).
    pub colocate: &'static [&'static str],
}

pub const LAYA_DIR: &str = "laya-multilingual";
pub const RERANKER_DIR: &str = "qwen3-reranker-0.6b";

pub struct LocalPaths {
    pub root: PathBuf,
}

impl LocalPaths {
    /// `~/.kode` (see `kode_core::kode_home_dir`).
    pub fn from_home() -> Option<Self> {
        kode_core::kode_home_dir().map(|root| Self { root })
    }

    pub fn models_dir(&self, revision: &str) -> PathBuf {
        self.root.join("models").join(revision)
    }

    pub fn runtime_dir(&self, ort_version: &str) -> PathBuf {
        self.root
            .join("runtime")
            .join(format!("onnxruntime-{ort_version}"))
    }
}

pub fn model_url(repo: &str, revision: &str, path: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/{revision}/{path}")
}

/// Linux x86_64 ships `cpu` and `gpu` variants; everything else ships one.
pub fn select_runtime<'a>(
    pins: &'a [RuntimePin],
    os: &str,
    arch: &str,
    nvidia: bool,
) -> Option<&'a RuntimePin> {
    let matching: Vec<&RuntimePin> = pins
        .iter()
        .filter(|p| p.os == os && p.arch == arch)
        .collect();
    if matching.len() <= 1 {
        return matching.into_iter().next();
    }
    let want = if nvidia { "gpu" } else { "cpu" };
    matching
        .iter()
        .copied()
        .find(|p| p.variant == want)
        .or_else(|| matching.first().copied())
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Stamp {
    sha256: String,
    size: u64,
    mtime_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct InstalledRuntime {
    variant: String,
    dylib: String,
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn stamp_path(path: &Path) -> PathBuf {
    suffixed(path, ".verified")
}

fn part_path(path: &Path) -> PathBuf {
    suffixed(path, ".part")
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> LocalError + '_ {
    move |source| LocalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn write_stamp(path: &Path, sha256: &str, size: u64) {
    if let Ok(meta) = std::fs::metadata(path) {
        let stamp = Stamp {
            sha256: sha256.to_lowercase(),
            size,
            mtime_secs: mtime_secs(&meta),
        };
        // A failed stamp write only costs a re-hash next time.
        let _ = std::fs::write(
            stamp_path(path),
            serde_json::to_string(&stamp).unwrap_or_default(),
        );
    }
}

/// Verifies `path` against its pin. Trusts a `.verified` stamp while the
/// file's size and mtime still match it; otherwise hashes the file and, on
/// success, writes a fresh stamp.
pub fn verify_file(path: &Path, sha256: &str, size: u64) -> Result<(), LocalError> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LocalError::Missing(path.to_path_buf()));
        }
        Err(e) => return Err(io_err(path)(e)),
    };
    if meta.len() != size {
        return Err(LocalError::ChecksumMismatch(path.to_path_buf()));
    }
    let expected = Stamp {
        sha256: sha256.to_lowercase(),
        size,
        mtime_secs: mtime_secs(&meta),
    };
    if let Ok(text) = std::fs::read_to_string(stamp_path(path))
        && serde_json::from_str::<Stamp>(&text).ok().as_ref() == Some(&expected)
    {
        return Ok(());
    }
    let actual = sha256_file(path).map_err(io_err(path))?;
    if actual != expected.sha256 {
        return Err(LocalError::ChecksumMismatch(path.to_path_buf()));
    }
    write_stamp(path, sha256, size);
    Ok(())
}

/// Verifies every pinned file under `<models>/<revision>/<subdir>/` and
/// returns that directory.
pub fn verify_model_dir(
    paths: &LocalPaths,
    revision: &str,
    files: &[PinnedFile],
    subdir: &str,
) -> Result<PathBuf, LocalError> {
    let prefix = format!("{subdir}/");
    let mine: Vec<&PinnedFile> = files
        .iter()
        .filter(|f| f.path.starts_with(&prefix))
        .collect();
    if mine.is_empty() {
        return Err(LocalError::NotPinned);
    }
    let root = paths.models_dir(revision);
    for f in mine {
        verify_file(&root.join(f.path), f.sha256, f.size)?;
    }
    Ok(root.join(subdir))
}

async fn stream_to(client: &reqwest::Client, url: &str, part: &Path) -> Result<String, LocalError> {
    let resp = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| LocalError::Download(format!("{url}: {e}")))?;
    let mut file = tokio::fs::File::create(part).await.map_err(io_err(part))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| LocalError::Download(format!("{url}: {e}")))?;
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(io_err(part))?;
    }
    file.flush().await.map_err(io_err(part))?;
    Ok(hex(&hasher.finalize()))
}

/// Downloads `url` to `dest` via `dest.part`, hashing while streaming. A
/// wrong checksum removes the partial file and leaves `dest` untouched. An
/// already-valid `dest` is not downloaded again.
pub async fn download_verified(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: &str,
    size: u64,
) -> Result<(), LocalError> {
    if verify_file(dest, sha256, size).is_ok() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(io_err(parent))?;
    }
    let part = part_path(dest);
    let actual = match stream_to(client, url, &part).await {
        Ok(hash) => hash,
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
    };
    if actual != sha256.to_lowercase() {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(LocalError::ChecksumMismatch(dest.to_path_buf()));
    }
    tokio::fs::rename(&part, dest).await.map_err(io_err(dest))?;
    write_stamp(dest, sha256, size);
    Ok(())
}

fn human(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

pub async fn install_models(
    client: &reqwest::Client,
    paths: &LocalPaths,
    repo: &str,
    revision: &str,
    files: &[PinnedFile],
    report: &mut (dyn FnMut(String) + Send),
) -> Result<(), LocalError> {
    if files.is_empty() || repo.is_empty() || revision.is_empty() {
        return Err(LocalError::NotPinned);
    }
    let root = paths.models_dir(revision);
    for f in files {
        report(format!("model {} ({})", f.path, human(f.size)));
        download_verified(
            client,
            &model_url(repo, revision, f.path),
            &root.join(f.path),
            f.sha256,
            f.size,
        )
        .await?;
    }
    Ok(())
}

/// On Windows, the system bsdtar (`%SystemRoot%\System32\tar.exe`) reads
/// the .zip/.nupkg runtime packages; a `tar` earlier on PATH may be GNU tar
/// (Git Bash, MSYS2), which cannot. Elsewhere `tar` only sees .tgz.
fn tar_program(windows: bool, system_root: Option<&std::ffi::OsStr>) -> PathBuf {
    if windows && let Some(root) = system_root {
        let bsdtar = Path::new(root).join("System32").join("tar.exe");
        if bsdtar.is_file() {
            return bsdtar;
        }
    }
    PathBuf::from("tar")
}

async fn extract(archive: &Path, into: &Path) -> Result<(), LocalError> {
    let tar = tar_program(cfg!(windows), std::env::var_os("SystemRoot").as_deref());
    let out = tokio::process::Command::new(tar)
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .await
        .map_err(io_err(archive))?;
    if !out.status.success() {
        return Err(LocalError::Download(format!(
            "extracting {}: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

pub async fn install_runtime(
    client: &reqwest::Client,
    paths: &LocalPaths,
    ort_version: &str,
    pin: &RuntimePin,
    report: &mut (dyn FnMut(String) + Send),
) -> Result<PathBuf, LocalError> {
    let dir = paths.runtime_dir(ort_version);
    let downloads = dir.join("downloads");
    for (i, a) in pin.archives.iter().enumerate() {
        report(format!(
            "onnxruntime {ort_version} ({}) part {} ({})",
            pin.variant,
            i + 1,
            human(a.size)
        ));
        let file = downloads.join(format!("archive-{i}"));
        download_verified(client, a.url, &file, a.sha256, a.size).await?;
        extract(&file, &dir).await?;
    }
    let dylib = dir.join(pin.dylib);
    let lib_dir = dylib.parent().unwrap_or(&dir).to_path_buf();
    for extra in pin.colocate {
        let src = dir.join(extra);
        let name = src
            .file_name()
            .ok_or_else(|| LocalError::Missing(src.clone()))?;
        tokio::fs::copy(&src, lib_dir.join(name))
            .await
            .map_err(io_err(&src))?;
    }
    if !dylib.is_file() {
        return Err(LocalError::Missing(dylib));
    }
    let marker = InstalledRuntime {
        variant: pin.variant.to_string(),
        dylib: pin.dylib.to_string(),
    };
    let marker_path = dir.join("installed.json");
    std::fs::write(
        &marker_path,
        serde_json::to_string(&marker).unwrap_or_default(),
    )
    .map_err(io_err(&marker_path))?;
    Ok(dylib)
}

/// `(variant, dylib path)` of a completed runtime install.
pub fn installed_runtime(paths: &LocalPaths, ort_version: &str) -> Option<(String, PathBuf)> {
    let dir = paths.runtime_dir(ort_version);
    let text = std::fs::read_to_string(dir.join("installed.json")).ok()?;
    let marker: InstalledRuntime = serde_json::from_str(&text).ok()?;
    let dylib = dir.join(&marker.dylib);
    dylib.is_file().then_some((marker.variant, dylib))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn temp_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-local-models-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn serve_once(body: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body).await.unwrap();
        });
        format!("http://{addr}/file")
    }

    const PINS: &[RuntimePin] = &[
        RuntimePin {
            os: "linux",
            arch: "x86_64",
            variant: "cpu",
            archives: &[],
            dylib: "cpu.so",
            colocate: &[],
        },
        RuntimePin {
            os: "linux",
            arch: "x86_64",
            variant: "gpu",
            archives: &[],
            dylib: "gpu.so",
            colocate: &[],
        },
        RuntimePin {
            os: "windows",
            arch: "x86_64",
            variant: "directml",
            archives: &[],
            dylib: "onnxruntime.dll",
            colocate: &[],
        },
    ];

    #[test]
    fn model_url_uses_resolve_revision() {
        assert_eq!(
            model_url(
                "me/kode-local-models",
                "abc123",
                "laya-multilingual/model.onnx"
            ),
            "https://huggingface.co/me/kode-local-models/resolve/abc123/laya-multilingual/model.onnx"
        );
    }

    #[test]
    fn select_runtime_prefers_gpu_variant_only_with_nvidia() {
        assert_eq!(
            select_runtime(PINS, "linux", "x86_64", true)
                .unwrap()
                .variant,
            "gpu"
        );
        assert_eq!(
            select_runtime(PINS, "linux", "x86_64", false)
                .unwrap()
                .variant,
            "cpu"
        );
        assert_eq!(
            select_runtime(PINS, "windows", "x86_64", false)
                .unwrap()
                .variant,
            "directml"
        );
        assert!(select_runtime(PINS, "windows", "aarch64", false).is_none());
    }

    #[test]
    fn verify_file_accepts_match_and_writes_stamp() {
        let dir = temp_dir("verify-ok");
        let f = dir.join("hello.bin");
        std::fs::write(&f, b"hello").unwrap();
        verify_file(&f, HELLO_SHA, 5).unwrap();
        assert!(stamp_path(&f).is_file());
        verify_file(&f, HELLO_SHA, 5).unwrap();
    }

    #[test]
    fn verify_file_reports_missing_mismatch_and_size() {
        let dir = temp_dir("verify-bad");
        let f = dir.join("hello.bin");
        assert!(matches!(
            verify_file(&f, HELLO_SHA, 5),
            Err(LocalError::Missing(_))
        ));
        std::fs::write(&f, b"hellp").unwrap();
        assert!(matches!(
            verify_file(&f, HELLO_SHA, 5),
            Err(LocalError::ChecksumMismatch(_))
        ));
        assert!(matches!(
            verify_file(&f, HELLO_SHA, 6),
            Err(LocalError::ChecksumMismatch(_))
        ));
    }

    #[test]
    fn stale_stamp_is_not_trusted_after_file_changes() {
        let dir = temp_dir("verify-stale");
        let f = dir.join("hello.bin");
        std::fs::write(&f, b"hello").unwrap();
        verify_file(&f, HELLO_SHA, 5).unwrap();
        std::fs::write(&f, b"hello!").unwrap();
        assert!(verify_file(&f, HELLO_SHA, 5).is_err());
    }

    #[test]
    fn verify_model_dir_checks_only_its_subdir() {
        let dir = temp_dir("model-dir");
        let paths = LocalPaths { root: dir.clone() };
        let files = [
            PinnedFile {
                path: "laya-multilingual/a.bin",
                sha256: HELLO_SHA,
                size: 5,
            },
            PinnedFile {
                path: "qwen3-reranker-0.6b/b.bin",
                sha256: HELLO_SHA,
                size: 5,
            },
        ];
        let laya = paths.models_dir("rev").join("laya-multilingual");
        std::fs::create_dir_all(&laya).unwrap();
        std::fs::write(laya.join("a.bin"), b"hello").unwrap();
        assert_eq!(
            verify_model_dir(&paths, "rev", &files, LAYA_DIR).unwrap(),
            laya
        );
        assert!(matches!(
            verify_model_dir(&paths, "rev", &files, RERANKER_DIR),
            Err(LocalError::Missing(_))
        ));
        assert!(matches!(
            verify_model_dir(&paths, "rev", &[], LAYA_DIR),
            Err(LocalError::NotPinned)
        ));
    }

    #[test]
    fn windows_extracts_with_system_bsdtar_not_path_tar() {
        let root = temp_dir("sysroot");
        let bsdtar = root.join("System32").join("tar.exe");
        std::fs::create_dir_all(bsdtar.parent().unwrap()).unwrap();
        std::fs::write(&bsdtar, b"").unwrap();
        assert_eq!(tar_program(true, Some(root.as_os_str())), bsdtar);
        // No system bsdtar (or not Windows): fall back to PATH lookup.
        let empty = temp_dir("sysroot-empty");
        assert_eq!(
            tar_program(true, Some(empty.as_os_str())),
            PathBuf::from("tar")
        );
        assert_eq!(tar_program(true, None), PathBuf::from("tar"));
        assert_eq!(
            tar_program(false, Some(root.as_os_str())),
            PathBuf::from("tar")
        );
    }

    #[test]
    fn installed_runtime_reads_marker() {
        let dir = temp_dir("runtime");
        let paths = LocalPaths { root: dir };
        assert!(installed_runtime(&paths, "1.22.0").is_none());
        let rt = paths.runtime_dir("1.22.0");
        std::fs::create_dir_all(rt.join("lib")).unwrap();
        std::fs::write(rt.join("lib/libonnxruntime.so"), b"x").unwrap();
        std::fs::write(
            rt.join("installed.json"),
            r#"{"variant":"cpu","dylib":"lib/libonnxruntime.so"}"#,
        )
        .unwrap();
        let (variant, dylib) = installed_runtime(&paths, "1.22.0").unwrap();
        assert_eq!(variant, "cpu");
        assert!(dylib.ends_with("lib/libonnxruntime.so"));
    }

    #[tokio::test]
    async fn download_verified_writes_file_and_stamp() {
        let url = serve_once(b"hello").await;
        let dest = temp_dir("dl-ok").join("sub/hello.bin");
        download_verified(&reqwest::Client::new(), &url, &dest, HELLO_SHA, 5)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert!(stamp_path(&dest).is_file());
    }

    #[tokio::test]
    async fn download_verified_rejects_wrong_checksum_and_leaves_nothing() {
        let url = serve_once(b"hellp").await;
        let dest = temp_dir("dl-bad").join("hello.bin");
        let err = download_verified(&reqwest::Client::new(), &url, &dest, HELLO_SHA, 5)
            .await
            .unwrap_err();
        assert!(matches!(err, LocalError::ChecksumMismatch(_)));
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }
}
