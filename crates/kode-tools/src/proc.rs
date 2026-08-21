//! Backwards-compatible re-export of the shared subprocess manager.

pub use kode_core::process::{ManagedChild, TreeGuard, managed_command, scrub_env, spawn_managed};
