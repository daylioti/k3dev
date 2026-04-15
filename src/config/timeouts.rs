//! Centralized timeout and refresh configuration
//!
//! This module defines all timing-related constants used throughout the application,
//! including the unified RefreshScheduler for managing periodic tasks.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Configuration for refresh intervals and operation timeouts
#[derive(Debug, Clone)]
pub struct RefreshConfig {
    /// Interval for refreshing ingress entries and health status
    pub ingress_refresh: Duration,

    /// Interval for checking /etc/hosts for missing entries
    pub hosts_check: Duration,

    /// Interval for toggling blink state (e.g., (H) indicator)
    pub blink_toggle: Duration,

    /// Interval for refreshing resource stats (CPU, memory)
    pub stats_refresh: Duration,

    /// Timeout for cluster status check operations
    pub status_check_timeout: Duration,

    /// Timeout for cluster operations (start, stop, restart, destroy)
    pub cluster_operation_timeout: Duration,

    /// Timeout for ingress refresh operations
    pub ingress_timeout: Duration,

    /// Timeout for ingress health check operations
    pub ingress_health_timeout: Duration,

    /// Timeout for docker stats operations
    pub docker_stats_timeout: Duration,

    /// Timeout for port forward detection
    pub port_forward_timeout: Duration,

    /// Timeout for manual hosts update operations
    pub manual_hosts_timeout: Duration,

    /// Interval for refreshing volume/PVC stats
    pub volume_refresh: Duration,

    /// Timeout for volume stats operations
    pub volume_timeout: Duration,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            // Refresh intervals
            ingress_refresh: Duration::from_secs(15),
            hosts_check: Duration::from_secs(5),
            blink_toggle: Duration::from_millis(500),
            stats_refresh: Duration::from_secs(2),

            // Operation timeouts
            status_check_timeout: Duration::from_secs(5),
            cluster_operation_timeout: Duration::from_secs(300), // 5 minutes
            ingress_timeout: Duration::from_secs(10),
            ingress_health_timeout: Duration::from_secs(30),
            docker_stats_timeout: Duration::from_secs(5),
            port_forward_timeout: Duration::from_secs(10),
            manual_hosts_timeout: Duration::from_secs(60),
            volume_refresh: Duration::from_secs(10),
            volume_timeout: Duration::from_secs(10),
        }
    }
}

/// Types of refresh tasks managed by the scheduler
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefreshTask {
    /// Refresh ingress entries and health status
    IngressRefresh,
    /// Check /etc/hosts for missing entries
    HostsCheck,
    /// Toggle blink state for UI indicators
    BlinkToggle,
    /// Refresh resource and pod stats
    StatsRefresh,
    /// Refresh volume/PVC stats
    VolumeRefresh,
}

/// Internal state for a scheduled task
struct TaskState {
    interval: Duration,
    last_run: Instant,
}

/// Unified scheduler for managing periodic refresh tasks
///
/// Replaces multiple individual timers with a single scheduler that
/// efficiently tracks and triggers multiple periodic tasks.
pub struct RefreshScheduler {
    tasks: HashMap<RefreshTask, TaskState>,
}

impl RefreshScheduler {
    /// Create a new scheduler with intervals from config
    pub fn new(config: &RefreshConfig) -> Self {
        let now = Instant::now();
        let mut tasks = HashMap::new();

        tasks.insert(
            RefreshTask::IngressRefresh,
            TaskState {
                interval: config.ingress_refresh,
                last_run: now,
            },
        );

        tasks.insert(
            RefreshTask::HostsCheck,
            TaskState {
                interval: config.hosts_check,
                last_run: now,
            },
        );

        tasks.insert(
            RefreshTask::BlinkToggle,
            TaskState {
                interval: config.blink_toggle,
                last_run: now,
            },
        );

        tasks.insert(
            RefreshTask::StatsRefresh,
            TaskState {
                interval: config.stats_refresh,
                last_run: now,
            },
        );

        tasks.insert(
            RefreshTask::VolumeRefresh,
            TaskState {
                interval: config.volume_refresh,
                last_run: now,
            },
        );

        Self { tasks }
    }

    /// Check which tasks are due to run and return them
    ///
    /// This method checks all registered tasks and returns a list of
    /// tasks whose interval has elapsed. It automatically updates the
    /// last_run time for returned tasks.
    pub fn tick(&mut self) -> Vec<RefreshTask> {
        let now = Instant::now();
        let mut due_tasks = Vec::new();

        for (task, state) in self.tasks.iter_mut() {
            if now.duration_since(state.last_run) >= state.interval {
                due_tasks.push(*task);
                state.last_run = now;
            }
        }

        due_tasks
    }

    /// Mark multiple tasks as having just run
    pub fn mark_run_multiple(&mut self, tasks: &[RefreshTask]) {
        let now = Instant::now();
        for task in tasks {
            if let Some(state) = self.tasks.get_mut(task) {
                state.last_run = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_scheduler_tick() {
        let config = RefreshConfig {
            blink_toggle: Duration::from_millis(10),
            ..RefreshConfig::default()
        };
        let mut scheduler = RefreshScheduler::new(&config);

        // Initially no tasks should be due
        let due = scheduler.tick();
        assert!(due.is_empty());

        // Wait for blink toggle interval
        sleep(Duration::from_millis(15));

        // Now blink toggle should be due
        let due = scheduler.tick();
        assert!(due.contains(&RefreshTask::BlinkToggle));

        // Immediately after, nothing should be due
        let due = scheduler.tick();
        assert!(due.is_empty());
    }
}
