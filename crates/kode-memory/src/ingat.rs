use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::EngineeringMemory;
use crate::MemoryStats;
use crate::error::{MemoryError, Result, map_reqwest_error};
use crate::types::{Memory, MemoryKind, MemoryQuery, NewMemory, Provenance};
use kode_core::config::IngatConfig;

/// Maximum number of `sym:` tags attached to a stored memory, so a memory
/// with a long symbol list doesn't blow up Ingat's tag list unboundedly.
const MAX_SYMBOL_TAGS: usize = 8;

/// Speaks REST to a local `ingat` `mcp_service` HTTP server and exposes the
/// narrow [`EngineeringMemory`] domain surface.
///
/// Talks only to Ingat's HTTP API — never touches Ingat's storage directly.
pub struct IngatAdapter {
    client: reqwest::Client,
    base_url: String,
}

impl IngatAdapter {
    pub fn new(cfg: &IngatConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url: cfg.url.trim_end_matches('/').to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Parses a response body as `T` on 2xx, or maps a non-2xx response into
    /// a [`MemoryError::Service`] (when Ingat sent a parseable
    /// `{"error","code"}` body) or [`MemoryError::Protocol`] otherwise.
    async fn parse_response<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if status.is_success() {
            resp.json::<T>()
                .await
                .map_err(|e| MemoryError::Protocol(format!("invalid response body: {e}")))
        } else {
            let text = resp.text().await.unwrap_or_default();
            match serde_json::from_str::<ErrorResponse>(&text) {
                Ok(err) => Err(MemoryError::Service {
                    code: err.code,
                    message: err.error,
                }),
                Err(_) => Err(MemoryError::Protocol(format!("http {status}: {text}"))),
            }
        }
    }
}

#[async_trait::async_trait]
impl EngineeringMemory for IngatAdapter {
    async fn health(&self) -> Result<()> {
        let resp = self
            .client
            .get(self.url("/health"))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let body: HealthResponse = self.parse_response(resp).await?;
        if body.status == "healthy" {
            Ok(())
        } else {
            Err(MemoryError::Unavailable(format!(
                "ingat status: {}",
                body.status
            )))
        }
    }

    async fn search(&self, query: &MemoryQuery) -> Result<Vec<Memory>> {
        let filters = QueryFilters {
            project: query.repository.clone(),
            kind: query.kind.map(kind_to_wire),
            tag: None,
            ide: None,
        };
        let req = SearchRequest {
            prompt: query.text.clone(),
            filters,
            limit: query.limit as usize,
        };

        let resp = self
            .client
            .post(self.url("/api/search"))
            .json(&req)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let body: SearchResponse = self.parse_response(resp).await?;

        Ok(body.results.into_iter().map(map_search_result).collect())
    }

    async fn remember(&self, memory: &NewMemory) -> Result<String> {
        let project = memory
            .context
            .repository
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let file_path = memory.context.files.first().cloned();
        let body = if memory.body.is_empty() {
            memory.summary.clone()
        } else {
            memory.body.clone()
        };

        let mut tags = memory.tags.clone();
        tags.push("kode".to_string());
        tags.push(format!("kode-kind:{}", memory.kind.as_kebab()));
        tags.push(format!("provenance:{}", memory.provenance.as_kebab()));
        if let Some(branch) = &memory.context.branch {
            tags.push(format!("branch:{branch}"));
        }
        if let Some(commit) = &memory.context.commit {
            tags.push(format!("commit:{commit}"));
        }
        for symbol in memory.context.symbols.iter().take(MAX_SYMBOL_TAGS) {
            tags.push(format!("sym:{symbol}"));
        }

        let req = IngestContextRequest {
            project,
            ide: "kode".to_string(),
            file_path,
            language: None,
            summary: memory.summary.clone(),
            body,
            tags,
            kind: kind_to_wire(memory.kind),
        };

        let resp = self
            .client
            .post(self.url("/api/contexts"))
            .json(&req)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let summary: ContextSummary = self.parse_response(resp).await?;
        Ok(summary.id)
    }

    async fn stats(&self) -> Result<MemoryStats> {
        let resp = self
            .client
            .get(self.url("/api/stats"))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let body: StatsResponse = self.parse_response(resp).await?;
        Ok(MemoryStats {
            total: body.total_contexts,
            version: body.version,
        })
    }
}

/// Maps a Kode [`MemoryKind`] onto Ingat's `ContextKind` wire enum.
/// `HistoricalSolution` has a direct Ingat counterpart (`FixHistory`);
/// everything else rides in `Other(<kebab>)` since Ingat's kind enum has no
/// slots for Kode-specific categories.
fn kind_to_wire(kind: MemoryKind) -> ContextKind {
    match kind {
        MemoryKind::HistoricalSolution => ContextKind::FixHistory,
        other => ContextKind::Other(other.as_kebab().to_string()),
    }
}

/// Best-effort reverse of [`kind_to_wire`], used only as a fallback when a
/// result carries no (or an unrecognized) `kode-kind:` tag.
fn kind_from_wire(kind: &ContextKind) -> Option<MemoryKind> {
    match kind {
        ContextKind::FixHistory => Some(MemoryKind::HistoricalSolution),
        ContextKind::Other(s) => MemoryKind::from_kebab(s),
        _ => None,
    }
}

/// Splits an Ingat tag list into Kode's parsed-out fields (kind, provenance)
/// plus the remaining user-authored tags. Kode's own bookkeeping tags
/// (`kode`, `kode-kind:*`, `provenance:*`, `branch:*`, `commit:*`, `sym:*`)
/// never surface as user-visible tags.
fn split_tags(tags: Vec<String>) -> (Option<MemoryKind>, Option<Provenance>, Vec<String>) {
    let mut kind = None;
    let mut provenance = None;
    let mut rest = Vec::new();

    for tag in tags {
        if tag == "kode" {
            continue;
        } else if let Some(kebab) = tag.strip_prefix("kode-kind:") {
            kind = MemoryKind::from_kebab(kebab);
        } else if let Some(kebab) = tag.strip_prefix("provenance:") {
            provenance = Provenance::from_kebab(kebab);
        } else if tag.starts_with("branch:")
            || tag.starts_with("commit:")
            || tag.starts_with("sym:")
        {
            continue;
        } else {
            rest.push(tag);
        }
    }

    (kind, provenance, rest)
}

fn map_search_result(dto: SearchResultDto) -> Memory {
    let (kind_from_tag, provenance, tags) = split_tags(dto.tags);
    let kind = kind_from_tag.or_else(|| kind_from_wire(&dto.kind));

    Memory {
        id: dto.id,
        kind,
        summary: dto.summary,
        body: dto.body,
        tags,
        provenance,
        score: dto.score,
        project: dto.project,
        created_at: dto.created_at,
    }
}

// --- Wire DTOs, mirroring Ingat's `mcp_service` REST API verbatim. Private:
// domain types (above) are the only thing that crosses the adapter boundary.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum ContextKind {
    CodeSnippet,
    FixHistory,
    ProjectSummary,
    Discussion,
    ToolLog,
    Other(String),
}

#[derive(Debug, Serialize)]
struct IngestContextRequest {
    project: String,
    ide: String,
    file_path: Option<String>,
    language: Option<String>,
    summary: String,
    body: String,
    tags: Vec<String>,
    kind: ContextKind,
}

#[derive(Debug, Deserialize)]
struct ContextSummary {
    id: String,
    #[allow(dead_code)]
    project: String,
    #[allow(dead_code)]
    summary: String,
    #[allow(dead_code)]
    kind: ContextKind,
    #[allow(dead_code)]
    tags: Vec<String>,
    #[allow(dead_code)]
    created_at: String,
}

#[derive(Debug, Serialize)]
struct SearchRequest {
    prompt: String,
    filters: QueryFilters,
    limit: usize,
}

#[derive(Debug, Serialize)]
struct QueryFilters {
    project: Option<String>,
    kind: Option<ContextKind>,
    tag: Option<String>,
    ide: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[allow(dead_code)]
    query: String,
    results: Vec<SearchResultDto>,
}

#[derive(Debug, Deserialize)]
struct SearchResultDto {
    id: String,
    project: String,
    summary: String,
    body: String,
    tags: Vec<String>,
    kind: ContextKind,
    score: f32,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct StatsResponse {
    total_contexts: u64,
    #[allow(dead_code)]
    data_dir: String,
    version: String,
    #[allow(dead_code)]
    uptime_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    status: String,
    #[allow(dead_code)]
    service: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    code: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn cfg(url: String) -> IngatConfig {
        IngatConfig { enabled: true, url }
    }

    struct FakeRequest {
        method: String,
        path: String,
        body: String,
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

    async fn read_request(stream: &mut TcpStream) -> FakeRequest {
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
        let request_line = header_text.lines().next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();

        let len = content_length(&header_text);
        while buf.len() < header_end + len {
            let n = stream.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&buf[header_end..header_end + len]).to_string();

        FakeRequest { method, path, body }
    }

    async fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    }

    /// Spins up a one-shot fake HTTP server: accepts a single connection,
    /// records the request, replies with the canned status/body.
    async fn one_shot_server(
        status: u16,
        reason: &'static str,
        response_body: String,
    ) -> (String, tokio::task::JoinHandle<FakeRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let req = read_request(&mut stream).await;
            write_response(&mut stream, status, reason, &response_body).await;
            req
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn health_ok_on_healthy_status() {
        let (base, handle) = one_shot_server(
            200,
            "OK",
            r#"{"status":"healthy","service":"ingat-backend"}"#.to_string(),
        )
        .await;
        let adapter = IngatAdapter::new(&cfg(base));

        adapter.health().await.unwrap();

        let req = handle.await.unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/health");
    }

    #[tokio::test]
    async fn health_connection_refused_maps_to_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // nothing listening on `addr` anymore

        let adapter = IngatAdapter::new(&cfg(format!("http://{addr}")));
        let err = adapter.health().await.unwrap_err();
        assert!(matches!(err, MemoryError::Unavailable(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn remember_sends_other_kind_and_bookkeeping_tags() {
        let response = r#"{"id":"mem-1","project":"kode","summary":"s","kind":{"Other":"project-rule"},"tags":["kode"],"created_at":"2026-01-01T00:00:00Z"}"#;
        let (base, handle) = one_shot_server(200, "OK", response.to_string()).await;
        let adapter = IngatAdapter::new(&cfg(base));

        let memory = NewMemory {
            kind: MemoryKind::ProjectRule,
            summary: "always prefix shell with rtk".to_string(),
            body: "always prefix shell commands with rtk for token savings".to_string(),
            tags: vec!["user-tag".to_string()],
            provenance: Provenance::ExplicitUser,
            context: crate::types::MemoryContext {
                repository: Some("kode".to_string()),
                branch: Some("main".to_string()),
                commit: Some("abc123".to_string()),
                files: vec!["src/lib.rs".to_string()],
                symbols: vec!["Foo::bar".to_string()],
            },
        };

        let id = adapter.remember(&memory).await.unwrap();
        assert_eq!(id, "mem-1");

        let req = handle.await.unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/contexts");

        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["ide"], "kode");
        assert_eq!(body["project"], "kode");
        assert_eq!(body["file_path"], "src/lib.rs");
        assert_eq!(body["kind"], serde_json::json!({"Other": "project-rule"}));

        let tags: Vec<&str> = body["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(tags.contains(&"user-tag"));
        assert!(tags.contains(&"kode"));
        assert!(tags.contains(&"kode-kind:project-rule"));
        assert!(tags.contains(&"provenance:explicit-user"));
        assert!(tags.contains(&"branch:main"));
        assert!(tags.contains(&"commit:abc123"));
        assert!(tags.contains(&"sym:Foo::bar"));
    }

    #[tokio::test]
    async fn remember_historical_solution_sends_fix_history_kind() {
        let response = r#"{"id":"mem-2","project":"kode","summary":"s","kind":"FixHistory","tags":["kode"],"created_at":"2026-01-01T00:00:00Z"}"#;
        let (base, handle) = one_shot_server(200, "OK", response.to_string()).await;
        let adapter = IngatAdapter::new(&cfg(base));

        let memory = NewMemory {
            kind: MemoryKind::HistoricalSolution,
            summary: "fixed flaky test".to_string(),
            body: String::new(),
            tags: vec![],
            provenance: Provenance::VerifiedTest,
            context: crate::types::MemoryContext::default(),
        };

        adapter.remember(&memory).await.unwrap();

        let req = handle.await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["kind"], serde_json::json!("FixHistory"));
        // empty body falls back to summary
        assert_eq!(body["body"], "fixed flaky test");
        assert_eq!(body["project"], "unknown");
    }

    #[tokio::test]
    async fn search_maps_tags_into_kind_provenance_and_user_tags() {
        let response = serde_json::json!({
            "query": "convention",
            "results": [{
                "id": "mem-3",
                "project": "kode",
                "summary": "use rtk",
                "body": "always prefix shell with rtk",
                "tags": ["kode", "kode-kind:convention", "provenance:verified-code", "sym:Foo::bar", "user-tag"],
                "kind": {"Other": "convention"},
                "score": 0.9,
                "created_at": "2026-01-01T00:00:00Z"
            }]
        })
        .to_string();
        let (base, handle) = one_shot_server(200, "OK", response).await;
        let adapter = IngatAdapter::new(&cfg(base));

        let results = adapter
            .search(&MemoryQuery {
                text: "convention".to_string(),
                repository: None,
                kind: None,
                limit: 8,
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        let m = &results[0];
        assert_eq!(m.id, "mem-3");
        assert_eq!(m.kind, Some(MemoryKind::Convention));
        assert_eq!(m.provenance, Some(Provenance::VerifiedCode));
        assert_eq!(m.tags, vec!["user-tag".to_string()]);

        let req = handle.await.unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/search");
    }

    #[tokio::test]
    async fn search_500_maps_to_service_error() {
        let response = r#"{"error":"index unavailable","code":"SEARCH_FAILED"}"#;
        let (base, handle) =
            one_shot_server(500, "Internal Server Error", response.to_string()).await;
        let adapter = IngatAdapter::new(&cfg(base));

        let err = adapter.search(&MemoryQuery::default()).await.unwrap_err();

        match err {
            MemoryError::Service { code, message } => {
                assert_eq!(code, "SEARCH_FAILED");
                assert_eq!(message, "index unavailable");
            }
            other => panic!("expected Service error, got {other:?}"),
        }
        handle.await.unwrap();
    }
}
