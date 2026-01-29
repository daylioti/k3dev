//! Command execution logic
//!
//! This module contains command execution functions including cluster actions,
//! pod commands, and palette command handling.

use std::collections::HashMap;
use std::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cluster::{ClusterManager, IngressManager};
use crate::commands::{CommandContext, PaletteCommandId};
use crate::config::{get_exec_placeholders, CommandEntry, RefreshTask};
use crate::k8s::PodExecutor;
use crate::ui::components::{ClusterAction, OutputLine};

use super::{App, AppMessage, AppMode, FocusArea};

impl App {
    /// Open a URL in the default browser
    pub(super) fn open_url(&mut self, url: &str) {
        use std::process::Stdio;

        // Use xdg-open on Linux, open on macOS
        // Redirect stdin/stdout/stderr to null to prevent terminal corruption
        #[cfg(target_os = "linux")]
        let result = Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        #[cfg(target_os = "macos")]
        let result = Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        #[cfg(target_os = "windows")]
        let result = Command::new("cmd")
            .args(["/C", "start", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match result {
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

    /// Check if sudo requires a password (non-interactive check)
    fn sudo_needs_password() -> bool {
        std::process::Command::new("sudo")
            .args(["-n", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
    }

    pub(super) fn execute_cluster_action(&mut self, action: ClusterAction) {
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

        let cluster_config = self.cluster_config.clone();

        tokio::spawn(async move {
            ctx.execute(move |_output_tx| async move {
                let mut manager = ClusterManager::new(Some(cluster_config))
                    .await
                    .map_err(|e| format!("Manager error: {}", e))?;

                let action_result = match action {
                    ClusterAction::Start => manager.start(tx).await,
                    ClusterAction::Stop => manager.stop(tx).await,
                    ClusterAction::Restart => manager.restart(tx).await,
                    ClusterAction::Destroy => manager.delete(tx).await,
                    ClusterAction::Info => manager.info(tx).await,
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

        self.execute_pod_command(&cmd);
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
            for (key, value) in &values {
                let pattern = format!("@{}", key);
                exec.target.namespace = exec.target.namespace.replace(&pattern, value);
                exec.target.selector = exec.target.selector.replace(&pattern, value);
                exec.target.pod_name = exec.target.pod_name.replace(&pattern, value);
                exec.target.container = exec.target.container.replace(&pattern, value);
                exec.workdir = exec.workdir.replace(&pattern, value);
                exec.cmd = exec.cmd.replace(&pattern, value);
            }
        }

        self.execute_pod_command(&cmd);
    }

    fn execute_pod_command(&mut self, cmd: &crate::config::CommandEntry) {
        let exec = match &cmd.exec {
            Some(e) => e,
            None => return,
        };

        let k8s_client = match &self.k8s_client {
            Some(c) => c,
            None => {
                self.output.add_error("Kubernetes client not connected");
                return;
            }
        };

        self.output.clear();
        self.output.set_title(&cmd.name);
        self.output_popup.clear();
        self.output_popup.set_title(&cmd.name);
        self.is_executing = true;
        self.status_bar.set_executing(true);
        // Show output popup immediately when command starts
        self.mode = super::AppMode::OutputPopup;

        let cancel_token = CancellationToken::new();
        self.cancel_token = Some(cancel_token.clone());

        let (tx, mut rx) = mpsc::channel::<OutputLine>(100);
        let message_tx = self.message_tx.clone();

        // Spawn output forwarder
        let msg_tx = message_tx.clone();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let _ = msg_tx.send(AppMessage::OutputLine(line)).await;
            }
        });

        let namespace = exec.target.namespace.clone();
        let selector = exec.target.selector.clone();
        let pod_name = exec.target.pod_name.clone();
        let container = exec.target.container.clone();
        let workdir = exec.workdir.clone();
        let command = exec.cmd.clone();
        let executor = PodExecutor::new(k8s_client);

        tokio::spawn(async move {
            // Find pod
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
                    let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
                    return;
                }
            };

            let _ = message_tx
                .send(AppMessage::OutputLine(OutputLine::info(format!(
                    "Executing on pod: {}",
                    pod.name
                ))))
                .await;

            // Execute command
            let result = executor
                .exec_with_workdir_streaming(
                    &namespace,
                    &pod.name,
                    if container.is_empty() {
                        None
                    } else {
                        Some(container.as_str())
                    },
                    &workdir,
                    &command,
                    tx,
                    cancel_token,
                )
                .await;

            match result {
                Ok(exit_code) => {
                    let _ = message_tx
                        .send(AppMessage::CommandComplete(exit_code))
                        .await;
                }
                Err(e) => {
                    let _ = message_tx
                        .send(AppMessage::Error(format!("Execution error: {}", e)))
                        .await;
                    let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
                }
            }
        });
    }

    pub(super) fn trigger_manual_hosts_update(&mut self) {
        if Self::sudo_needs_password() && self.sudo_password.is_empty() {
            self.pending_hosts_update = true;
            self.password_input.clear();
            self.password_popup
                .set_message("Updating /etc/hosts requires sudo privileges.");
            self.mode = AppMode::SudoPassword;
            return;
        }

        self.do_manual_hosts_update();
    }

    pub(super) fn do_manual_hosts_update(&mut self) {
        self.output.clear();
        self.output.set_title("Updating /etc/hosts".to_string());
        self.output_popup.clear();
        self.output_popup
            .set_title("Updating /etc/hosts".to_string());
        self.is_executing = true;
        self.status_bar.set_executing(true);
        // Show output popup immediately when command starts
        self.mode = super::AppMode::OutputPopup;

        let timeout = self.refresh_config.manual_hosts_timeout;
        let (ctx, tx) = CommandContext::new(self.message_tx.clone(), timeout);

        let domain = self.cluster_config.domain.clone();
        let sudo_password = if self.sudo_password.is_empty() {
            None
        } else {
            Some(self.sudo_password.clone())
        };

        tokio::spawn(async move {
            ctx.execute(move |_output_tx| async move {
                let mut ingress_manager =
                    IngressManager::with_domain_and_sudo(domain, sudo_password);
                ingress_manager
                    .update_hosts(Some(tx))
                    .await
                    .map_err(|e| format!("Failed to update /etc/hosts: {}", e))
            })
            .await;
        });
    }

    /// Execute a pod action from the context menu
    pub(super) fn execute_pod_action(
        &mut self,
        action: crate::ui::components::PodAction,
        pod_name: &str,
        namespace: &str,
    ) {
        use crate::ui::components::PodAction;

        let k8s_client = match &self.k8s_client {
            Some(c) => c.clone(),
            None => {
                self.output.add_error("Kubernetes client not connected");
                return;
            }
        };

        let action_name = action.label();
        self.output.clear();
        self.output
            .set_title(format!("{} - {}", action_name, pod_name));
        self.output_popup.clear();
        self.output_popup
            .set_title(format!("{} - {}", action_name, pod_name));

        match action {
            PodAction::ViewLogs => {
                self.is_executing = true;
                self.status_bar.set_executing(true);
                self.mode = super::AppMode::OutputPopup;

                let (tx, mut rx) = mpsc::channel::<OutputLine>(100);
                let message_tx = self.message_tx.clone();

                // Spawn output forwarder
                let msg_tx = message_tx.clone();
                tokio::spawn(async move {
                    while let Some(line) = rx.recv().await {
                        let _ = msg_tx.send(AppMessage::OutputLine(line)).await;
                    }
                });

                let namespace = namespace.to_string();
                let pod_name = pod_name.to_string();

                tokio::spawn(async move {
                    match k8s_client
                        .get_pod_logs(&namespace, &pod_name, None, Some(100))
                        .await
                    {
                        Ok(logs) => {
                            for line in logs.lines() {
                                let _ = tx.send(OutputLine::info(line.to_string())).await;
                            }
                            let _ = message_tx.send(AppMessage::CommandComplete(0)).await;
                        }
                        Err(e) => {
                            let _ = message_tx
                                .send(AppMessage::Error(format!("Failed to get logs: {}", e)))
                                .await;
                            let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
                        }
                    }
                });
            }
            PodAction::Describe => {
                self.is_executing = true;
                self.status_bar.set_executing(true);
                self.mode = super::AppMode::OutputPopup;

                let (tx, mut rx) = mpsc::channel::<OutputLine>(100);
                let message_tx = self.message_tx.clone();

                // Spawn output forwarder
                let msg_tx = message_tx.clone();
                tokio::spawn(async move {
                    while let Some(line) = rx.recv().await {
                        let _ = msg_tx.send(AppMessage::OutputLine(line)).await;
                    }
                });

                let namespace = namespace.to_string();
                let pod_name = pod_name.to_string();

                tokio::spawn(async move {
                    match k8s_client.describe_pod(&namespace, &pod_name).await {
                        Ok(description) => {
                            for line in description.lines() {
                                let _ = tx.send(OutputLine::info(line.to_string())).await;
                            }
                            let _ = message_tx.send(AppMessage::CommandComplete(0)).await;
                        }
                        Err(e) => {
                            let _ = message_tx
                                .send(AppMessage::Error(format!("Failed to describe pod: {}", e)))
                                .await;
                            let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
                        }
                    }
                });
            }
            PodAction::Delete => {
                self.is_executing = true;
                self.status_bar.set_executing(true);
                self.mode = super::AppMode::OutputPopup;

                let message_tx = self.message_tx.clone();
                let namespace = namespace.to_string();
                let pod_name = pod_name.to_string();

                tokio::spawn(async move {
                    let _ = message_tx
                        .send(AppMessage::OutputLine(OutputLine::info(format!(
                            "Deleting pod {}...",
                            pod_name
                        ))))
                        .await;

                    match k8s_client.delete_pod(&namespace, &pod_name).await {
                        Ok(_) => {
                            let _ = message_tx
                                .send(AppMessage::OutputLine(OutputLine::info(format!(
                                    "Pod {} deleted successfully",
                                    pod_name
                                ))))
                                .await;
                            let _ = message_tx.send(AppMessage::CommandComplete(0)).await;
                        }
                        Err(e) => {
                            let _ = message_tx
                                .send(AppMessage::Error(format!("Failed to delete pod: {}", e)))
                                .await;
                            let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
                        }
                    }
                });
            }
            PodAction::Restart => {
                self.is_executing = true;
                self.status_bar.set_executing(true);
                self.mode = super::AppMode::OutputPopup;

                let message_tx = self.message_tx.clone();
                let namespace = namespace.to_string();
                let pod_name = pod_name.to_string();

                tokio::spawn(async move {
                    let _ = message_tx
                        .send(AppMessage::OutputLine(OutputLine::info(format!(
                            "Restarting pod {} (delete and let deployment recreate)...",
                            pod_name
                        ))))
                        .await;

                    match k8s_client.delete_pod(&namespace, &pod_name).await {
                        Ok(_) => {
                            let _ = message_tx
                                .send(AppMessage::OutputLine(OutputLine::info(format!(
                                    "Pod {} deleted - deployment will recreate it",
                                    pod_name
                                ))))
                                .await;
                            let _ = message_tx.send(AppMessage::CommandComplete(0)).await;
                        }
                        Err(e) => {
                            let _ = message_tx
                                .send(AppMessage::Error(format!("Failed to restart pod: {}", e)))
                                .await;
                            let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
                        }
                    }
                });
            }
            PodAction::ExecShell => {
                // ExecShell requires interactive terminal - show info message
                self.output.add_info(format!(
                    "To exec into pod, run: kubectl exec -it -n {} {} -- /bin/sh",
                    namespace, pod_name
                ));
                self.output_popup.add_line(OutputLine::info(format!(
                    "Interactive shell not supported in TUI mode.\n\nRun this command in a separate terminal:\n\nkubectl exec -it -n {} {} -- /bin/sh",
                    namespace, pod_name
                )));
                self.mode = super::AppMode::OutputPopup;
            }
        }
    }
}
