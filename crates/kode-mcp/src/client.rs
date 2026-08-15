//! Minimal hand-rolled MCP JSON-RPC client over an arbitrary async byte
//! stream (newline-delimited JSON, one message per line — standard MCP
//! stdio framing). No MCP SDK dependency; we only need `initialize`,
//! `tools/list`, and `tools/call`.

use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::error::{McpError, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A tool advertised by a remote MCP server via `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
}

/// A connected-but-not-yet-initialized (or initialized) MCP client.
///
/// Requests are always awaited to completion before the next is issued, so
/// holding this behind a `tokio::sync::Mutex` and keeping the guard across
/// an `.await` (as adapters do) is safe: calls are inherently serialized by
/// the protocol, there is no independent progress to block.
pub struct McpClient {
    reader: BufReader<Pin<Box<dyn AsyncRead + Send>>>,
    writer: Pin<Box<dyn AsyncWrite + Send>>,
    next_id: i64,
}

impl McpClient {
    pub fn new(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self {
            reader: BufReader::new(Box::pin(reader)),
            writer: Box::pin(writer),
            next_id: 0,
        }
    }

    fn alloc_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    async fn write_line(&mut self, value: &Value) -> Result<()> {
        let mut line =
            serde_json::to_string(value).map_err(|e| McpError::Protocol(e.to_string()))?;
        tracing::debug!(target: "kode_mcp::client", %line, "-> server");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Reads lines until one is a response carrying `id`, skipping any
    /// server-initiated notifications/requests seen along the way.
    async fn read_response(&mut self, id: i64) -> Result<Value> {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(McpError::Protocol("connection closed".to_string()));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|e| McpError::Protocol(format!("invalid json: {e}")))?;
            tracing::debug!(target: "kode_mcp::client", %line, "<- server");

            let is_notification_or_request = value.get("method").is_some();
            let resp_id = value.get("id").and_then(|v| v.as_i64());
            if is_notification_or_request || resp_id != Some(id) {
                continue;
            }
            return Ok(value);
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.alloc_id();
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_line(&msg).await?;
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, self.read_response(id))
            .await
            .map_err(|_| McpError::Timeout)??;

        if let Some(err) = resp.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(McpError::Tool(message.to_string()));
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| McpError::Protocol("response missing result".to_string()))
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_line(&msg).await
    }

    /// MCP handshake: `initialize` request, then `notifications/initialized`.
    pub async fn initialize(&mut self) -> Result<()> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "kode", "version": env!("CARGO_PKG_VERSION")},
        });
        self.request("initialize", params).await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// Calls an MCP tool and returns the `content[0].text` payload string.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String> {
        let params = json!({"name": name, "arguments": arguments});
        let result = self.request("tools/call", params).await?;

        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            let text = extract_text(&result).unwrap_or_else(|| "tool call failed".to_string());
            return Err(McpError::Tool(text));
        }
        extract_text(&result)
            .ok_or_else(|| McpError::Protocol("missing content[0].text".to_string()))
    }

    /// Lists tools advertised by the connected server (`tools/list`).
    pub async fn list_tools(&mut self) -> Result<Vec<RemoteToolInfo>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(tools
            .into_iter()
            .map(|t| {
                let name = t
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let description = t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string();
                let input_schema = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                let read_only = t
                    .get("annotations")
                    .and_then(|a| a.get("readOnlyHint"))
                    .and_then(|v| v.as_bool())
                    == Some(true);
                RemoteToolInfo {
                    name,
                    description,
                    input_schema,
                    read_only,
                }
            })
            .collect())
    }
}

fn extract_text(result: &Value) -> Option<String> {
    result
        .get("content")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader as TokioBufReader;

    /// Wires up a client over an in-memory duplex pipe and hands back the
    /// server-side reader/writer for a fake-server task to drive.
    fn client_pair() -> (
        McpClient,
        TokioBufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (client_r, server_w) = tokio::io::duplex(64 * 1024);
        let (server_r, client_w) = tokio::io::duplex(64 * 1024);
        let client = McpClient::new(client_r, client_w);
        (client, TokioBufReader::new(server_r), server_w)
    }

    async fn read_server_line(server_r: &mut TokioBufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        server_r.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn write_server_line(server_w: &mut tokio::io::DuplexStream, value: &Value) {
        let mut s = serde_json::to_string(value).unwrap();
        s.push('\n');
        server_w.write_all(s.as_bytes()).await.unwrap();
        server_w.flush().await.unwrap();
    }

    #[tokio::test]
    async fn initialize_handshake_succeeds() {
        let (mut client, mut server_r, mut server_w) = client_pair();

        let server = tokio::spawn(async move {
            let req = read_server_line(&mut server_r).await;
            assert_eq!(req["method"], "initialize");
            let id = req["id"].clone();
            write_server_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {"name": "fake", "version": "0"}
                    }
                }),
            )
            .await;

            let notif = read_server_line(&mut server_r).await;
            assert_eq!(notif["method"], "notifications/initialized");
        });

        client.initialize().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_tool_returns_text_payload() {
        let (mut client, mut server_r, mut server_w) = client_pair();

        let server = tokio::spawn(async move {
            let req = read_server_line(&mut server_r).await;
            assert_eq!(req["method"], "tools/call");
            assert_eq!(req["params"]["name"], "health_check");
            let id = req["id"].clone();
            write_server_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"content": [{"type": "text", "text": "{\"ok\":true}"}]}
                }),
            )
            .await;
        });

        let text = client.call_tool("health_check", json!({})).await.unwrap();
        assert_eq!(text, "{\"ok\":true}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_tool_is_error_flag_maps_to_tool_error() {
        let (mut client, mut server_r, mut server_w) = client_pair();

        let server = tokio::spawn(async move {
            let req = read_server_line(&mut server_r).await;
            let id = req["id"].clone();
            write_server_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": "No project loaded"}],
                        "isError": true
                    }
                }),
            )
            .await;
        });

        let err = client.call_tool("search", json!({})).await.unwrap_err();
        assert!(matches!(err, McpError::Tool(msg) if msg == "No project loaded"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn top_level_jsonrpc_error_maps_to_tool_error() {
        let (mut client, mut server_r, mut server_w) = client_pair();

        let server = tokio::spawn(async move {
            let req = read_server_line(&mut server_r).await;
            let id = req["id"].clone();
            write_server_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32000, "message": "NO_PROJECT: No project loaded"}
                }),
            )
            .await;
        });

        let err = client.call_tool("search", json!({})).await.unwrap_err();
        assert!(matches!(err, McpError::Tool(msg) if msg.contains("NO_PROJECT")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unrelated_notification_before_response_is_skipped() {
        let (mut client, mut server_r, mut server_w) = client_pair();

        let server = tokio::spawn(async move {
            let req = read_server_line(&mut server_r).await;
            let id = req["id"].clone();
            // Server sends an unrelated notification first.
            write_server_line(
                &mut server_w,
                &json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}),
            )
            .await;
            write_server_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"content": [{"type": "text", "text": "ok"}]}
                }),
            )
            .await;
        });

        let text = client.call_tool("search", json!({})).await.unwrap();
        assert_eq!(text, "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn list_tools_parses_two_tools_one_read_only() {
        let (mut client, mut server_r, mut server_w) = client_pair();

        let server = tokio::spawn(async move {
            let req = read_server_line(&mut server_r).await;
            assert_eq!(req["method"], "tools/list");
            let id = req["id"].clone();
            write_server_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "search",
                                "description": "search things",
                                "inputSchema": {"type": "object", "properties": {}},
                                "annotations": {"readOnlyHint": true}
                            },
                            {
                                "name": "write_thing",
                                "description": "writes a thing",
                                "inputSchema": {"type": "object"},
                                "annotations": {"readOnlyHint": false}
                            }
                        ]
                    }
                }),
            )
            .await;
        });

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "search things");
        assert!(tools[0].read_only);
        assert_eq!(tools[1].name, "write_thing");
        assert!(!tools[1].read_only);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn list_tools_defaults_missing_description_and_schema() {
        let (mut client, mut server_r, mut server_w) = client_pair();

        let server = tokio::spawn(async move {
            let req = read_server_line(&mut server_r).await;
            let id = req["id"].clone();
            write_server_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {"name": "bare_tool"}
                        ]
                    }
                }),
            )
            .await;
        });

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "bare_tool");
        assert_eq!(tools[0].description, "");
        assert_eq!(tools[0].input_schema, json!({"type": "object"}));
        assert!(!tools[0].read_only);
        server.await.unwrap();
    }
}
