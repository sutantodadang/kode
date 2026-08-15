use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::error::{IntelError, Result};
use crate::types::{
    CodeContext, CodeContextRequest, CodeSearchResult, FileOutline, IntelHealth, OutlineSymbol,
};
use crate::{CodeIntelligence, error};
use kode_core::config::ZindeksConfig;
use kode_mcp::McpClient;

/// Speaks MCP JSON-RPC to a local `zindeks` process (stdio child) or a
/// running `zindeks serve --port N` (TCP), and exposes the narrow
/// [`CodeIntelligence`] domain surface.
pub struct ZindeksAdapter {
    client: Mutex<McpClient>,
    root: PathBuf,
    // Kept alive so the spawned `zindeks serve` process is killed on drop;
    // unused otherwise.
    #[allow(dead_code)]
    child: Option<Child>,
}

impl ZindeksAdapter {
    /// Connects using `cfg`, performing the MCP handshake before returning.
    ///
    /// - `transport = "stdio"` spawns `cfg.command serve` and speaks MCP over
    ///   its stdin/stdout.
    /// - `transport = "tcp"` connects to `cfg.tcp_addr`.
    pub async fn connect(cfg: &ZindeksConfig, root: &Path) -> Result<Self> {
        match cfg.transport.as_str() {
            "stdio" => {
                let mut child = spawn_stdio_child(cfg).await?;
                let stdin = child.stdin.take().ok_or_else(|| {
                    IntelError::Unavailable("zindeks child missing stdin".to_string())
                })?;
                let stdout = child.stdout.take().ok_or_else(|| {
                    IntelError::Unavailable("zindeks child missing stdout".to_string())
                })?;

                let mut client = McpClient::new(stdout, stdin);
                client.initialize().await?;

                Ok(Self {
                    client: Mutex::new(client),
                    root: root.to_path_buf(),
                    child: Some(child),
                })
            }
            "tcp" => {
                let stream = TcpStream::connect(&cfg.tcp_addr).await.map_err(|e| {
                    IntelError::Unavailable(format!(
                        "cannot connect to zindeks at {}: {e}",
                        cfg.tcp_addr
                    ))
                })?;
                let (read_half, write_half) = stream.into_split();
                let mut client = McpClient::new(read_half, write_half);
                client.initialize().await?;

                Ok(Self {
                    client: Mutex::new(client),
                    root: root.to_path_buf(),
                    child: None,
                })
            }
            other => Err(IntelError::Unavailable(format!(
                "unknown zindeks transport: {other}"
            ))),
        }
    }

    /// Test/DI constructor: wraps arbitrary reader/writer halves (e.g.
    /// `tokio::io::duplex`). Does not perform the handshake — call
    /// [`ZindeksAdapter::initialize`] first.
    pub fn from_transport(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            client: Mutex::new(McpClient::new(reader, writer)),
            root: root.into(),
            child: None,
        }
    }

    /// Performs the MCP handshake. Required after [`Self::from_transport`];
    /// already done by [`Self::connect`].
    pub async fn initialize(&self) -> Result<()> {
        let mut client = self.client.lock().await;
        client.initialize().await.map_err(IntelError::from)
    }

    /// Whether `root` is already indexed by the connected zindeks instance
    /// (via `list_projects`), without binding/refreshing it.
    pub async fn is_indexed(&self) -> Result<bool> {
        let text = self.call_tool_text("list_projects", json!({})).await?;
        let value = parse_tool_json(&text)?;
        let projects = value.as_array().cloned().unwrap_or_default();
        let target = normalize_path(&self.root.to_string_lossy());
        Ok(projects.iter().any(|p| {
            p.get("root")
                .and_then(|r| r.as_str())
                .map(normalize_path)
                .as_deref()
                == Some(target.as_str())
        }))
    }

    /// Binds/refreshes `root` as the active project (`index_repository`).
    /// Callers should check [`Self::is_indexed`] first — this method does
    /// not gate on it, so calling it directly would perform a first-time
    /// index. Prefer [`Self::ensure_bound`].
    pub async fn bind_project(&self) -> Result<Value> {
        let text = self
            .call_tool_text(
                "index_repository",
                json!({"path": self.root.to_string_lossy()}),
            )
            .await?;
        parse_tool_json(&text)
    }

    /// Binds `root` for this session, but only if it is already indexed.
    /// Never triggers a first-time index (that requires explicit user
    /// action via `zindeks index .`).
    pub async fn ensure_bound(&self) -> Result<()> {
        if self.is_indexed().await? {
            self.bind_project().await?;
            Ok(())
        } else {
            Err(IntelError::NotIndexed(self.root.display().to_string()))
        }
    }

    async fn call_tool_text(&self, name: &str, arguments: Value) -> Result<String> {
        let mut client = self.client.lock().await;
        match client.call_tool(name, arguments).await {
            Ok(text) => Ok(text),
            Err(kode_mcp::McpError::Tool(msg)) if is_not_indexed_error(&msg) => {
                Err(IntelError::NotIndexed(self.root.display().to_string()))
            }
            Err(e) => Err(IntelError::from(e)),
        }
    }
}

/// Spawns `cfg.command serve` for the stdio transport. When `cfg.command` is
/// the default (`"zindeks"`, meaning the caller never overrode it) and that
/// spawn fails — most likely because zindeks isn't on `PATH` — retries once
/// against the managed binary Kode's `kode setup` installs
/// (`<managed_bin_dir>/zindeks[.exe]`) before giving up.
async fn spawn_stdio_child(cfg: &ZindeksConfig) -> Result<Child> {
    let mut cmd = tokio::process::Command::new(&cfg.command);
    cmd.arg("serve");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.kill_on_drop(true);

    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(first_err) if cfg.command == "zindeks" => {
            let managed = kode_core::managed_bin_dir().map(|dir| {
                let bin = if cfg!(windows) {
                    "zindeks.exe"
                } else {
                    "zindeks"
                };
                dir.join(bin)
            });

            let Some(managed_path) = managed else {
                return Err(IntelError::Unavailable(format!(
                    "cannot start {}: {first_err} — run `kode setup` to install zindeks",
                    cfg.command
                )));
            };

            let mut fallback_cmd = tokio::process::Command::new(&managed_path);
            fallback_cmd.arg("serve");
            fallback_cmd.stdin(Stdio::piped());
            fallback_cmd.stdout(Stdio::piped());
            fallback_cmd.stderr(Stdio::null());
            fallback_cmd.kill_on_drop(true);

            fallback_cmd.spawn().map_err(|_| {
                IntelError::Unavailable(format!(
                    "cannot start {}: {first_err} — run `kode setup` to install zindeks",
                    cfg.command
                ))
            })
        }
        Err(e) => Err(IntelError::Unavailable(format!(
            "cannot start {}: {e} — run `kode setup` to install zindeks or set [zindeks] command",
            cfg.command
        ))),
    }
}

fn is_not_indexed_error(msg: &str) -> bool {
    msg.contains("No project loaded") || msg.contains("NO_PROJECT")
}

fn normalize_path(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Parses a zindeks tool's JSON text payload, tolerating a known zindeks
/// 0.9.2 quirk on Windows: some tools (observed on `index_repository`'s
/// echoed `project` field) embed raw OS paths without escaping backslashes,
/// producing technically-invalid JSON (e.g. `"C:\Users\..."` instead of
/// `"C:\\Users\\..."`). Other tools (`list_projects`, `search`,
/// `file_outline`, `health_check`) were verified to escape correctly.
///
/// Strategy: try strict parsing first (the common, correct case); only on
/// failure, heuristically double any backslash that isn't already part of a
/// valid JSON escape sequence and retry once. This can misinterpret a path
/// segment that happens to start with `r`, `n`, `t`, `b`, `f`, or a valid
/// `\uXXXX` run as a real control/unicode escape, but it never changes
/// well-formed payloads and never throws where strict parsing would have
/// succeeded — it only widens what we can recover from.
fn parse_tool_json(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Ok(v);
    }
    let repaired = repair_stray_backslashes(text);
    serde_json::from_str(&repaired)
        .map_err(|e| IntelError::Protocol(format!("invalid tool json: {e}")))
}

fn repair_stray_backslashes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            let valid_simple = matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't');
            let valid_unicode = next == 'u'
                && i + 6 <= chars.len()
                && chars[i + 2..i + 6].iter().all(|c| c.is_ascii_hexdigit());
            if valid_simple || valid_unicode {
                out.push('\\');
            } else {
                out.push('\\');
                out.push('\\');
            }
            out.push(next);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[async_trait::async_trait]
impl CodeIntelligence for ZindeksAdapter {
    async fn health(&self) -> error::Result<IntelHealth> {
        let text = self.call_tool_text("health_check", json!({})).await?;
        let v = parse_tool_json(&text)?;

        let status = v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();
        let counts = v.get("counts").cloned().unwrap_or_default();
        let get_count = |key: &str| counts.get(key).and_then(|n| n.as_u64()).unwrap_or(0);

        Ok(IntelHealth {
            status,
            documents: get_count("documents"),
            symbols: get_count("symbols"),
            edges: get_count("edges"),
        })
    }

    async fn get_context(&self, request: CodeContextRequest) -> error::Result<CodeContext> {
        let mut args = serde_json::Map::new();
        args.insert("query".to_string(), json!(request.query));
        if !request.working_set.is_empty() {
            args.insert("working_set".to_string(), json!(request.working_set));
        }
        if let Some(max_tokens) = request.max_tokens {
            args.insert("max_tokens".to_string(), json!(max_tokens));
        }

        let text = self
            .call_tool_text("get_context", Value::Object(args))
            .await?;
        let v = parse_tool_json(&text)?;

        let text = v
            .get("context")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();
        let token_estimate = v
            .get("token_estimate")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32;

        Ok(CodeContext {
            text,
            token_estimate,
        })
    }

    async fn search(&self, query: &str, limit: u32) -> error::Result<Vec<CodeSearchResult>> {
        let text = self
            .call_tool_text("search", json!({"query": query, "limit": limit}))
            .await?;
        let value = parse_tool_json(&text)?;
        let rows = value.as_array().cloned().unwrap_or_default();

        Ok(rows
            .into_iter()
            .map(|row| {
                let path = row
                    .get("p")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let snippet = row
                    .get("x")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let score = row
                    .get("fused_score")
                    .and_then(|n| n.as_f64())
                    .unwrap_or(0.0);
                CodeSearchResult {
                    path,
                    snippet,
                    score,
                }
            })
            .collect())
    }

    async fn file_outline(&self, path: &str) -> error::Result<FileOutline> {
        let text = self
            .call_tool_text("file_outline", json!({"path": path}))
            .await?;
        let v = parse_tool_json(&text)?;

        let out_path = v
            .get("path")
            .and_then(|s| s.as_str())
            .unwrap_or(path)
            .to_string();
        let symbols = v
            .get("symbols")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|s| OutlineSymbol {
                name: s
                    .get("n")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                kind: s
                    .get("k")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                line: s.get("l").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                line_end: s.get("e").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            })
            .collect();

        Ok(FileOutline {
            path: out_path,
            symbols,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    /// Spins up a `ZindeksAdapter` over a duplex pipe, already initialized,
    /// plus the server-side handle a fake-server task drives.
    async fn adapter_pair(
        root: &str,
    ) -> (
        ZindeksAdapter,
        BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (client_r, mut server_w) = tokio::io::duplex(64 * 1024);
        let (server_r, client_w) = tokio::io::duplex(64 * 1024);
        let adapter = ZindeksAdapter::from_transport(client_r, client_w, PathBuf::from(root));
        let mut server_r = BufReader::new(server_r);

        // Drive the handshake inline before handing control to the caller's
        // fake-server task, so `adapter.initialize()` can run concurrently.
        let init = adapter.initialize();
        let handshake = async {
            let req = read_line(&mut server_r).await;
            assert_eq!(req["method"], "initialize");
            write_line(
                &mut server_w,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": {}}),
            )
            .await;
            let notif = read_line(&mut server_r).await;
            assert_eq!(notif["method"], "notifications/initialized");
        };
        let (init_result, ()) = tokio::join!(init, handshake);
        init_result.unwrap();

        (adapter, server_r, server_w)
    }

    async fn read_line(r: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn write_line(w: &mut tokio::io::DuplexStream, value: &Value) {
        let mut s = serde_json::to_string(value).unwrap();
        s.push('\n');
        w.write_all(s.as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    }

    async fn respond_tool_call(
        server_r: &mut BufReader<tokio::io::DuplexStream>,
        server_w: &mut tokio::io::DuplexStream,
        expected_tool: &str,
        payload: &str,
    ) {
        let req = read_line(server_r).await;
        assert_eq!(req["method"], "tools/call");
        assert_eq!(req["params"]["name"], expected_tool);
        write_line(
            server_w,
            &json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {"content": [{"type": "text", "text": payload}]}
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn health_parses_live_payload_shape() {
        let (adapter, mut server_r, mut server_w) =
            adapter_pair(r"C:\Users\sutan\Documents\Programing\rust\Kode").await;

        let payload = r#"{"status":"healthy","counts":{"documents":38,"symbols":230,"edges":400,"embeddings":38,"communities":0},"last_indexed":1786802246,"uptime_seconds":0,"cache_hits":0,"cache_misses":1}"#;
        let server = tokio::spawn(async move {
            respond_tool_call(&mut server_r, &mut server_w, "health_check", payload).await;
        });

        let health = adapter.health().await.unwrap();
        assert_eq!(health.status, "healthy");
        assert_eq!(health.documents, 38);
        assert_eq!(health.symbols, 230);
        assert_eq!(health.edges, 400);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn search_parses_live_payload_shape() {
        let (adapter, mut server_r, mut server_w) =
            adapter_pair(r"C:\Users\sutan\Documents\Programing\rust\Kode").await;

        let payload = r#"[{"doc_id":30,"p":"crates\\kode-tools\\src\\permission.rs","bm25_score":6.59,"semantic_score":0,"fused_score":0.0164,"x":"snippet text"}]"#;
        let server = tokio::spawn(async move {
            respond_tool_call(&mut server_r, &mut server_w, "search", payload).await;
        });

        let results = adapter.search("permission", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "crates\\kode-tools\\src\\permission.rs");
        assert_eq!(results[0].snippet, "snippet text");
        assert!((results[0].score - 0.0164).abs() < 1e-9);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn file_outline_parses_live_payload_shape() {
        let (adapter, mut server_r, mut server_w) =
            adapter_pair(r"C:\Users\sutan\Documents\Programing\rust\Kode").await;

        let payload = r#"{"path":"crates/kode-agent/src/lib.rs","count":1,"symbols":[{"n":"Agent","k":"struct_type","l":22,"e":28}]}"#;
        let server = tokio::spawn(async move {
            respond_tool_call(&mut server_r, &mut server_w, "file_outline", payload).await;
        });

        let outline = adapter
            .file_outline("crates/kode-agent/src/lib.rs")
            .await
            .unwrap();
        assert_eq!(outline.symbols.len(), 1);
        assert_eq!(outline.symbols[0].name, "Agent");
        assert_eq!(outline.symbols[0].kind, "struct_type");
        assert_eq!(outline.symbols[0].line, 22);
        assert_eq!(outline.symbols[0].line_end, 28);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn get_context_is_passed_through_verbatim() {
        let (adapter, mut server_r, mut server_w) =
            adapter_pair(r"C:\Users\sutan\Documents\Programing\rust\Kode").await;

        let payload = "{\"context\":\"## Search Results for ...\\nsome markdown\",\"token_estimate\":1381,\"max_tokens\":1200}";
        let server = tokio::spawn(async move {
            respond_tool_call(&mut server_r, &mut server_w, "get_context", payload).await;
        });

        let ctx = adapter
            .get_context(CodeContextRequest {
                query: "how does status work".to_string(),
                working_set: vec![],
                max_tokens: None,
            })
            .await
            .unwrap();
        assert_eq!(ctx.text, "## Search Results for ...\nsome markdown");
        assert_eq!(ctx.token_estimate, 1381);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn is_indexed_true_when_root_present_in_list_projects() {
        let root = r"C:\Users\sutan\Documents\Programing\rust\Kode";
        let (adapter, mut server_r, mut server_w) = adapter_pair(root).await;

        let payload = format!(
            r#"[{{"root":"{}","project_id":"kode-abc","current_segment":"main","updated_at":1786802246,"zindeks_version":1}}]"#,
            root.replace('\\', "\\\\")
        );
        let server = tokio::spawn(async move {
            respond_tool_call(&mut server_r, &mut server_w, "list_projects", &payload).await;
        });

        assert!(adapter.is_indexed().await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn is_indexed_false_when_root_absent() {
        let (adapter, mut server_r, mut server_w) =
            adapter_pair(r"C:\Users\sutan\Documents\Programing\rust\Kode").await;

        let payload = r#"[{"root":"C:\\Users\\sutan\\Documents\\Programing\\rust\\other","project_id":"other","current_segment":"main","updated_at":1,"zindeks_version":1}]"#;
        let server = tokio::spawn(async move {
            respond_tool_call(&mut server_r, &mut server_w, "list_projects", payload).await;
        });

        assert!(!adapter.is_indexed().await.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn no_project_loaded_error_maps_to_not_indexed() {
        let (adapter, mut server_r, mut server_w) =
            adapter_pair(r"C:\Users\sutan\Documents\Programing\rust\Kode").await;

        let server = tokio::spawn(async move {
            let req = read_line(&mut server_r).await;
            assert_eq!(req["method"], "tools/call");
            write_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "error": {"code": -32000, "message": "NO_PROJECT: No project loaded. Run index_repository first."}
                }),
            )
            .await;
        });

        let err = adapter.health().await.unwrap_err();
        assert!(matches!(err, IntelError::NotIndexed(_)));
        server.await.unwrap();
    }
}
