//! Backends that outlive a single task.
//!
//! Opening the code-intelligence engine, the memory store and every MCP
//! server at the start of each task made every turn pay for setup the
//! previous turn had already done. `SessionRuntime` keeps them open, keyed
//! by the config that produced them, and reopens one when its config
//! changes. Failures are never kept: the next task tries again.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kode_core::config::{IngatConfig, McpConfig, ZindeksConfig};
use kode_intel::CodeIntelligence;
use kode_mcp::McpManager;
use kode_memory::EngineeringMemory;
use kode_tools::Tool;

/// A code-intelligence backend plus whether this call opened it. A fresh
/// handle still has to be bound to the repository.
pub struct IntelHandle {
    pub backend: Arc<dyn CodeIntelligence>,
    pub fresh: bool,
}

struct McpEntry {
    config: McpConfig,
    // Owns the server processes; dropping it kills them.
    manager: McpManager,
    /// Every enabled server connected. An incomplete set is never reused.
    complete: bool,
}

#[derive(Default)]
struct Inner {
    intel: Option<(ZindeksConfig, PathBuf, Arc<dyn CodeIntelligence>)>,
    memory: Option<(IngatConfig, Arc<dyn EngineeringMemory>)>,
    mcp: Option<McpEntry>,
}

pub struct SessionRuntime {
    // Tasks run one at a time, so holding this across a backend open is fine.
    inner: tokio::sync::Mutex<Inner>,
}

impl std::fmt::Debug for SessionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionRuntime")
    }
}

impl SessionRuntime {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(Inner::default()),
        }
    }

    /// The code-intelligence backend for `cwd`, reused while `cfg` and `cwd`
    /// are unchanged. `None` when zindeks is disabled.
    pub async fn intel(
        &self,
        cfg: &ZindeksConfig,
        cwd: &Path,
    ) -> anyhow::Result<Option<IntelHandle>> {
        let mut inner = self.inner.lock().await;
        if !cfg.enabled {
            inner.intel = None;
            return Ok(None);
        }
        if let Some((cached_cfg, cached_cwd, backend)) = &inner.intel
            && cached_cfg == cfg
            && cached_cwd == cwd
        {
            return Ok(Some(IntelHandle {
                backend: backend.clone(),
                fresh: false,
            }));
        }
        // Close the old engine before opening another on the same store.
        inner.intel = None;
        let Some(backend) = crate::intel_backend::connect(cfg, cwd).await? else {
            return Ok(None);
        };
        inner.intel = Some((cfg.clone(), cwd.to_path_buf(), backend.clone()));
        Ok(Some(IntelHandle {
            backend,
            fresh: true,
        }))
    }

    /// Drops the kept backend, e.g. after it failed to bind, so the next
    /// task opens a new one.
    pub async fn forget_intel(&self) {
        self.inner.lock().await.intel = None;
    }

    /// The memory backend, reused while `cfg` is unchanged. `None` when
    /// memory is disabled.
    pub async fn memory(
        &self,
        cfg: &IngatConfig,
    ) -> anyhow::Result<Option<Arc<dyn EngineeringMemory>>> {
        let mut inner = self.inner.lock().await;
        if !cfg.enabled {
            inner.memory = None;
            return Ok(None);
        }
        if let Some((cached_cfg, backend)) = &inner.memory
            && cached_cfg == cfg
        {
            return Ok(Some(backend.clone()));
        }
        inner.memory = None;
        let Some(backend) = crate::memory_backend::connect(cfg).await? else {
            return Ok(None);
        };
        inner.memory = Some((cfg.clone(), backend.clone()));
        Ok(Some(backend))
    }

    /// Drops the kept memory backend so the next task reopens it.
    pub async fn forget_memory(&self) {
        self.inner.lock().await.memory = None;
    }

    /// Tools of every connected MCP server, in a stable order. The server
    /// processes are reused while `cfg` is unchanged, every enabled server
    /// connected, and every process is still running; otherwise all of them
    /// are restarted. `notes` receives one line per server, only when a
    /// connection is attempted.
    pub async fn mcp_tools(&self, cfg: &McpConfig, notes: &mut Vec<String>) -> Vec<Arc<dyn Tool>> {
        let mut inner = self.inner.lock().await;
        if cfg.servers.is_empty() {
            inner.mcp = None;
            return Vec::new();
        }

        let reusable = match &mut inner.mcp {
            Some(entry) => {
                entry.config == *cfg
                    && entry.complete
                    && entry
                        .manager
                        .handles
                        .iter_mut()
                        .all(|handle| handle.is_alive())
            }
            None => false,
        };
        if !reusable {
            // Kill the old processes before starting their replacements.
            inner.mcp = None;
            let manager = McpManager::connect_all(&cfg.servers, notes).await;
            let enabled = cfg.servers.values().filter(|server| server.enabled).count();
            let complete = manager.handles.len() == enabled;
            inner.mcp = Some(McpEntry {
                config: cfg.clone(),
                manager,
                complete,
            });
        }

        inner
            .mcp
            .as_ref()
            .map(|entry| {
                entry
                    .manager
                    .handles
                    .iter()
                    .flat_map(|handle| handle.tools.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_core::config::McpServerConfig;

    fn temp_store(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kode-session-runtime-{label}-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn same_backend(a: &Arc<dyn EngineeringMemory>, b: &Arc<dyn EngineeringMemory>) -> bool {
        std::ptr::eq(Arc::as_ptr(a) as *const (), Arc::as_ptr(b) as *const ())
    }

    #[tokio::test]
    async fn disabled_backends_yield_nothing() {
        let runtime = SessionRuntime::new();
        let zindeks = ZindeksConfig {
            enabled: false,
            ..ZindeksConfig::default()
        };
        let ingat = IngatConfig {
            enabled: false,
            ..IngatConfig::default()
        };

        assert!(
            runtime
                .intel(&zindeks, Path::new("."))
                .await
                .unwrap()
                .is_none()
        );
        assert!(runtime.memory(&ingat).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_is_reused_for_the_same_config_and_reopened_for_a_new_one() {
        let runtime = SessionRuntime::new();
        let first_path = temp_store("a");
        let second_path = temp_store("b");
        let first_cfg = IngatConfig {
            store_path: Some(first_path.clone()),
            ..IngatConfig::default()
        };
        let second_cfg = IngatConfig {
            store_path: Some(second_path.clone()),
            ..IngatConfig::default()
        };

        let a = runtime.memory(&first_cfg).await.unwrap().unwrap();
        let b = runtime.memory(&first_cfg).await.unwrap().unwrap();
        assert!(
            same_backend(&a, &b),
            "same config must reuse the open store"
        );

        let c = runtime.memory(&second_cfg).await.unwrap().unwrap();
        assert!(!same_backend(&a, &c), "a changed config must reopen");

        runtime.forget_memory().await;
        let d = runtime.memory(&second_cfg).await.unwrap().unwrap();
        assert!(!same_backend(&c, &d), "forgetting must force a reopen");

        drop((a, b, c, d));
        drop(runtime);
        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
    }

    #[tokio::test]
    async fn no_mcp_servers_means_no_tools_and_no_notes() {
        let runtime = SessionRuntime::new();
        let mut notes = Vec::new();
        let tools = runtime.mcp_tools(&McpConfig::default(), &mut notes).await;
        assert!(tools.is_empty());
        assert!(notes.is_empty());
    }

    #[tokio::test]
    async fn a_failed_mcp_connection_is_retried_on_the_next_task() {
        let runtime = SessionRuntime::new();
        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "broken".to_string(),
            McpServerConfig {
                command: "kode-test-definitely-not-a-real-binary".to_string(),
                args: vec![],
                enabled: true,
            },
        );

        let mut first = Vec::new();
        assert!(runtime.mcp_tools(&cfg, &mut first).await.is_empty());
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].contains("broken"));

        // Reuse would stay silent. A retry reports the failure again.
        let mut second = Vec::new();
        assert!(runtime.mcp_tools(&cfg, &mut second).await.is_empty());
        assert_eq!(second.len(), 1, "{second:?}");
    }

    #[tokio::test]
    async fn disabled_mcp_servers_do_not_count_as_failures() {
        let runtime = SessionRuntime::new();
        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "off".to_string(),
            McpServerConfig {
                command: "kode-test-definitely-not-a-real-binary".to_string(),
                args: vec![],
                enabled: false,
            },
        );

        let mut notes = Vec::new();
        assert!(runtime.mcp_tools(&cfg, &mut notes).await.is_empty());
        assert!(notes.is_empty());
    }
}
