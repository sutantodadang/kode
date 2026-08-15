//! Adapts a single remote MCP tool into `kode_tools::Tool`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use kode_tools::{
    RequiredPermission, Result as ToolResult, Tool, ToolContext, ToolError, ToolOutput,
};

use crate::client::{McpClient, RemoteToolInfo};
use crate::error::McpError;

/// A tool backed by a remote MCP server, registered into the tool runtime as
/// `{server}__{tool}`. The remote tool is invoked by its unqualified name.
pub struct McpTool {
    info: RemoteToolInfo,
    client: Arc<Mutex<McpClient>>,
    qualified_name: String,
    qualified_description: String,
}

impl McpTool {
    pub fn new(
        server: impl Into<String>,
        info: RemoteToolInfo,
        client: Arc<Mutex<McpClient>>,
    ) -> Self {
        let server = server.into();
        let qualified_name = format!("{server}__{}", info.name);
        let qualified_description = format!("[{server}] {}", info.description);
        Self {
            info,
            client,
            qualified_name,
            qualified_description,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.qualified_description
    }

    fn parameters(&self) -> Value {
        self.info.input_schema.clone()
    }

    fn required_permission(&self) -> RequiredPermission {
        if self.info.read_only {
            RequiredPermission::ReadOnly
        } else {
            RequiredPermission::Mutating
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult<ToolOutput> {
        let mut client = self.client.lock().await;
        match client.call_tool(&self.info.name, args).await {
            Ok(text) => Ok(ToolOutput { content: text }),
            Err(McpError::Timeout) => Err(ToolError::Timeout(Duration::from_secs(30))),
            Err(e) => Err(ToolError::Failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            cancel: kode_core::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn execute_round_trips_through_the_remote_tool_and_maps_permission() {
        let (client_r, mut server_w) = tokio::io::duplex(64 * 1024);
        let (server_r, client_w) = tokio::io::duplex(64 * 1024);
        let client = Arc::new(Mutex::new(McpClient::new(client_r, client_w)));
        let mut server_r = BufReader::new(server_r);

        let info = RemoteToolInfo {
            name: "search".to_string(),
            description: "search things".to_string(),
            input_schema: json!({"type": "object"}),
            read_only: true,
        };
        let tool = McpTool::new("everything", info, client);

        assert_eq!(tool.name(), "everything__search");
        assert_eq!(tool.description(), "[everything] search things");
        assert_eq!(tool.required_permission(), RequiredPermission::ReadOnly);

        let server = tokio::spawn(async move {
            let req = read_line(&mut server_r).await;
            assert_eq!(req["method"], "tools/call");
            // Remote call uses the unqualified tool name, not the qualified one.
            assert_eq!(req["params"]["name"], "search");
            write_line(
                &mut server_w,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": {"content": [{"type": "text", "text": "result text"}]}
                }),
            )
            .await;
        });

        let out = tool.execute(json!({"q": "x"}), &ctx()).await.unwrap();
        assert_eq!(out.content, "result text");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn mutating_tool_maps_permission_correctly() {
        let (client_r, _server_w) = tokio::io::duplex(64 * 1024);
        let (_server_r, client_w) = tokio::io::duplex(64 * 1024);
        let client = Arc::new(Mutex::new(McpClient::new(client_r, client_w)));

        let info = RemoteToolInfo {
            name: "write_thing".to_string(),
            description: "writes".to_string(),
            input_schema: json!({"type": "object"}),
            read_only: false,
        };
        let tool = McpTool::new("everything", info, client);
        assert_eq!(tool.required_permission(), RequiredPermission::Mutating);
    }
}
