use std::path::PathBuf;

pub use kode_core::cancel::CancellationToken;

pub mod error;
pub mod path;
pub mod permission;
pub mod proc;
pub mod registry;
pub mod tools;

pub use error::{Result, ToolError};

/// Context handed to every tool invocation.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredPermission {
    ReadOnly,
    Mutating,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the arguments object.
    fn parameters(&self) -> serde_json::Value;
    fn required_permission(&self) -> RequiredPermission;
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

/// Test-only support shared across modules. `ENV_LOCK` serializes tests that
/// mutate process-global environment variables so they don't race each other
/// when `cargo test` runs them in parallel threads within this crate's test
/// binary.
#[cfg(test)]
pub(crate) mod test_support {
    pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
