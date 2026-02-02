//! Message types and handling for async communication
//!
//! This module defines the AppMessage enum and the handle_message implementation.

use crate::cluster::{
    ClusterStatus, ContainerStats, IngressEntry, IngressHealthStatus, ResourceStats,
};
use crate::config::RefreshTask;
use crate::ui::components::{ActivePortForward, OutputLine, PodStat};
use std::collections::{HashMap, HashSet};

use super::App;

/// Async message types for communication between tasks and the app
pub enum AppMessage {
    /// Output line from a command
    OutputLine(OutputLine),

    /// Command completed with exit code
    CommandComplete(i32),

    /// Cluster status update
    ClusterStatusUpdate(ClusterStatus),

    /// Ingress entries loaded
    IngressEntriesLoaded(Vec<IngressEntry>),

    /// Ingress health status updated
    IngressHealthUpdated(HashMap<String, IngressHealthStatus>),

    /// Missing hosts from /etc/hosts
    MissingHostsUpdated(HashSet<String>),

    /// Resource stats updated
    ResourceStatsUpdated(Option<ResourceStats>),

    /// Pod stats updated (per-container stats)
    PodStatsUpdated(Vec<ContainerStats>),

    /// Active port forwards detected
    ActivePortForwardsUpdated(Vec<ActivePortForward>),

    /// Error message
    Error(String),
}

impl App {
    pub(super) fn handle_message(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::OutputLine(line) => {
                // Log the output line based on its type
                use crate::ui::components::OutputType;
                match line.output_type {
                    OutputType::Info => tracing::info!("{}", line.content),
                    OutputType::Warning => tracing::warn!("{}", line.content),
                    OutputType::Error => tracing::error!("{}", line.content),
                    OutputType::Success => tracing::info!(event = "success", "{}", line.content),
                }

                // Forward to both output (internal buffer) and output_popup
                self.output.add_line(line.clone());
                self.output_popup.add_line(line);
            }
            AppMessage::CommandComplete(exit_code) => {
                tracing::info!(exit_code = %exit_code, "Command completed");

                self.is_executing = false;
                self.status_bar.set_executing(false);
                self.cancel_token = None;

                if exit_code == 0 {
                    self.output.add_success("Command completed successfully");
                    self.output_popup
                        .add_line(crate::ui::components::OutputLine::success(
                            "Command completed successfully",
                        ));
                } else {
                    self.output
                        .add_error(format!("Command exited with code {}", exit_code));
                    self.output_popup
                        .add_line(crate::ui::components::OutputLine::error(format!(
                            "Command exited with code {}",
                            exit_code
                        )));
                }

                // Scroll to bottom to show completion message
                self.output_popup.scroll_to_bottom();

                // Refresh status check
                self.spawn_status_check();
            }
            AppMessage::ClusterStatusUpdate(status) => {
                let was_running = matches!(self.cluster_status, ClusterStatus::Running);
                let is_running = matches!(status, ClusterStatus::Running);

                // Log cluster status changes
                if self.cluster_status != status {
                    tracing::info!(
                        old_status = ?self.cluster_status,
                        new_status = ?status,
                        "Cluster status changed"
                    );
                }

                self.cluster_status = status;

                // If cluster just became running, trigger refresh and show ports
                if is_running && !was_running {
                    // Set forwarded ports from config
                    let mut all_ports = vec![
                        (self.cluster_config.http_port, self.cluster_config.http_port),
                        (
                            self.cluster_config.https_port,
                            self.cluster_config.https_port,
                        ),
                        (self.cluster_config.api_port, self.cluster_config.api_port),
                    ];
                    all_ports.extend(self.cluster_config.additional_ports.clone());
                    self.menu.set_forwarded_ports(all_ports);

                    self.spawn_ingress_refresh();
                    self.spawn_missing_hosts_check();
                    self.spawn_port_forwards_check();
                    // Reset scheduler timers for tasks we just triggered
                    self.scheduler
                        .mark_run_multiple(&[RefreshTask::IngressRefresh, RefreshTask::HostsCheck]);
                }

                // If cluster stopped running, clear cluster-specific data
                if !is_running && was_running {
                    self.menu.set_forwarded_ports(Vec::new());
                    self.menu.set_active_port_forwards(Vec::new());
                    self.menu.set_ingress_entries(Vec::new());
                    self.menu
                        .set_ingress_health(std::collections::HashMap::new());
                    self.menu
                        .set_missing_hosts(std::collections::HashSet::new());
                    self.status_bar.set_resource_stats(None);
                    self.pod_stats.set_pods(Vec::new());
                }
            }
            AppMessage::IngressEntriesLoaded(entries) => {
                self.menu.set_ingress_entries(entries);
                self.spawn_ingress_health_check();
                self.spawn_missing_hosts_check();
            }
            AppMessage::IngressHealthUpdated(health) => {
                self.menu.set_ingress_health(health);
            }
            AppMessage::MissingHostsUpdated(missing) => {
                self.menu.set_missing_hosts(missing);
            }
            AppMessage::ResourceStatsUpdated(stats) => {
                self.status_bar.set_resource_stats(stats);
            }
            AppMessage::PodStatsUpdated(stats) => {
                // Convert ContainerStats to PodStat
                let mut pod_stats: Vec<PodStat> = stats
                    .into_iter()
                    .map(|s| PodStat {
                        name: s.name,
                        namespace: s.namespace,
                        cpu_percent: s.cpu_percent,
                        cpu_limit_millicores: s.cpu_limit_millicores,
                        memory_used_mb: s.memory_used_mb,
                        memory_limit_mb: s.memory_limit_mb,
                        status: s.status,
                    })
                    .collect();
                // Sort by namespace first, then by pod name within each namespace
                pod_stats.sort_by(|a, b| {
                    a.namespace
                        .cmp(&b.namespace)
                        .then_with(|| a.name.cmp(&b.name))
                });
                self.pod_stats.set_pods(pod_stats);
            }
            AppMessage::ActivePortForwardsUpdated(forwards) => {
                self.menu.set_active_port_forwards(forwards);
            }
            AppMessage::Error(msg) => {
                tracing::error!("{}", msg);
                self.output.add_error(&msg);
            }
        }
    }
}
