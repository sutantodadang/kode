use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{KodeError, Result};

fn default_model_provider() -> String {
    "openai".to_string()
}

fn default_model_name() -> String {
    String::new()
}

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    #[serde(default = "default_model_provider")]
    pub provider: String,
    #[serde(default = "default_model_name")]
    pub model: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: default_model_provider(),
            model: default_model_name(),
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
}

impl Default for ZindeksConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            transport: default_zindeks_transport(),
            command: default_zindeks_command(),
            tcp_addr: default_zindeks_tcp_addr(),
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            max_tool_calls: default_max_tool_calls(),
            max_context_tokens: default_max_context_tokens(),
            context_budget_tokens: default_context_budget_tokens(),
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
        assert!(cfg.zindeks.enabled);
        assert_eq!(cfg.zindeks.transport, "stdio");
        assert_eq!(cfg.zindeks.command, "zindeks");
        assert_eq!(cfg.zindeks.tcp_addr, "127.0.0.1:7717");
        assert!(cfg.ingat.enabled);
        assert_eq!(cfg.ingat.url, "http://127.0.0.1:3200");
        assert_eq!(cfg.agent.max_iterations, 40);
        assert_eq!(cfg.agent.max_tool_calls, 100);
        assert_eq!(cfg.agent.max_context_tokens, 100_000);
        assert_eq!(cfg.agent.context_budget_tokens, 16_000);
        assert_eq!(cfg.permissions.default_mode, PermissionMode::Ask);
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
    fn load_invalid_toml_errors() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(kode_dir.join("config.toml"), "not = [valid toml").unwrap();

        let err = KodeConfig::load(&dir).unwrap_err();
        assert!(matches!(err, KodeError::Config { .. }));
    }
}
