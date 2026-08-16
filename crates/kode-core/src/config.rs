use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{KodeError, Result};

fn default_model_provider() -> String {
    "openai".to_string()
}

fn default_model_name() -> String {
    String::new()
}

fn default_model_effort() -> String {
    String::new()
}

/// Valid values for `model.effort` / `--effort` / `/effort`, in ascending
/// order of reasoning depth.
pub const VALID_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

fn default_true() -> bool {
    true
}

fn default_zindeks_transport() -> String {
    "stdio".to_string()
}

fn default_zindeks_command() -> String {
    "zindeks".to_string()
}

fn default_zindeks_tcp_addr() -> String {
    "127.0.0.1:7717".to_string()
}

fn default_zindeks_watch() -> bool {
    true
}

fn default_ingat_url() -> String {
    "http://127.0.0.1:3200".to_string()
}

fn default_max_iterations() -> u32 {
    40
}

fn default_max_tool_calls() -> u32 {
    100
}

fn default_max_context_tokens() -> u32 {
    100_000
}

fn default_context_budget_tokens() -> u32 {
    16_000
}

fn default_history_budget_tokens() -> u32 {
    6000
}

fn default_permission_mode() -> PermissionMode {
    PermissionMode::Ask
}

/// Top-level Kode configuration, loaded from `.kode/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KodeConfig {
    pub model: ModelConfig,
    pub zindeks: ZindeksConfig,
    pub ingat: IngatConfig,
    pub agent: AgentConfig,
    pub permissions: PermissionsConfig,
    pub mcp: McpConfig,
}

impl KodeConfig {
    /// Path to the config file for a given project root.
    pub fn config_path(project_root: &Path) -> PathBuf {
        project_root.join(".kode").join("config.toml")
    }

    /// Loads config from `<project_root>/.kode/config.toml`.
    ///
    /// A missing file yields defaults. An unreadable or invalid file yields
    /// `KodeError::Config`.
    pub fn load(project_root: &Path) -> Result<Self> {
        let path = Self::config_path(project_root);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(KodeError::Config {
                    path,
                    message: err.to_string(),
                });
            }
        };

        toml::from_str(&content).map_err(|err| KodeError::Config {
            path,
            message: err.to_string(),
        })
    }

    /// Updates `[model].provider`, `[model].model`, and/or `[model].effort`
    /// in `<project_root>/.kode/config.toml`, preserving every other key
    /// (including unknown/future ones). Creates the directory and file if
    /// missing. Passing `None` for any argument leaves that key untouched.
    pub fn update_model_config(
        project_root: &Path,
        provider: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<()> {
        let path = Self::config_path(project_root);

        let mut root: toml::Value = match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).map_err(|err| KodeError::Config {
                path: path.clone(),
                message: err.to_string(),
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(toml::value::Table::new())
            }
            Err(err) => {
                return Err(KodeError::Config {
                    path,
                    message: err.to_string(),
                });
            }
        };

        let table = root.as_table_mut().ok_or_else(|| KodeError::Config {
            path: path.clone(),
            message: "config root is not a table".to_string(),
        })?;

        let model_table = table
            .entry("model")
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()))
            .as_table_mut()
            .ok_or_else(|| KodeError::Config {
                path: path.clone(),
                message: "[model] is not a table".to_string(),
            })?;

        if let Some(p) = provider {
            model_table.insert("provider".to_string(), toml::Value::String(p.to_string()));
        }
        if let Some(m) = model {
            model_table.insert("model".to_string(), toml::Value::String(m.to_string()));
        }
        if let Some(e) = effort {
            model_table.insert("effort".to_string(), toml::Value::String(e.to_string()));
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| KodeError::Config {
                path: path.clone(),
                message: err.to_string(),
            })?;
        }

        let serialized = toml::to_string_pretty(&root).map_err(|err| KodeError::Config {
            path: path.clone(),
            message: err.to_string(),
        })?;
        std::fs::write(&path, serialized).map_err(|err| KodeError::Config {
            path,
            message: err.to_string(),
        })?;

        Ok(())
    }

    /// Updates `[model].model` and/or `[model].effort` in
    /// `<project_root>/.kode/config.toml`, preserving every other key.
    /// Thin wrapper over [`Self::update_model_config`] with `provider: None`.
    pub fn update_model_selection(
        project_root: &Path,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<()> {
        Self::update_model_config(project_root, None, model, effort)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    #[serde(default = "default_model_provider")]
    pub provider: String,
    #[serde(default = "default_model_name")]
    pub model: String,
    #[serde(default = "default_model_effort")]
    pub effort: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: default_model_provider(),
            model: default_model_name(),
            effort: default_model_effort(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZindeksConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// "stdio" (default, spawns `command`) or "tcp" (connects to `tcp_addr`).
    #[serde(default = "default_zindeks_transport")]
    pub transport: String,
    /// Binary spawned for stdio transport.
    #[serde(default = "default_zindeks_command")]
    pub command: String,
    /// Address used for tcp transport.
    #[serde(default = "default_zindeks_tcp_addr")]
    pub tcp_addr: String,
    /// Whether to enable zindeks's built-in poll-watcher (`ZINDEKS_WATCH=1`)
    /// on the stdio child Kode spawns, so the index refreshes itself in the
    /// background instead of via Kode's post-task `ensure_bound` call. Only
    /// takes effect for `transport = "stdio"` — Kode doesn't control TCP
    /// servers, so it can't enable their watcher.
    #[serde(default = "default_zindeks_watch")]
    pub watch: bool,
}

impl Default for ZindeksConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            transport: default_zindeks_transport(),
            command: default_zindeks_command(),
            tcp_addr: default_zindeks_tcp_addr(),
            watch: default_zindeks_watch(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IngatConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_ingat_url")]
    pub url: String,
}

impl Default for IngatConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            url: default_ingat_url(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: u32,
    #[serde(default = "default_context_budget_tokens")]
    pub context_budget_tokens: u32,
    #[serde(default = "default_history_budget_tokens")]
    pub history_budget_tokens: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            max_tool_calls: default_max_tool_calls(),
            max_context_tokens: default_max_context_tokens(),
            context_budget_tokens: default_context_budget_tokens(),
            history_budget_tokens: default_history_budget_tokens(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    #[serde(rename = "default", default = "default_permission_mode")]
    pub default_mode: PermissionMode,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            default_mode: default_permission_mode(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Allow,
    #[default]
    Ask,
    Deny,
}

/// Configuration for user-defined external MCP (Model Context Protocol)
/// servers, distinct from the first-class `[zindeks]`/`[ingat]` integrations.
/// Each entry's tools register into the tool runtime as `{server}__{tool}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// A single external MCP server, spawned as a stdio child process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Binary to spawn. No sensible default — required per server entry.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            enabled: default_true(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn temp_project_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-core-test-{}-{}-{}",
            std::process::id(),
            nanos(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_are_correct() {
        let cfg = KodeConfig::default();
        assert_eq!(cfg.model.provider, "openai");
        assert_eq!(cfg.model.model, "");
        assert_eq!(cfg.model.effort, "");
        assert!(cfg.zindeks.enabled);
        assert_eq!(cfg.zindeks.transport, "stdio");
        assert_eq!(cfg.zindeks.command, "zindeks");
        assert_eq!(cfg.zindeks.tcp_addr, "127.0.0.1:7717");
        assert!(cfg.zindeks.watch);
        assert!(cfg.ingat.enabled);
        assert_eq!(cfg.ingat.url, "http://127.0.0.1:3200");
        assert_eq!(cfg.agent.max_iterations, 40);
        assert_eq!(cfg.agent.max_tool_calls, 100);
        assert_eq!(cfg.agent.max_context_tokens, 100_000);
        assert_eq!(cfg.agent.context_budget_tokens, 16_000);
        assert_eq!(cfg.agent.history_budget_tokens, 6000);
        assert_eq!(cfg.permissions.default_mode, PermissionMode::Ask);
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn load_without_history_budget_tokens_defaults_to_6000() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            concat!("[agent]\n", "max_iterations = 5\n"),
        )
        .unwrap();

        let cfg = KodeConfig::load(&dir).unwrap();
        assert_eq!(cfg.agent.max_iterations, 5);
        assert_eq!(cfg.agent.history_budget_tokens, 6000);
    }

    #[test]
    fn load_parses_mcp_servers_table() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            concat!(
                "[mcp.servers.everything]\n",
                "command = \"npx\"\n",
                "args = [\"-y\", \"@modelcontextprotocol/server-everything\"]\n",
                "enabled = true\n",
            ),
        )
        .unwrap();

        let cfg = KodeConfig::load(&dir).unwrap();
        let server = cfg.mcp.servers.get("everything").unwrap();
        assert_eq!(server.command, "npx");
        assert_eq!(
            server.args,
            vec!["-y", "@modelcontextprotocol/server-everything"]
        );
        assert!(server.enabled);
    }

    #[test]
    fn mcp_server_enabled_defaults_to_true() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            "[mcp.servers.everything]\ncommand = \"npx\"\n",
        )
        .unwrap();

        let cfg = KodeConfig::load(&dir).unwrap();
        let server = cfg.mcp.servers.get("everything").unwrap();
        assert!(server.enabled);
        assert!(server.args.is_empty());
    }

    #[test]
    fn load_missing_config_returns_defaults() {
        let dir = temp_project_dir();
        let cfg = KodeConfig::load(&dir).unwrap();
        assert_eq!(cfg, KodeConfig::default());
    }

    #[test]
    fn load_overrides_from_file() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            "[agent]\nmax_iterations = 7\n\n[permissions]\ndefault = \"deny\"\n",
        )
        .unwrap();

        let cfg = KodeConfig::load(&dir).unwrap();
        assert_eq!(cfg.agent.max_iterations, 7);
        assert_eq!(cfg.permissions.default_mode, PermissionMode::Deny);
    }

    #[test]
    fn zindeks_watch_defaults_true_and_honors_explicit_false() {
        let dir = temp_project_dir();
        let cfg = KodeConfig::load(&dir).unwrap();
        assert!(cfg.zindeks.watch);

        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(kode_dir.join("config.toml"), "[zindeks]\nwatch = false\n").unwrap();

        let cfg = KodeConfig::load(&dir).unwrap();
        assert!(!cfg.zindeks.watch);
    }

    #[test]
    fn load_invalid_toml_errors() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(kode_dir.join("config.toml"), "not = [valid toml").unwrap();

        let err = KodeConfig::load(&dir).unwrap_err();
        assert!(matches!(err, KodeError::Config { .. }));
    }

    #[test]
    fn update_model_selection_creates_file() {
        let dir = temp_project_dir();
        assert!(!KodeConfig::config_path(&dir).exists());

        KodeConfig::update_model_selection(&dir, Some("gpt-5.6-sol"), Some("high")).unwrap();

        assert!(KodeConfig::config_path(&dir).exists());
        let cfg = KodeConfig::load(&dir).unwrap();
        assert_eq!(cfg.model.model, "gpt-5.6-sol");
        assert_eq!(cfg.model.effort, "high");
    }

    #[test]
    fn update_model_selection_preserves_unrelated_keys() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            "[agent]\nmax_iterations = 7\n\n[model]\nprovider = \"codex\"\n",
        )
        .unwrap();

        KodeConfig::update_model_selection(&dir, Some("gpt-5.6-sol"), None).unwrap();

        let cfg = KodeConfig::load(&dir).unwrap();
        assert_eq!(cfg.agent.max_iterations, 7);
        assert_eq!(cfg.model.provider, "codex");
        assert_eq!(cfg.model.model, "gpt-5.6-sol");
        assert_eq!(cfg.model.effort, "");
    }

    #[test]
    fn update_model_config_persists_provider_and_preserves_unknown_keys() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            "[agent]\nmax_iterations = 7\n\n[mcp.servers.everything]\ncommand = \"npx\"\n",
        )
        .unwrap();

        KodeConfig::update_model_config(&dir, Some("codex"), Some(""), None).unwrap();

        let cfg = KodeConfig::load(&dir).unwrap();
        assert_eq!(cfg.model.provider, "codex");
        assert_eq!(cfg.model.model, "");
        assert_eq!(cfg.agent.max_iterations, 7);
        assert_eq!(cfg.mcp.servers.get("everything").unwrap().command, "npx");
    }
}
