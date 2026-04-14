//! Cluster status types

/// Cluster status
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClusterStatus {
    Running,
    Stopped,
    Starting,
    Paused,
    NotCreated,
    RuntimeNotRunning,
    Unknown,
}
