//! Command type definitions
//!
//! This module defines typed enums for command identifiers, replacing magic strings
//! with type-safe variants.

use crate::ui::components::ClusterAction;

/// Command palette command identifiers
///
/// These represent all commands that can be executed from the command palette.
/// Using an enum instead of strings provides compile-time safety and better refactoring support.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PaletteCommandId {
    // Cluster commands
    ClusterStart,
    ClusterStop,
    ClusterRestart,
    ClusterDestroy,
    ClusterInfo,
    ClusterDeleteSnapshots,
    ClusterDiagnostics,
    ClusterPreflightCheck,

    // Application commands
    AppRefresh,
    AppUpdateHosts,
    AppHelp,
    AppQuit,

    // Navigation commands
    NavFocusMenu,
    NavFocusActions,

    // Custom commands from config (path like "Group Name/Command Name")
    Custom(String),
}

impl PaletteCommandId {
    /// Convert to a string representation
    pub fn as_str(&self) -> &str {
        match self {
            Self::ClusterStart => "cluster:start",
            Self::ClusterStop => "cluster:stop",
            Self::ClusterRestart => "cluster:restart",
            Self::ClusterDestroy => "cluster:destroy",
            Self::ClusterInfo => "cluster:info",
            Self::ClusterDeleteSnapshots => "cluster:delete-snapshots",
            Self::ClusterDiagnostics => "cluster:diagnostics",
            Self::ClusterPreflightCheck => "cluster:preflight-check",
            Self::AppRefresh => "app:refresh",
            Self::AppUpdateHosts => "app:update-hosts",
            Self::AppHelp => "app:help",
            Self::AppQuit => "app:quit",
            Self::NavFocusMenu => "nav:focus-menu",
            Self::NavFocusActions => "nav:focus-actions",
            Self::Custom(path) => path.as_str(),
        }
    }

    /// Get the custom command path if this is a custom command
    pub fn custom_path(&self) -> Option<&str> {
        match self {
            Self::Custom(path) => Some(path),
            _ => None,
        }
    }

    /// Try to convert to a ClusterAction if this is a cluster command
    pub fn as_cluster_action(&self) -> Option<ClusterAction> {
        match self {
            Self::ClusterStart => Some(ClusterAction::Start),
            Self::ClusterStop => Some(ClusterAction::Stop),
            Self::ClusterRestart => Some(ClusterAction::Restart),
            Self::ClusterDestroy => Some(ClusterAction::Destroy),
            Self::ClusterInfo => Some(ClusterAction::Info),
            Self::ClusterDeleteSnapshots => Some(ClusterAction::DeleteSnapshots),
            Self::ClusterDiagnostics => Some(ClusterAction::Diagnostics),
            Self::ClusterPreflightCheck => Some(ClusterAction::PreflightCheck),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_as_str() {
        assert_eq!(PaletteCommandId::ClusterStart.as_str(), "cluster:start");
    }

    #[test]
    fn test_as_cluster_action() {
        assert!(PaletteCommandId::ClusterStart.as_cluster_action().is_some());
        assert!(PaletteCommandId::AppQuit.as_cluster_action().is_none());
    }
}
