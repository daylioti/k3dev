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

#[allow(dead_code)]
impl PaletteCommandId {
    /// Convert a string ID to a PaletteCommandId
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cluster:start" => Some(Self::ClusterStart),
            "cluster:stop" => Some(Self::ClusterStop),
            "cluster:restart" => Some(Self::ClusterRestart),
            "cluster:destroy" => Some(Self::ClusterDestroy),
            "cluster:info" => Some(Self::ClusterInfo),
            "app:refresh" => Some(Self::AppRefresh),
            "app:update-hosts" => Some(Self::AppUpdateHosts),
            "app:help" => Some(Self::AppHelp),
            "app:quit" => Some(Self::AppQuit),
            "nav:focus-menu" => Some(Self::NavFocusMenu),
            "nav:focus-actions" => Some(Self::NavFocusActions),
            _ => None,
        }
    }

    /// Convert to a string representation
    pub fn as_str(&self) -> &str {
        match self {
            Self::ClusterStart => "cluster:start",
            Self::ClusterStop => "cluster:stop",
            Self::ClusterRestart => "cluster:restart",
            Self::ClusterDestroy => "cluster:destroy",
            Self::ClusterInfo => "cluster:info",
            Self::AppRefresh => "app:refresh",
            Self::AppUpdateHosts => "app:update-hosts",
            Self::AppHelp => "app:help",
            Self::AppQuit => "app:quit",
            Self::NavFocusMenu => "nav:focus-menu",
            Self::NavFocusActions => "nav:focus-actions",
            Self::Custom(path) => path.as_str(),
        }
    }

    /// Check if this is a custom command
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
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
            _ => None,
        }
    }

    /// Check if this is a cluster command
    pub fn is_cluster_command(&self) -> bool {
        matches!(
            self,
            Self::ClusterStart
                | Self::ClusterStop
                | Self::ClusterRestart
                | Self::ClusterDestroy
                | Self::ClusterInfo
        )
    }

    /// Check if this is an app command
    pub fn is_app_command(&self) -> bool {
        matches!(
            self,
            Self::AppRefresh | Self::AppUpdateHosts | Self::AppHelp | Self::AppQuit
        )
    }

    /// Check if this is a navigation command
    pub fn is_nav_command(&self) -> bool {
        matches!(self, Self::NavFocusMenu | Self::NavFocusActions)
    }

    /// Get all built-in command IDs (excludes Custom commands)
    pub fn all() -> &'static [PaletteCommandId] {
        &[
            Self::ClusterStart,
            Self::ClusterStop,
            Self::ClusterRestart,
            Self::ClusterDestroy,
            Self::ClusterInfo,
            Self::AppRefresh,
            Self::AppUpdateHosts,
            Self::AppHelp,
            Self::AppQuit,
            Self::NavFocusMenu,
            Self::NavFocusActions,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        assert_eq!(
            PaletteCommandId::parse("cluster:start"),
            Some(PaletteCommandId::ClusterStart)
        );
        assert_eq!(PaletteCommandId::parse("unknown"), None);
    }

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
