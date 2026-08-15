//! Generic MCP (Model Context Protocol) stdio client, tool adapter, and
//! server manager. Kept architecturally separate from the first-class
//! Zindeks/Ingat integrations (`kode-intel`, `kode-memory`), which consume
//! [`McpClient`] directly for their own narrow domain surfaces. This crate
//! is what powers configurable, user-defined external MCP servers whose
//! tools register into the tool runtime as `{server}__{tool}`.

pub mod client;
pub mod error;
pub mod manager;
pub mod tool;

pub use client::{McpClient, RemoteToolInfo};
pub use error::{McpError, Result};
pub use manager::{McpManager, McpServerHandle};
pub use tool::McpTool;
