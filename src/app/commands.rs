//! Command execution logic
//!
//! This module contains command execution functions including cluster actions,
//! pod commands, and palette command handling.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::cluster::diagnostics::{run_all_diagnostics, run_preflight_checks};
use crate::cluster::{ClusterManager, HostsUpdateResult, IngressManager};
use crate::commands::{CommandContext, PaletteCommandId};
use crate::cluster::DockerManager;
use crate::config::{get_exec_placeholders, CommandEntry, ExecutionTarget, RefreshTask};
use crate::k8s::PodExecutor;
use crate::ui::components::{ClusterAction, DetailTab};

use super::{App, AppMessage, AppMode, FocusArea};

impl App {
    /// Open a URL in the default browser
    pub(super) fn open_url(&mut self, url: &str) {
        match open::that(url) {
            Ok(_) => {
                self.output.add_info(format!("Opening: {}", url));
            }
            Err(e) => {
                self.output.add_error(format!("Failed to open URL: {}", e));
            }
        }
    }

    pub(super) fn execute_palette_command(&mut self, cmd_id: PaletteCommandId) {
        // Handle custom commands from config
        if let Some(path) = cmd_id.custom_path() {
            self.execute_custom_command(path);
            return;
        }

        // Handle cluster commands via ClusterAction conversion
        if let Some(action) = cmd_id.as_cluster_action() {
            self.execute_cluster_action(action);
            return;
        }

        // Handle other commands
        match cmd_id {
            PaletteCommandId::AppRefresh => {
                self.spawn_status_check();
                self.spawn_ingress_refresh();
                self.spawn_missing_hosts_check();
                self.spawn_port_forwards_check();
                // Reset scheduler timers for tasks we just triggered
                self.scheduler
                    .mark_run_multiple(&[RefreshTask::IngressRefresh, RefreshTask::HostsCheck]);
            }
            PaletteCommandId::AppUpdateHosts => self.trigger_manual_hosts_update(),
            PaletteCommandId::AppHelp => self.mode = AppMode::Help,
            PaletteCommandId::AppQuit => self.should_quit = true,
            PaletteCommandId::NavFocusMenu => self.focus = FocusArea::Content,
            PaletteCommandId::NavFocusActions => self.focus = FocusArea::ActionBar,
            _ => {}
        }
    }

    /// Execute a custom command by path (e.g., "Group Name/Command Name")
    pub(super) fn execute_custom_command(&mut self, path: &str) {
        if let Some(cmd) = self.find_command_by_path(path) {
            self.execute_command(cmd);
        } else {
            self.output
                .add_error(format!("Command not found: {}", path));
        }
    }

    /// Find a command by its path (e.g., "Group Name/Command Name" or "Group/Subgroup/Command")
    fn find_command_by_path(&self, path: &str) -> Option<CommandEntry> {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.is_empty() {
            return None;
        }

        // Find the group
        let group_name = parts[0].trim();
        let group = self
            .config
            .commands
            .iter()
            .find(|g| g.name.eq_ignore_ascii_case(group_name))?;

        // Navigate through the remaining path parts
        let mut commands = &group.commands;
        for (i, part) in parts[1..].iter().enumerate() {
            let part = part.trim();
            let is_last = i == parts.len() - 2;

            let found = commands
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(part))?;

            if is_last {
                return Some(found.clone());
            } else {
                commands = &found.commands;
            }
        }

        None
    }

    pub(super) fn execute_cluster_action(&mut self, action: ClusterAction) {
        // Diagnostics and preflight use the diagnostics overlay, not the output popup
        if action == ClusterAction::Diagnostics {
            self.run_diagnostics();
            return;
        }
        if action == ClusterAction::PreflightCheck {
            self.run_preflight_check();
            return;
        }

        // Show confirmation for destroy action
        if action == ClusterAction::Destroy {
            self.pending_cluster_action = Some(action);
            self.confirm_popup.set_content(
                "Destroy Cluster",
                "This will permanently destroy the cluster. This cannot be undone.",
            );
            self.mode = AppMode::ConfirmDestroy;
            return;
        }

        // Show confirmation for delete snapshots action
        if action == ClusterAction::DeleteSnapshots {
            self.pending_cluster_action = Some(action);
            self.confirm_popup.set_content(
                "Delete Snapshots",
                "This will remove all snapshot images. Next cluster start will be slower.",
            );
            self.mode = AppMode::ConfirmDestroy;
            return;
        }

        self.do_execute_cluster_action(action);
    }

    /// Handle confirmation for destroy action
    pub(super) fn confirm_destroy(&mut self) {
        if let Some(action) = self.pending_cluster_action.take() {
            self.mode = AppMode::Normal;
            self.do_execute_cluster_action(action);
        }
    }

    /// Cancel destroy confirmation
    pub(super) fn cancel_destroy(&mut self) {
        self.pending_cluster_action = None;
        self.mode = AppMode::Normal;
        self.output.add_info("Destroy cancelled");
    }

    pub(super) fn do_execute_cluster_action(&mut self, action: ClusterAction) {
        self.output.clear();
        self.output
            .set_title(format!("Cluster {}", action.as_str()));
        self.output_popup.clear();
        self.output_popup
            .set_title(format!("Cluster {}", action.as_str()));
        self.is_executing = true;
        self.status_bar.set_executing(true);
        // Show output popup immediately when command starts
        self.mode = super::AppMode::OutputPopup;

        let cancel_token = CancellationToken::new();
        self.cancel_token = Some(cancel_token.clone());

        let timeout_duration = self.refresh_config.cluster_operation_timeout;
        let (ctx, tx) = CommandContext::new(self.message_tx.clone(), timeout_duration);

        let cluster_config = Arc::clone(&self.cluster_config);

        tokio::spawn(async move {
            ctx.execute(move |_output_tx| async move {
                let mut manager = ClusterManager::new(cluster_config)
                    .await
                    .map_err(|e| format!("Manager error: {}", e))?;

                let action_result = match action {
                    ClusterAction::Start => manager.start(tx).await,
                    ClusterAction::Stop => manager.stop(tx).await,
                    ClusterAction::Restart => manager.restart(tx).await,
                    ClusterAction::Destroy => manager.delete(tx).await,
                    ClusterAction::Info => manager.info(tx).await,
                    ClusterAction::DeleteSnapshots => manager.delete_snapshots(tx).await,
                    // Diagnostics and PreflightCheck are handled before reaching here
                    ClusterAction::Diagnostics | ClusterAction::PreflightCheck => {
                        unreachable!()
                    }
                };

                action_result.map_err(|e| format!("Error: {}", e))
            })
            .await;
        });
    }

    pub(super) fn execute_command(&mut self, cmd: crate::config::CommandEntry) {
        let exec = match &cmd.exec {
            Some(e) => e,
            None => return,
        };

        // Check for input placeholders
        let placeholders = get_exec_placeholders(exec);
        if !placeholders.is_empty() {
            let inputs: HashMap<String, String> = placeholders
                .iter()
                .map(|p| {
                    let prompt = exec
                        .input
                        .get(p)
                        .cloned()
                        .unwrap_or_else(|| format!("Enter {}:", p));
                    (p.clone(), prompt)
                })
                .collect();

            self.input_form.setup(&cmd.name, &inputs);
            self.pending_command = Some(cmd);
            self.mode = AppMode::Input;
            return;
        }

        self.dispatch_command(&cmd);
    }

    pub(super) fn submit_input(&mut self) {
        let values = self.input_form.get_values();
        let cmd = match self.pending_command.take() {
            Some(c) => c,
            None => return,
        };

        self.mode = AppMode::Normal;
        self.input_form.clear();

        // Substitute placeholders
        let mut cmd = cmd.clone();
        if let Some(exec) = &mut cmd.exec {
            let subst = |s: &mut String| {
                for (key, value) in &values {
                    let pattern = format!("@{}", key);
                    *s = s.replace(&pattern, value);
                }
            };
            match &mut exec.target {
                ExecutionTarget::Host => {}
                ExecutionTarget::Docker { container } => subst(container),
                ExecutionTarget::Kubernetes {
                    namespace,
                    selector,
                    pod_name,
                    container,
                } => {
                    subst(namespace);
                    subst(selector);
                    subst(pod_name);
                    subst(container);
                }
            }
            subst(&mut exec.workdir);
            subst(&mut exec.cmd);
        }

        self.dispatch_command(&cmd);
    }

    /// Dispatch a command to the right executor based on its target.
    fn dispatch_command(&mut self, cmd: &crate::config::CommandEntry) {
        let exec = match &cmd.exec {
            Some(e) => e,
            None => return,
        };

        match &exec.target {
            ExecutionTarget::Kubernetes { .. } => self.execute_pod_command(cmd),
            ExecutionTarget::Host => self.execute_host_command(cmd),
            ExecutionTarget::Docker { .. } => self.execute_docker_command(cmd),
        }
    }

    fn execute_pod_command(&mut self, cmd: &crate::config::CommandEntry) {
        let exec = match &cmd.exec {
            Some(e) => e,
            None => return,
        };

        let k8s_target = match exec.target.as_kubernetes() {
            Some(t) => t,
            None => return,
        };

        let k8s_client = match &self.k8s_client {
            Some(c) => c,
            None => {
                self.output.add_error("Kubernetes client not connected");
                return;
            }
        };

        let namespace = k8s_target.namespace.to_string();
        let selector = k8s_target.selector.to_string();
        let pod_name = k8s_target.pod_name.to_string();
        let container = k8s_target.container.to_string();
        let workdir = exec.workdir.clone();
        let command = exec.cmd.clone();
        let executor = PodExecutor::new(k8s_client);
        let message_tx = self.message_tx.clone();

        // Resolve target pod async, then send ShellCommandPodResolved
        tokio::spawn(async move {
            let pod = match executor
                .find_pod(
                    &namespace,
                    if selector.is_empty() {
                        None
                    } else {
                        Some(selector.as_str())
                    },
                    if pod_name.is_empty() {
                        None
                    } else {
                        Some(pod_name.as_str())
                    },
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    let _ = message_tx
                        .send(AppMessage::Error(format!("Pod not found: {}", e)))
                        .await;
                    return;
                }
            };

            let _ = message_tx
                .send(AppMessage::ShellCommandPodResolved {
                    pod_name: pod.name,
                    namespace,
                    container,
                    workdir,
                    command,
                })
                .await;
        });
    }

    /// Run a command on the user's host shell, streaming output to the popup.
    fn execute_host_command(&mut self, cmd: &crate::config::CommandEntry) {
        let exec = match &cmd.exec {
            Some(e) => e,
            None => return,
        };
        let command = exec.cmd.clone();
        let workdir = exec.workdir.clone();
        let title = format!("Host: {}", cmd.name);

        self.start_popup_command(title);
        let timeout_duration = self.refresh_config.cluster_operation_timeout;
        let (ctx, output_tx) = CommandContext::new(self.message_tx.clone(), timeout_duration);

        tokio::spawn(async move {
            ctx.execute(move |tx| async move {
                run_host_command(&command, &workdir, tx).await
            })
            .await;
            drop(output_tx);
        });
    }

    /// Run a command in a docker container via `docker exec`, streaming output to the popup.
    fn execute_docker_command(&mut self, cmd: &crate::config::CommandEntry) {
        let exec = match &cmd.exec {
            Some(e) => e,
            None => return,
        };
        let container = match &exec.target {
            ExecutionTarget::Docker { container } => container.clone(),
            _ => return,
        };
        let command = exec.cmd.clone();
        let workdir = exec.workdir.clone();
        let title = format!("Docker [{}]: {}", container, cmd.name);

        self.start_popup_command(title);
        let timeout_duration = self.refresh_config.cluster_operation_timeout;
        let (ctx, output_tx) = CommandContext::new(self.message_tx.clone(), timeout_duration);

        tokio::spawn(async move {
            ctx.execute(move |tx| async move {
                run_docker_command(&container, &command, &workdir, tx).await
            })
            .await;
            drop(output_tx);
        });
    }

    fn start_popup_command(&mut self, title: String) {
        self.output.clear();
        self.output.set_title(title.clone());
        self.output_popup.clear();
        self.output_popup.set_title(title);
        self.is_executing = true;
        self.status_bar.set_executing(true);
        self.mode = super::AppMode::OutputPopup;

        let cancel_token = CancellationToken::new();
        self.cancel_token = Some(cancel_token);
    }

    pub(super) fn trigger_manual_hosts_update(&mut self) {
        self.output.clear();
        self.output.set_title("Updating /etc/hosts".to_string());
        self.output_popup.clear();
        self.output_popup
            .set_title("Updating /etc/hosts".to_string());
        self.is_executing = true;
        self.status_bar.set_executing(true);
        self.mode = super::AppMode::OutputPopup;

        let timeout = self.refresh_config.manual_hosts_timeout;
        let (ctx, tx) = CommandContext::new(self.message_tx.clone(), timeout);
        let message_tx = self.message_tx.clone();
        let domain = self.cluster_config.domain.clone();

        tokio::spawn(async move {
            ctx.execute(move |_output_tx| async move {
                let mut ingress_manager = IngressManager::with_domain(domain);
                let result = ingress_manager
                    .update_hosts(Some(tx))
                    .await
                    .map_err(|e| format!("Failed to update /etc/hosts: {}", e))?;

                // If sudo is needed, send message back to main thread for interactive handling
                if let HostsUpdateResult::NeedsSudo { content, count } = result {
                    let _ = message_tx
                        .send(AppMessage::NeedsSudoHostsWrite { content, count })
                        .await;
                }

                Ok(())
            })
            .await;
        });
    }

    /// Run cluster diagnostics
    pub(super) fn run_diagnostics(&mut self) {
        self.diagnostics_overlay.reset();
        self.mode = AppMode::Diagnostics;

        let message_tx = self.message_tx.clone();
        let cluster_config = Arc::clone(&self.cluster_config);

        tokio::spawn(async move {
            run_all_diagnostics(cluster_config, message_tx).await;
        });
    }

    /// Run preflight checks (can run without a started cluster)
    pub(super) fn run_preflight_check(&mut self) {
        self.diagnostics_overlay.reset();
        self.mode = AppMode::Diagnostics;

        let message_tx = self.message_tx.clone();
        let cluster_config = Arc::clone(&self.cluster_config);

        tokio::spawn(async move {
            run_preflight_checks(cluster_config, message_tx).await;
        });
    }

    /// Auto-open detail panel when a pod is selected, close when no pods
    pub(super) fn ensure_detail_panel_synced(&mut self) {
        if let Some(pod) = self.pod_stats.selected_pod() {
            let name = pod.name.clone();
            let ns = pod.namespace.clone();
            if !self.pod_detail_panel.is_open() {
                // First open — default to Logs tab
                self.pod_detail_panel.open(name, ns, DetailTab::Logs);
                self.pod_detail_panel
                    .set_volume_entries(self.volume_entries_cache.clone());
                self.load_active_tab();
            } else if self.pod_detail_panel.pod_name() != name {
                // Pod changed (e.g. pod list shifted) — refresh
                let tab = self.pod_detail_panel.active_tab();
                self.pod_detail_panel.open(name, ns, tab);
                self.pod_detail_panel
                    .set_volume_entries(self.volume_entries_cache.clone());
                self.load_active_tab();
            }
        } else if self.pod_detail_panel.is_open() {
            // No pods left — close panel and shell session
            self.pod_detail_panel.close();
            if let Some(session) = self.shell_session.take() {
                session.close();
            }
            if self.mode == AppMode::Shell {
                self.mode = AppMode::Normal;
            }
        }
    }

    /// Open detail panel on a tab (or switch tab if already open)
    pub(super) fn open_or_switch_detail_tab(&mut self, tab: DetailTab) {
        if let Some(pod) = self.pod_stats.selected_pod() {
            let name = pod.name.clone();
            let ns = pod.namespace.clone();
            if !self.pod_detail_panel.is_open() || self.pod_detail_panel.pod_name() != name {
                self.pod_detail_panel.open(name, ns, tab);
            } else {
                self.pod_detail_panel.set_tab(tab);
            }
            self.load_active_tab();
        }
    }

    /// Load data for the currently active tab
    pub(super) fn load_active_tab(&mut self) {
        let tab = self.pod_detail_panel.active_tab();
        let pod_name = self.pod_detail_panel.pod_name().to_string();
        let namespace = self.pod_detail_panel.namespace().to_string();

        match tab {
            DetailTab::Logs => self.load_detail_logs(&pod_name, &namespace),
            DetailTab::Describe => self.load_detail_describe(&pod_name, &namespace),
            DetailTab::Timeline => self.load_detail_timeline(&pod_name, &namespace),
            DetailTab::Volumes => self.update_detail_panel_volumes(),
            DetailTab::Shell => self.activate_shell_tab(),
        }
    }

    pub(super) fn load_detail_logs(&mut self, pod_name: &str, namespace: &str) {
        let k8s_client = match &self.k8s_client {
            Some(c) => c.clone(),
            None => return,
        };
        self.pod_detail_panel.set_loading(DetailTab::Logs, true);

        let message_tx = self.message_tx.clone();
        let namespace = namespace.to_string();
        let pod_name = pod_name.to_string();

        tokio::spawn(async move {
            match k8s_client
                .get_pod_logs(&namespace, &pod_name, None, Some(100))
                .await
            {
                Ok(logs) => {
                    let lines: Vec<String> = logs.lines().map(|l| l.to_string()).collect();
                    let _ = message_tx.send(AppMessage::PodLogsLoaded(lines)).await;
                }
                Err(e) => {
                    let _ = message_tx
                        .send(AppMessage::Error(format!("Failed to get logs: {}", e)))
                        .await;
                }
            }
        });
    }

    fn load_detail_describe(&mut self, pod_name: &str, namespace: &str) {
        let k8s_client = match &self.k8s_client {
            Some(c) => c.clone(),
            None => return,
        };
        self.pod_detail_panel.set_loading(DetailTab::Describe, true);

        let message_tx = self.message_tx.clone();
        let namespace = namespace.to_string();
        let pod_name = pod_name.to_string();

        tokio::spawn(async move {
            match k8s_client.describe_pod(&namespace, &pod_name).await {
                Ok(description) => {
                    let lines: Vec<String> = description.lines().map(|l| l.to_string()).collect();
                    let _ = message_tx.send(AppMessage::PodDescribeLoaded(lines)).await;
                }
                Err(e) => {
                    let _ = message_tx
                        .send(AppMessage::Error(format!("Failed to describe pod: {}", e)))
                        .await;
                }
            }
        });
    }

    fn load_detail_timeline(&mut self, pod_name: &str, namespace: &str) {
        let k8s_client = match &self.k8s_client {
            Some(c) => c.clone(),
            None => return,
        };
        self.pod_detail_panel.set_loading(DetailTab::Timeline, true);

        let client = k8s_client.client().clone();
        let message_tx = self.message_tx.clone();
        let namespace = namespace.to_string();
        let pod_name = pod_name.to_string();

        tokio::spawn(async move {
            match crate::k8s::get_pod_timeline(&client, &namespace, &pod_name).await {
                Ok(timeline) => {
                    let _ = message_tx
                        .send(AppMessage::PodTimelineLoaded(timeline))
                        .await;
                }
                Err(e) => {
                    let _ = message_tx
                        .send(AppMessage::Error(format!("Failed to load timeline: {}", e)))
                        .await;
                }
            }
        });
    }

    /// Activate the shell tab — reuse existing session or start a new one
    fn activate_shell_tab(&mut self) {
        let pod_name = self.pod_detail_panel.pod_name().to_string();
        let namespace = self.pod_detail_panel.namespace().to_string();

        // Reuse existing session if it's for the same pod
        if let Some(session) = &self.shell_session {
            if session.pod_name() == pod_name && session.namespace() == namespace {
                // Ensure shell_view exists (might have been cleared by open())
                if !self.pod_detail_panel.has_shell_view() {
                    let (rows, cols) = self.calculate_shell_dimensions();
                    self.pod_detail_panel.init_shell_view(rows, cols);
                    self.pod_detail_panel.set_shell_connected();
                }
                return;
            }
            // Different pod — close old session
            session.close();
            self.shell_session = None;
        }

        // Start new session
        self.spawn_shell_session(&pod_name, &namespace, None);
    }

    /// Spawn a new shell session for the given pod
    pub(super) fn spawn_shell_session(
        &mut self,
        pod_name: &str,
        namespace: &str,
        container: Option<&str>,
    ) {
        let k8s_client = match &self.k8s_client {
            Some(c) => c.clone(),
            None => {
                self.pod_detail_panel
                    .set_shell_error("Kubernetes client not connected".into());
                return;
            }
        };

        let (rows, cols) = self.calculate_shell_dimensions();
        self.pod_detail_panel.init_shell_view(rows, cols);
        self.shell_area_size = (rows, cols);

        let client = k8s_client.client().clone();
        let message_tx = self.message_tx.clone();
        let pod_name = pod_name.to_string();
        let namespace = namespace.to_string();
        let container = container.map(|s| s.to_string());

        tokio::spawn(async move {
            crate::k8s::shell_session::start_shell_session(
                client, pod_name, namespace, container, message_tx,
            )
            .await;
        });
    }

    /// Calculate shell content area dimensions from current layout
    pub(super) fn calculate_shell_dimensions(&self) -> (u16, u16) {
        let layout = match &self.current_layout {
            Some(l) => l,
            None => return (24, 80),
        };
        let area = layout.pod_stats;
        // Outer block: -2 rows (borders), -2 cols (borders)
        // 50% split for pod list, 1 row separator, rest for detail
        // Detail: -1 row for tab bar
        let inner_height = area.height.saturating_sub(2);
        let pod_list_height = inner_height / 2;
        let detail_height = inner_height.saturating_sub(pod_list_height + 1);
        let content_height = detail_height.saturating_sub(1);
        let content_width = area.width.saturating_sub(2);
        (content_height, content_width)
    }

    /// Compute which pod names should be highlighted based on the selected menu item
    pub(super) fn compute_highlighted_pods(&self) -> HashSet<String> {
        let mut result = HashSet::new();

        let items = self.menu.flat_items();
        let selected = match self.menu.selected_item() {
            Some(s) => s,
            None => return result,
        };

        // Collect owned K8s targets to match against pods (other targets don't highlight)
        let mut targets: Vec<KubernetesTargetOwned> = Vec::new();

        // For leaf commands, use the target directly
        if let Some(cmd) = &selected.command {
            if let Some(exec) = &cmd.exec {
                if let Some(t) = kubernetes_target_owned(&exec.target) {
                    targets.push(t);
                }
            }
        }

        // For groups/parents, collect child targets
        if selected.has_children || selected.is_group {
            if selected.is_group {
                // Top-level group — use original command data (works even if collapsed)
                let group_cmds = self.menu.group_commands(selected.group_index);
                collect_targets_recursive(group_cmds, &mut targets);
            } else {
                // Nested parent with children — walk flat_items forward
                let selected_level = selected.level;
                let selected_idx = self.menu.selected_index();

                for item in items.iter().skip(selected_idx + 1) {
                    if item.level <= selected_level {
                        break;
                    }
                    if let Some(cmd) = &item.command {
                        if let Some(exec) = &cmd.exec {
                            if let Some(t) = kubernetes_target_owned(&exec.target) {
                                targets.push(t);
                            }
                        }
                    }
                }
            }
        }

        // Match targets against pods
        let pods = self.pod_stats.pods();
        for target in &targets {
            for pod in pods {
                if matches_target(target, pod) {
                    result.insert(pod.name.clone());
                }
            }
        }

        result
    }

    /// Update pod highlights based on currently selected menu item
    pub(super) fn update_pod_highlights(&mut self) {
        if self.focus == FocusArea::Content {
            let highlighted = self.compute_highlighted_pods();
            if !highlighted.is_empty() {
                tracing::debug!(count = highlighted.len(), pods = ?highlighted, "Highlighting pods");
            }
            self.pod_stats.set_highlighted_pods(highlighted);
        } else {
            self.pod_stats.set_highlighted_pods(HashSet::new());
        }
    }
}

/// Owned snapshot of a Kubernetes target — used for pod-highlight matching.
struct KubernetesTargetOwned {
    namespace: String,
    selector: String,
    pod_name: String,
}

fn kubernetes_target_owned(target: &ExecutionTarget) -> Option<KubernetesTargetOwned> {
    target.as_kubernetes().map(|k| KubernetesTargetOwned {
        namespace: k.namespace.to_string(),
        selector: k.selector.to_string(),
        pod_name: k.pod_name.to_string(),
    })
}

/// Recursively collect all Kubernetes targets from a list of command entries
fn collect_targets_recursive(
    entries: &[crate::config::CommandEntry],
    targets: &mut Vec<KubernetesTargetOwned>,
) {
    for entry in entries {
        if let Some(exec) = &entry.exec {
            if let Some(t) = kubernetes_target_owned(&exec.target) {
                targets.push(t);
            }
        }
        if !entry.commands.is_empty() {
            collect_targets_recursive(&entry.commands, targets);
        }
    }
}

/// Check if a Kubernetes target matches a pod
fn matches_target(target: &KubernetesTargetOwned, pod: &crate::ui::components::PodStat) -> bool {
    // Check namespace if specified
    if !target.namespace.is_empty() && target.namespace != pod.namespace {
        return false;
    }

    // Match by pod_name
    if !target.pod_name.is_empty() {
        return pod.name.contains(&target.pod_name);
    }

    // Match by selector (extract values from key=value pairs)
    if !target.selector.is_empty() {
        for part in target.selector.split(',') {
            let part: &str = part.trim();
            if let Some((_key, value)) = part.split_once('=') {
                if pod.name.contains(value) {
                    return true;
                }
            }
        }
        return false;
    }

    // If only namespace specified, match all pods in that namespace
    !target.namespace.is_empty()
}

/// Run a host-side shell command, streaming combined stdout+stderr to the popup.
async fn run_host_command(
    command: &str,
    workdir: &str,
    output_tx: tokio::sync::mpsc::Sender<crate::ui::components::OutputLine>,
) -> Result<(), String> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    if !workdir.is_empty() {
        cmd.current_dir(workdir);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn host command: {}", e))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_tx = output_tx.clone();
    let stdout_handle = tokio::spawn(async move {
        if let Some(out) = stdout {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = stdout_tx
                    .send(crate::ui::components::OutputLine::info(line))
                    .await;
            }
        }
    });

    let stderr_tx = output_tx.clone();
    let stderr_handle = tokio::spawn(async move {
        if let Some(err) = stderr {
            let mut reader = BufReader::new(err).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = stderr_tx
                    .send(crate::ui::components::OutputLine::error(line))
                    .await;
            }
        }
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait on host command: {}", e))?;

    let _ = stdout_handle.await;
    let _ = stderr_handle.await;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Host command exited with code {}",
            status.code().unwrap_or(-1)
        ))
    }
}

/// Run a one-shot command inside a docker container, sending output to the popup.
async fn run_docker_command(
    container: &str,
    command: &str,
    workdir: &str,
    output_tx: tokio::sync::mpsc::Sender<crate::ui::components::OutputLine>,
) -> Result<(), String> {
    let docker = DockerManager::from_default_socket()
        .map_err(|e| format!("Failed to connect to docker: {}", e))?;

    // Compose `cd <workdir> && <cmd>` so the user's workdir is honored.
    let full_cmd = if workdir.is_empty() {
        command.to_string()
    } else {
        format!("cd {} && {}", workdir, command)
    };

    match docker
        .exec_in_container(container, &["sh", "-c", &full_cmd])
        .await
    {
        Ok(out) => {
            for line in out.lines() {
                let _ = output_tx
                    .send(crate::ui::components::OutputLine::info(line.to_string()))
                    .await;
            }
            Ok(())
        }
        Err(e) => Err(format!("docker exec failed: {}", e)),
    }
}
