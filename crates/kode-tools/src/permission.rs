use kode_core::config::PermissionMode;

use crate::RequiredPermission;

#[async_trait::async_trait]
pub trait PermissionHandler: Send + Sync {
    /// Ask the user to approve a Mutating tool invocation. `summary` is a
    /// one-line human-readable description (tool name + key args).
    async fn confirm(&self, summary: &str) -> bool;
}

pub struct AutoApprove;

#[async_trait::async_trait]
impl PermissionHandler for AutoApprove {
    async fn confirm(&self, _summary: &str) -> bool {
        true
    }
}

pub struct AutoDeny;

#[async_trait::async_trait]
impl PermissionHandler for AutoDeny {
    async fn confirm(&self, _summary: &str) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

pub fn decide(mode: PermissionMode, required: RequiredPermission) -> Decision {
    match required {
        RequiredPermission::ReadOnly => Decision::Allow,
        RequiredPermission::Mutating => match mode {
            PermissionMode::Allow => Decision::Allow,
            PermissionMode::Ask => Decision::Ask,
            PermissionMode::Deny => Decision::Deny,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_always_allowed() {
        assert_eq!(
            decide(PermissionMode::Allow, RequiredPermission::ReadOnly),
            Decision::Allow
        );
        assert_eq!(
            decide(PermissionMode::Ask, RequiredPermission::ReadOnly),
            Decision::Allow
        );
        assert_eq!(
            decide(PermissionMode::Deny, RequiredPermission::ReadOnly),
            Decision::Allow
        );
    }

    #[test]
    fn mutating_follows_mode() {
        assert_eq!(
            decide(PermissionMode::Allow, RequiredPermission::Mutating),
            Decision::Allow
        );
        assert_eq!(
            decide(PermissionMode::Ask, RequiredPermission::Mutating),
            Decision::Ask
        );
        assert_eq!(
            decide(PermissionMode::Deny, RequiredPermission::Mutating),
            Decision::Deny
        );
    }
}
