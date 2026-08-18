use std::path::Path;

use kode_core::config::KodeConfig;
use kode_memory::wire::WireEntry;
use kode_memory::{
    EngineeringMemory, IngatAdapter, MemoryContext, MemoryKind, NewMemory, Provenance,
};

use crate::team_memory;

/// Runs `kode remember <text> [--kind <kind>] [--tag <tag> ...] [--team]`.
///
/// Explicit user memory is written directly to Ingat with no permission
/// prompt (per product spec §31) — the user asked for it by name. `--team`
/// additionally appends the memory to the git-backed
/// `.kode/memory/team.jsonl` file so teammates pick it up on their next
/// session start (see `docs/superpowers/specs/2026-08-18-team-memory-design.md`).
pub async fn run(
    text: &str,
    kind: &str,
    tags: Vec<String>,
    team: bool,
    cwd: &Path,
) -> anyhow::Result<()> {
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
    let author = git_output(cwd, &["config", "user.name"]).await;
    let summary = truncate_summary(text, 100);

    let memory = NewMemory {
        kind,
        summary,
        body: text.to_string(),
        tags,
        provenance: Provenance::ExplicitUser,
        context,
        team,
    };

    let adapter = IngatAdapter::new(&config.ingat);
    match adapter.remember(&memory).await {
        Ok(id) => {
            println!("remembered ({}): {id}", kind.as_kebab());
            if team {
                let entry = WireEntry::new(&memory, author);
                team_memory::share(cwd, &entry)?;
                println!(
                    "shared with team: {}",
                    team_memory::team_file_path(cwd).display()
                );
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-remember-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0)
    }

    /// Reads a full HTTP/1.1 request (headers + body, by `Content-Length`)
    /// off `stream` before the caller responds — closing the socket while
    /// unread request bytes remain can send a `RST` instead of a clean
    /// `FIN`, corrupting the response on the client side (see the
    /// equivalent helper in `kode-memory::ingat`'s tests, which this
    /// mirrors).
    async fn drain_request(stream: &mut TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_header_end(&buf) {
                break pos;
            }
        };
        let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let len = content_length(&header_text);
        while buf.len() < header_end + len {
            let n = stream.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// A minimal one-shot fake `/api/contexts` responder: accepts one
    /// connection, drains the request, then replies with a canned success
    /// body. We don't assert on the request here — that's covered
    /// thoroughly by `kode-memory`'s own adapter tests.
    async fn one_shot_ok_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            drain_request(&mut stream).await;
            let body = r#"{"id":"mem-cli","project":"kode","summary":"s","kind":{"Other":"project-rule"},"tags":["kode"],"created_at":"2026-01-01T00:00:00Z"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
        format!("http://{addr}")
    }

    async fn write_ingat_config(dir: &Path, base_url: &str) {
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            format!("[ingat]\nurl = \"{base_url}\"\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn remember_team_appends_wire_entry_and_prints_share_path() {
        let dir = temp_dir("team");
        let base = one_shot_ok_server().await;
        write_ingat_config(&dir, &base).await;

        run(
            "always squash-merge feature branches",
            "convention",
            vec![],
            true,
            &dir,
        )
        .await
        .unwrap();

        let path = team_memory::team_file_path(&dir);
        let (entries, corrupt) = kode_memory::wire::read_entries(&path);
        assert_eq!(corrupt, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "always squash-merge feature branches");
        assert_eq!(entries[0].kind, "convention");
        assert_eq!(entries[0].provenance, "explicit-user");
    }

    #[tokio::test]
    async fn remember_without_team_does_not_write_team_file() {
        let dir = temp_dir("no-team");
        let base = one_shot_ok_server().await;
        write_ingat_config(&dir, &base).await;

        run(
            "a personal-only note about local setup",
            "project-rule",
            vec![],
            false,
            &dir,
        )
        .await
        .unwrap();

        let path = team_memory::team_file_path(&dir);
        assert!(!path.exists());
    }

    #[test]
    fn truncate_summary_is_char_safe() {
        assert_eq!(truncate_summary("hello world", 5), "hello");
        assert_eq!(truncate_summary("hi", 5), "hi");
    }
}
