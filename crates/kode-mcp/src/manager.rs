//! Spawns and owns configured external MCP servers for the lifetime of a
//! run, registering each advertised remote tool into the tool runtime.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use kode_core::config::McpServerConfig;
use kode_tools::Tool;

use crate::client::McpClient;
use crate::error::McpError;
use crate::tool::McpTool;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A running MCP server child process plus its bound client and the tools it
/// advertised. Dropping this kills the child (`kill_on_drop`).
pub struct McpServerHandle {
    pub name: String,
    #[allow(dead_code)]
    client: Arc<Mutex<McpClient>>,
    // Kept alive so the spawned child is killed on drop; unused otherwise.
    #[allow(dead_code)]
    child: Child,
    pub tools: Vec<Arc<dyn Tool>>,
}

/// Owns every successfully connected MCP server for the run's duration.
pub struct McpManager {
    pub handles: Vec<McpServerHandle>,
}

impl McpManager {
    /// Spawns each enabled server, initializes it, and lists its tools.
    /// Per-server failures are reported via `notes` and never abort the
    /// overall call — a bad server just yields no tools.
    pub async fn connect_all(
        servers: &BTreeMap<String, McpServerConfig>,
        notes: &mut Vec<String>,
    ) -> Self {
        let mut handles = Vec::new();
        for (name, cfg) in servers {
            if !cfg.enabled {
                continue;
            }
            match tokio::time::timeout(CONNECT_TIMEOUT, connect_one(name, cfg)).await {
                Ok(Ok(handle)) => {
                    notes.push(format!("mcp server {name}: {} tools", handle.tools.len()));
                    handles.push(handle);
                }
                Ok(Err(e)) => {
                    notes.push(format!("mcp server {name} unavailable: {e}"));
                }
                Err(_) => {
                    notes.push(format!("mcp server {name} unavailable: request timed out"));
                }
            }
        }
        Self { handles }
    }
}

async fn connect_one(name: &str, cfg: &McpServerConfig) -> crate::error::Result<McpServerHandle> {
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| McpError::Unavailable(format!("cannot start {}: {e}", cfg.command)))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| McpError::Unavailable("mcp child missing stdin".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| McpError::Unavailable("mcp child missing stdout".to_string()))?;

    let mut client = McpClient::new(stdout, stdin);
    client.initialize().await?;
    let tool_infos = client.list_tools().await?;
    let client = Arc::new(Mutex::new(client));

    let tools: Vec<Arc<dyn Tool>> = tool_infos
        .into_iter()
        .map(|info| Arc::new(McpTool::new(name, info, client.clone())) as Arc<dyn Tool>)
        .collect();

    Ok(McpServerHandle {
        name: name.to_string(),
        client,
        child,
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_all_reports_note_and_stays_empty_on_bogus_command() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "bogus".to_string(),
            McpServerConfig {
                command: "this-binary-does-not-exist-kode-mcp-test".to_string(),
                args: vec![],
                enabled: true,
            },
        );

        let mut notes = Vec::new();
        let manager = McpManager::connect_all(&servers, &mut notes).await;

        assert!(manager.handles.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("bogus"));
        assert!(notes[0].contains("unavailable"));
    }

    #[tokio::test]
    async fn connect_all_skips_disabled_servers_without_a_note() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "disabled".to_string(),
            McpServerConfig {
                command: "whatever".to_string(),
                args: vec![],
                enabled: false,
            },
        );

        let mut notes = Vec::new();
        let manager = McpManager::connect_all(&servers, &mut notes).await;

        assert!(manager.handles.is_empty());
        assert!(notes.is_empty());
    }
}
