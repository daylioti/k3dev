//! Message types and handling for async communication
//!
//! This module defines the AppMessage enum and the handle_message implementation.

use crate::cluster::diagnostics::DiagnosticsReport;
use crate::cluster::{
    ClusterStatus, ContainerPullProgress, ContainerStats, IngressEntry, IngressHealthStatus,
    ResourceStats,
};
use crate::config::RefreshTask;
use crate::k8s::{PendingPodInfo, PodTimeline, PvcInfo, ShellSessionHandle};
use crate::ui::components::{
    ActivePortForward, ContainerPullInfo, DetailTab, OutputLine, PodStat, PodState,
};
use std::collections::{HashMap, HashSet};

use super::{App, AppMode};

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

    /// Pending pods (waiting for image pulls, etc.)
    PendingPodsUpdated(Vec<PendingPodInfo>),

    /// Image pull progress updated (image -> progress)
    PullProgressUpdated(HashMap<String, ContainerPullProgress>),

    /// A pull monitor stream has finished (image pull complete or errored)
    ImagePullMonitorDone(String),

    /// Diagnostics state update (sent incrementally as tests complete)
    DiagnosticsUpdated(DiagnosticsReport),

    /// Pod startup timeline loaded
    PodTimelineLoaded(PodTimeline),

    /// Pod logs loaded for detail panel
    PodLogsLoaded(Vec<String>),

    /// Pod describe loaded for detail panel
    PodDescribeLoaded(Vec<String>),

    /// Raw output bytes from interactive shell
    ShellOutput(Vec<u8>),

    /// Shell session handle delivered to App
    ShellSessionReady(ShellSessionHandle),

    /// Shell session closed (optional error message)
    ShellSessionEnded(Option<String>),

    /// Pod resolved for a shell command — open shell tab and send command
    ShellCommandPodResolved {
        pod_name: String,
        namespace: String,
        container: String,
        workdir: String,
        command: String,
    },

    /// Volume/PVC stats updated
    VolumeStatsUpdated(Vec<PvcInfo>),

    /// K8s client initialized (lazy, triggered when cluster becomes running)
    K8sClientReady(Option<crate::k8s::K8sClient>),

    /// Image architecture data for pods (pod_key → architecture string)
    ImageArchUpdated(HashMap<String, String>),

    /// Hosts update requires sudo — contains the file content to write
    NeedsSudoHostsWrite { content: String, count: usize },

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
                self.status_bar.update_connection_state(is_running);

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
                    self.spawn_volume_stats_check();
                    // Lazily init K8s client now that cluster is running
                    if self.k8s_client.is_none() {
                        let tx = self.message_tx.clone();
                        let kubeconfig = self.cluster_config.kubeconfig.clone();
                        let context = self.cluster_config.context.clone();
                        tokio::spawn(async move {
                            let kc = kubeconfig.as_deref();
                            let ctx = context.as_deref();
                            let client = crate::k8s::K8sClient::new(kc, ctx).await.ok();
                            let _ = tx.send(AppMessage::K8sClientReady(client)).await;
                        });
                    }
                    // Reset scheduler timers for tasks we just triggered
                    self.scheduler.mark_run_multiple(&[
                        RefreshTask::IngressRefresh,
                        RefreshTask::HostsCheck,
                        RefreshTask::VolumeRefresh,
                    ]);
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
                    self.menu.set_volume_entries(Vec::new());
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
                // Cache the running pods and merge with pending
                self.running_pods_cache = stats;
                self.merge_and_update_pod_stats();
            }
            AppMessage::PendingPodsUpdated(pending) => {
                // Cache the pending pods and merge with running
                self.pending_pods_cache = pending;

                // Clean up stale pull progress for images no longer pulling
                let still_pulling: HashSet<String> = self
                    .pending_pods_cache
                    .iter()
                    .flat_map(|p| {
                        p.containers
                            .iter()
                            .filter(|c| {
                                c.reason == "ContainerCreating" || c.reason == "PodInitializing"
                            })
                            .map(|c| c.image.clone())
                    })
                    .collect();
                self.pull_progress_cache
                    .retain(|img, _| still_pulling.contains(img));

                self.merge_and_update_pod_stats();
            }
            AppMessage::PullProgressUpdated(progress) => {
                // Merge monotonically: never let downloaded/total bytes decrease
                for (image, new_progress) in progress {
                    let entry = self.pull_progress_cache.entry(image).or_insert_with(|| {
                        ContainerPullProgress::new(
                            &new_progress.container_name,
                            &new_progress.image,
                        )
                    });
                    if new_progress.downloaded_bytes >= entry.downloaded_bytes {
                        entry.downloaded_bytes = new_progress.downloaded_bytes;
                    }
                    if new_progress.total_bytes >= entry.total_bytes {
                        entry.total_bytes = new_progress.total_bytes;
                    }
                    // Recompute percent from monotonic values
                    if entry.total_bytes > 0 {
                        entry.progress_percent =
                            (entry.downloaded_bytes as f64 / entry.total_bytes as f64 * 100.0)
                                .min(100.0);
                        entry.tracking_available = true;
                    }
                    // Pass through phase and layer counts
                    entry.phase = new_progress.phase;
                    entry.layers_done = new_progress.layers_done;
                    entry.layers_total = new_progress.layers_total;
                }
                self.merge_and_update_pod_stats();
            }
            AppMessage::ImagePullMonitorDone(image) => {
                // Pull finished or errored — remove from caches
                self.pull_progress_cache.remove(&image);
                self.active_pull_monitors.remove(&image);
                self.merge_and_update_pod_stats();
            }
            AppMessage::DiagnosticsUpdated(report) => {
                self.diagnostics_overlay.update(report);
            }
            AppMessage::PodTimelineLoaded(timeline) => {
                // Only apply if the detail panel is open for this pod
                if self.pod_detail_panel.is_open()
                    && self.pod_detail_panel.pod_name() == timeline.pod_name
                {
                    self.pod_detail_panel.set_timeline(timeline);
                }
            }
            AppMessage::PodLogsLoaded(lines) => {
                if self.pod_detail_panel.is_open() {
                    self.pod_detail_panel.set_logs(lines);
                }
            }
            AppMessage::PodDescribeLoaded(lines) => {
                if self.pod_detail_panel.is_open() {
                    self.pod_detail_panel.set_describe(lines);
                }
            }
            AppMessage::ActivePortForwardsUpdated(forwards) => {
                self.menu.set_active_port_forwards(forwards);
            }
            AppMessage::VolumeStatsUpdated(entries) => {
                self.menu.set_volume_entries(entries);
            }
            AppMessage::ShellOutput(bytes) => {
                if self.pod_detail_panel.is_open() && self.pod_detail_panel.has_shell_view() {
                    self.pod_detail_panel.feed_shell_output(&bytes);
                }
            }
            AppMessage::ShellSessionReady(handle) => {
                // Only accept if shell tab is still active for this pod
                if self.pod_detail_panel.is_open()
                    && self.pod_detail_panel.active_tab() == DetailTab::Shell
                    && self.pod_detail_panel.pod_name() == handle.pod_name()
                {
                    self.pod_detail_panel.set_shell_connected();
                    self.shell_session = Some(handle);

                    // If a command is pending, send it now and enter shell mode
                    if let Some(cmd) = self.pending_shell_command.take() {
                        if let Some(session) = &self.shell_session {
                            session.write(cmd.as_bytes());
                        }
                        self.mode = AppMode::Shell;
                        self.pod_detail_panel.set_shell_interactive(true);
                    }
                } else {
                    // Shell tab was closed/changed while connecting
                    handle.close();
                }
            }
            AppMessage::ShellSessionEnded(error) => {
                self.shell_session = None;
                self.pending_shell_command = None;
                self.pod_detail_panel.set_shell_interactive(false);
                if let Some(err) = error {
                    self.pod_detail_panel.set_shell_error(err);
                } else {
                    self.pod_detail_panel.set_shell_disconnected();
                }
                if self.mode == AppMode::Shell {
                    self.mode = AppMode::Normal;
                }
            }
            AppMessage::ShellCommandPodResolved {
                pod_name,
                namespace,
                container,
                workdir,
                command,
            } => {
                // Build the shell command string
                let shell_cmd = if workdir.is_empty() {
                    format!("{}\n", command)
                } else {
                    format!("cd {} && {}\n", workdir, command)
                };

                // Find and select the pod in the pod list
                let pod_index = self
                    .pod_stats
                    .pods()
                    .iter()
                    .position(|p| p.name == pod_name);

                if let Some(idx) = pod_index {
                    self.pod_stats.select_index(idx);
                }

                // Focus on PodStats and open Shell tab
                self.focus = super::FocusArea::PodStats;
                self.pod_detail_panel
                    .open(pod_name.clone(), namespace.clone(), DetailTab::Shell);

                // Check if we already have a connected shell session for this pod
                let same_pod_session = self
                    .shell_session
                    .as_ref()
                    .map(|s| s.pod_name() == pod_name && s.namespace() == namespace)
                    .unwrap_or(false);

                if same_pod_session {
                    // Session already connected — send command immediately
                    if let Some(session) = &self.shell_session {
                        // Ensure shell_view exists
                        if !self.pod_detail_panel.has_shell_view() {
                            let (rows, cols) = self.calculate_shell_dimensions();
                            self.pod_detail_panel.init_shell_view(rows, cols);
                            self.pod_detail_panel.set_shell_connected();
                        }
                        session.write(shell_cmd.as_bytes());
                    }
                    self.mode = AppMode::Shell;
                    self.pod_detail_panel.set_shell_interactive(true);
                } else {
                    // Close old session if any
                    if let Some(old_session) = self.shell_session.take() {
                        old_session.close();
                    }
                    // Store pending command and start new shell session
                    self.pending_shell_command = Some(shell_cmd);
                    let container_opt = if container.is_empty() {
                        None
                    } else {
                        Some(container.as_str())
                    };
                    self.spawn_shell_session(&pod_name, &namespace, container_opt);
                }
            }
            AppMessage::ImageArchUpdated(arch_data) => {
                self.image_arch_cache.extend(arch_data);
                self.image_arch_check_pending = false;
                self.merge_and_update_pod_stats();
            }
            AppMessage::NeedsSudoHostsWrite { content, count } => {
                // Store for the main event loop to handle (needs terminal access)
                self.pending_sudo_hosts_content = Some((content, count));
            }
            AppMessage::K8sClientReady(client) => {
                self.k8s_client = client;
            }
            AppMessage::Error(msg) => {
                tracing::error!("{}", msg);
                self.output.add_error(&msg);
            }
        }
    }

    /// The host's expected image architecture
    fn host_image_arch() -> &'static str {
        if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "amd64"
        }
    }

    /// Check if a pod's image has a mismatched architecture
    fn is_arch_mismatch(&self, namespace: &str, pod_name: &str) -> bool {
        let key = format!("{}/{}", namespace, pod_name);
        self.image_arch_cache
            .get(&key)
            .map(|arch| arch != Self::host_image_arch())
            .unwrap_or(false)
    }

    /// Merge running pods (from cgroups) with pending pods (from K8s API)
    /// and update the pod_stats component
    fn merge_and_update_pod_stats(&mut self) {
        let mut pod_stats: Vec<PodStat> = Vec::new();

        // Build a set of running pod names for quick lookup
        let running_pod_names: HashSet<String> = self
            .running_pods_cache
            .iter()
            .map(|s| s.name.clone())
            .collect();

        // Add running pods with Running state
        for s in &self.running_pods_cache {
            pod_stats.push(PodStat {
                name: s.name.clone(),
                namespace: s.namespace.clone(),
                state: PodState::Running,
                cpu_percent: s.cpu_percent,
                cpu_limit_millicores: s.cpu_limit_millicores,
                memory_used_mb: s.memory_used_mb,
                memory_limit_mb: s.memory_limit_mb,
                status: s.status.clone(),
                arch_mismatch: self.is_arch_mismatch(&s.namespace, &s.name),
            });
        }

        // Add pending pods (only if not already in running list)
        for pending in &self.pending_pods_cache {
            // Skip if this pod is already running (cgroups data is more accurate)
            if running_pod_names.contains(&pending.name) {
                continue;
            }

            // Check if any container is in a pulling state
            let pulling_containers: Vec<_> = pending
                .containers
                .iter()
                .filter(|c| c.reason == "ContainerCreating" || c.reason == "PodInitializing")
                .collect();

            let failed_container = pending.containers.iter().find(|c| {
                matches!(
                    c.reason.as_str(),
                    "ImagePullBackOff"
                        | "ErrImagePull"
                        | "InvalidImageName"
                        | "CrashLoopBackOff"
                        | "Error"
                        | "Failed"
                )
            });

            // Determine state based on container waiting reasons
            let state = if let Some(failed) = failed_container {
                // If any container failed, show failure state
                // Use short image name to avoid truncation in the UI
                let short_image = failed.image.rsplit('/').next().unwrap_or(&failed.image);
                PodState::Failed {
                    reason: format!("{}: {}", failed.reason, short_image),
                }
            } else if !pulling_containers.is_empty() {
                // Build container pull info for all pulling containers
                let containers: Vec<ContainerPullInfo> = pulling_containers
                    .iter()
                    .map(|c| {
                        // Look up progress from cache
                        let progress = self.pull_progress_cache.get(&c.image);
                        let mut info = ContainerPullInfo::new(&c.name, &c.image);
                        if let Some(p) = progress {
                            if p.tracking_available {
                                info = info.with_progress(p.downloaded_bytes, p.total_bytes);
                            }
                            info.layers_done = p.layers_done;
                            info.layers_total = p.layers_total;
                            info.phase = p.phase.clone();
                        }
                        info
                    })
                    .collect();

                PodState::Pulling {
                    containers,
                    started_at: pending.started_at,
                }
            } else if let Some(container) = pending.containers.first() {
                PodState::Waiting {
                    reason: container.reason.clone(),
                }
            } else {
                PodState::Waiting {
                    reason: "Pending".to_string(),
                }
            };

            pod_stats.push(PodStat {
                name: pending.name.clone(),
                namespace: pending.namespace.clone(),
                state,
                cpu_percent: 0.0,
                cpu_limit_millicores: 0.0,
                memory_used_mb: 0.0,
                memory_limit_mb: 0.0,
                status: "Pending".to_string(),
                arch_mismatch: false, // Pending pods don't have image arch info yet
            });
        }

        // Sort by namespace first, then by pod name within each namespace
        pod_stats.sort_by(|a, b| {
            a.namespace
                .cmp(&b.namespace)
                .then_with(|| a.name.cmp(&b.name))
        });

        // Check if any running pods need architecture info (avoid duplicate checks)
        if !self.image_arch_check_pending {
            let has_uncached = self.running_pods_cache.iter().any(|s| {
                let key = format!("{}/{}", s.namespace, s.name);
                !self.image_arch_cache.contains_key(&key)
            });
            if has_uncached {
                self.image_arch_check_pending = true;
                self.spawn_image_arch_check();
            }
        }

        self.pod_stats.set_pods(pod_stats);

        // Auto-open detail panel when a pod is selected
        self.ensure_detail_panel_synced();
    }
}
