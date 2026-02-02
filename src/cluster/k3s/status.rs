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

impl ClusterStatus {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterStatus::Running => "Running",
            ClusterStatus::Stopped => "Stopped",
            ClusterStatus::Starting => "Starting",
            ClusterStatus::Paused => "Paused",
            ClusterStatus::NotCreated => "Not Created",
            ClusterStatus::RuntimeNotRunning => "Runtime Not Running",
            ClusterStatus::Unknown => "Unknown",
        }
    }
}
