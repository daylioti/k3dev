//! Snapshot-based startup optimization
//!
//! This module provides snapshot functionality for faster cluster startup:
//! - Creating snapshots of initialized clusters
//! - Starting clusters from snapshots
//! - Deep snapshots (post-Traefik) for skipping wait_for_cluster_ready
//! - Cleaning up old snapshots

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::K3sManager;
use crate::cluster::config::ClusterConfig;
use crate::cluster::docker::{ContainerRunConfig, DockerManager};
use crate::cluster::platform::{docker_host_tcp_url, PlatformInfo};
use crate::config::HookEvent;
use crate::hooks::HookExecutor;
use crate::ui::components::OutputLine;

impl K3sManager {
    /// Sanitize k3s version string for use in snapshot image name
    /// Replaces dots and special chars with dashes
    /// Example: "v1.33.4-k3s1" -> "v1-33-4-k3s1"
    pub(super) fn sanitize_version(version: &str) -> String {
        version.replace(['.', '/'], "-")
    }

    /// Calculate config hash from fields that affect cluster state
    /// Excludes: speedup settings, logging config
    pub(super) fn calculate_config_hash(&self, docker_root: &str) -> String {
        Self::calculate_config_hash_static(&self.config, docker_root)
    }

    /// Compute snapshot image name from config (static version)
    pub(crate) fn compute_snapshot_image_name(config: &ClusterConfig, docker_root: &str) -> String {
        let version = Self::sanitize_version(&config.k3s_version);
        let hash = Self::calculate_config_hash_static(config, docker_root);
        format!("{}{}-{}", config.snapshot_prefix(), version, hash)
    }

    /// Get snapshot image name based on config hash
    /// Format: k3dev-snapshot-{cluster}-{version}-{hash}
    /// Example: k3dev-snapshot-k3dev-v1-33-4-k3s1-a7b3c2d1
    pub(super) fn get_snapshot_image_name(&self, docker_root: &str) -> String {
        Self::compute_snapshot_image_name(&self.config, docker_root)
    }

    /// Check if a snapshot image is a deep snapshot (created after Traefik + hooks)
    pub(crate) async fn is_deep_snapshot(docker: &DockerManager, image: &str) -> bool {
        docker
            .get_image_labels(image)
            .await
            .get("k3dev.snapshot.deep")
            .map(|v| v == "true")
            .unwrap_or(false)
    }

    /// Static version of calculate_config_hash
    fn calculate_config_hash_static(config: &ClusterConfig, docker_root: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(config.cluster_name.as_bytes());
        hasher.update(config.k3s_version.as_bytes());
        hasher.update(config.k3s_image_repo.as_bytes());
        hasher.update(config.domain.as_bytes());
        hasher.update(config.api_port.to_string().as_bytes());
        hasher.update(config.http_port.to_string().as_bytes());
        hasher.update(config.https_port.to_string().as_bytes());
        hasher.update(config.use_router.to_string().as_bytes());
        for (host, container) in &config.additional_ports {
            hasher.update(format!("{}:{}", host, container).as_bytes());
        }
        hasher.update(Self::RANCHER_DATA_PATH.as_bytes());
        hasher.update(config.local_pv_storage_path(docker_root).as_bytes());
        hasher.update(config.kubelet_root_dir(docker_root).as_bytes());
        hasher.update(config.cgroup_root().as_bytes());
        hasher.update(config.cluster_cidr().as_bytes());
        hasher.update(config.service_cidr().as_bytes());
        hasher.update(config.cluster_dns().as_bytes());
        hasher.update(b"--docker");
        hasher.update(b"--disable=metrics-server");
        hasher.update(b"--disable=servicelb");
        let result = hasher.finalize();
        format!("{:x}", result)[..8].to_string()
    }

    /// Create a snapshot of the current running cluster
    pub(super) async fn create_snapshot(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let docker_root = self.docker.get_docker_root_dir().await;
        let snapshot_image = self.get_snapshot_image_name(&docker_root);

        let _ = output_tx
            .send(OutputLine::info(format!(
                "Creating cluster snapshot: {}...",
                snapshot_image
            )))
            .await;

        // Step 1: Copy volume data into container filesystem for snapshot
        let _ = output_tx
            .send(OutputLine::info("Saving cluster state into snapshot..."))
            .await;

        let copy_cmd = format!(
            "mkdir -p /snapshot-data && \
             rm -rf /snapshot-data/rancher /snapshot-data/pv && \
             cp -a {} /snapshot-data/rancher && \
             cp -a {} /snapshot-data/pv",
            Self::RANCHER_DATA_PATH,
            self.config.local_pv_storage_path(&docker_root)
        );

        match self
            .docker
            .exec_in_container(&self.config.container_name, &["sh", "-c", &copy_cmd])
            .await
        {
            Ok(_) => {
                let _ = output_tx
                    .send(OutputLine::info("Cluster state saved to snapshot data"))
                    .await;
            }
            Err(e) => {
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "Failed to save cluster state: {}",
                        e
                    )))
                    .await;
                return Err(e);
            }
        }

        // Step 2: Prepare labels for the snapshot
        let mut labels = HashMap::new();
        labels.insert(
            "k3dev.snapshot.created".to_string(),
            chrono::Utc::now().to_rfc3339(),
        );
        labels.insert(
            "k3dev.k3s_version".to_string(),
            self.config.k3s_version.clone(),
        );
        labels.insert(
            "k3dev.config_hash".to_string(),
            self.calculate_config_hash(&docker_root),
        );
        labels.insert("k3dev.domain".to_string(), self.config.domain.clone());
        labels.insert(
            "k3dev.cluster".to_string(),
            self.config.cluster_name.clone(),
        );

        // Step 3: Commit the running container to an image (includes /snapshot-data/)
        match self
            .docker
            .commit_container(&self.config.container_name, &snapshot_image, labels)
            .await
        {
            Ok(()) => {
                let _ = output_tx
                    .send(OutputLine::success(format!(
                        "Snapshot created: {}",
                        snapshot_image
                    )))
                    .await;
                Ok(())
            }
            Err(e) => {
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "Snapshot creation failed (cluster still usable): {}",
                        e
                    )))
                    .await;
                // Don't fail the entire start operation
                Err(e)
            }
        }
    }

    /// Create a deep snapshot after Traefik + hooks have completed.
    /// This is a static method so it can be called from a background task.
    pub(crate) async fn create_deep_snapshot(
        container_name: &str,
        docker: &DockerManager,
        config: &ClusterConfig,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let docker_root = docker.get_docker_root_dir().await;
        let snapshot_image = Self::compute_snapshot_image_name(config, &docker_root);

        let _ = output_tx
            .send(OutputLine::info(format!(
                "Creating deep snapshot: {}...",
                snapshot_image
            )))
            .await;

        // Copy volume data into container filesystem for snapshot
        let copy_cmd = format!(
            "mkdir -p /snapshot-data && \
             rm -rf /snapshot-data/rancher /snapshot-data/pv && \
             cp -a {} /snapshot-data/rancher && \
             cp -a {} /snapshot-data/pv",
            Self::RANCHER_DATA_PATH,
            config.local_pv_storage_path(&docker_root)
        );

        docker
            .exec_in_container(container_name, &["sh", "-c", &copy_cmd])
            .await?;

        // Prepare labels — same as regular snapshot but with deep flag
        let mut labels = HashMap::new();
        labels.insert(
            "k3dev.snapshot.created".to_string(),
            chrono::Utc::now().to_rfc3339(),
        );
        labels.insert("k3dev.k3s_version".to_string(), config.k3s_version.clone());
        labels.insert(
            "k3dev.config_hash".to_string(),
            Self::calculate_config_hash_static(config, &docker_root),
        );
        labels.insert("k3dev.domain".to_string(), config.domain.clone());
        labels.insert("k3dev.cluster".to_string(), config.cluster_name.clone());
        labels.insert("k3dev.snapshot.deep".to_string(), "true".to_string());

        docker
            .commit_container(container_name, &snapshot_image, labels)
            .await?;

        let _ = output_tx
            .send(OutputLine::success(format!(
                "Deep snapshot created: {}",
                snapshot_image
            )))
            .await;

        Ok(())
    }

    /// Start cluster from a snapshot image (fast path)
    /// If `is_deep` is true, skip wait_for_cluster_ready (coredns, local-path-provisioner, configmap)
    pub(super) async fn start_from_snapshot(
        &mut self,
        snapshot_image: &str,
        is_deep: bool,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        if is_deep {
            let _ = output_tx
                .send(OutputLine::info(format!(
                    "Deep snapshot detected: {} (skipping deployment waits)",
                    snapshot_image
                )))
                .await;
        } else {
            let _ = output_tx
                .send(OutputLine::info(format!(
                    "Starting from snapshot: {}",
                    snapshot_image
                )))
                .await;
        }

        // Get docker socket path, docker root, and iptables mode
        let socket_path = self.platform.docker_socket_path().await?;
        let docker_root = self.docker.get_docker_root_dir().await;
        let pv_storage_path = self.config.local_pv_storage_path(&docker_root);
        let iptables_mode = PlatformInfo::detect_iptables_mode();

        // Build port mappings
        #[allow(unused_mut)]
        let mut ports: Vec<(u16, u16)> = self
            .config
            .port_mappings()
            .iter()
            .filter_map(|p| {
                let parts: Vec<&str> = p.split(':').collect();
                if parts.len() == 2 {
                    Some((parts[0].parse().ok()?, parts[1].parse().ok()?))
                } else {
                    None
                }
            })
            .collect();

        #[cfg(target_os = "macos")]
        {
            let relay_port = crate::cluster::platform::find_available_port(2375).unwrap_or(2375);
            ports.push((relay_port, relay_port));
        }

        #[cfg(target_os = "macos")]
        let ports = {
            let mut p = ports;
            p.push((2375, 2375));
            p
        };

        // See create_cluster: the proxy needs a unix socket to forward to.
        let use_proxy = docker_host_tcp_url().is_none();

        // K3s server command with snapshot data restoration
        let k3s_command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "set -e && \
                 if [ -d /snapshot-data ]; then \
                   echo 'Restoring cluster state from snapshot...' && \
                   mkdir -p {rancher} {pv} && \
                   cp -a /snapshot-data/rancher/. {rancher}/ 2>/dev/null || true && \
                   cp -a /snapshot-data/pv/. {pv}/ 2>/dev/null || true && \
                   echo 'Cluster state restored'; \
                 fi && \
                 nsenter --mount=/proc/1/ns/mnt modprobe br_netfilter 2>/dev/null || true && \
                 sysctl -w net.bridge.bridge-nf-call-iptables=1 2>/dev/null || true && \
                 mkdir -p /run/k3s {cgroup_path} && \
                 {proxy}{server}",
                rancher = Self::RANCHER_DATA_PATH,
                pv = pv_storage_path,
                cgroup_path = self.config.cgroup_root_path(),
                proxy = if use_proxy {
                    Self::criproxy_prologue(&self.config)
                } else {
                    String::new()
                },
                server = Self::k3s_server_args(&self.config, &docker_root, use_proxy),
            ),
        ];

        // Build volumes and env - handle TCP Docker (no socket file to mount)
        let tcp_url = docker_host_tcp_url();
        let mut volumes = vec![
            // Mount Docker data directory - required for k3s --docker mode
            (
                docker_root.clone(),
                docker_root.clone(),
                "bind-propagation=rshared".to_string(),
            ),
            // Docker volume for rancher data
            (
                self.config.rancher_volume_name(),
                Self::RANCHER_DATA_PATH.to_string(),
                "volume".to_string(),
            ),
            // Docker volume for local PV storage
            (
                self.config.local_pv_volume_name(),
                pv_storage_path.clone(),
                "volume".to_string(),
            ),
        ];
        let mut env = vec![("IPTABLES_MODE".to_string(), iptables_mode.to_string())];

        if let Some(ref url) = tcp_url {
            tracing::info!(docker_host = %url, "TCP Docker detected, passing DOCKER_HOST to k3s container");
            env.push(("DOCKER_HOST".to_string(), url.clone()));
        } else {
            volumes.insert(
                0,
                (
                    self.platform.docker_socket_mount_source(&socket_path),
                    Self::HOST_DOCKER_SOCK.to_string(),
                    String::new(),
                ),
            );
        }

        // Run container from snapshot image
        let run_config = ContainerRunConfig {
            name: self.config.container_name.clone(),
            hostname: Some(self.config.container_name.clone()),
            image: snapshot_image.to_string(),
            detach: true,
            privileged: true,
            ports,
            volumes,
            env,
            network: Some(self.config.network_name.clone()),
            cgroupns_host: true,
            pid_host: true,
            entrypoint: Some(String::new()),
            command: Some(k3s_command),
            security_opt: vec!["apparmor=unconfined".to_string()],
            labels: Self::cluster_labels(&self.config),
            auto_remove: false,
        };

        // Ensure prerequisites exist (volumes, network)
        let _ = output_tx
            .send(OutputLine::info("Ensuring prerequisites..."))
            .await;
        let rancher_volume = self.config.rancher_volume_name();
        let pv_volume = self.config.local_pv_volume_name();
        tokio::try_join!(
            self.docker.create_volume(&rancher_volume),
            self.docker.create_volume(&pv_volume),
            self.docker.create_network(&self.config.network_name),
        )?;

        // Start container from snapshot
        let _ = output_tx
            .send(OutputLine::info("Starting container from snapshot..."))
            .await;
        self.docker.run_container(&run_config).await?;

        // The entrypoint blocks until this lands (see create_cluster)
        if use_proxy {
            self.install_criproxy()
                .await
                .context("Failed to install CRI filtering proxy")?;
        }

        // Wait for API (should be fast since cluster is pre-initialized)
        self.wait_for_api(output_tx).await?;

        // Setup kubeconfig and install agent in parallel
        let _ = output_tx
            .send(OutputLine::info("Setting up kubeconfig..."))
            .await;
        let (kubeconfig_result, agent_result) =
            tokio::join!(self.setup_kubeconfig(), self.install_agent(),);
        kubeconfig_result?;
        if let Err(e) = &agent_result {
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Agent install failed (stats will use fallback): {:#}",
                    e
                )))
                .await;
        }

        if !is_deep {
            // Legacy snapshot: wait for cluster to be fully ready (deployments, etc.)
            self.wait_for_cluster_ready(output_tx).await?;
        }

        // Execute on_cluster_available hooks
        if self.config.hooks.has_hooks() {
            let hook_executor = HookExecutor::new(
                self.config.hooks.clone(),
                self.config.pinned_kubeconfig(),
                self.config.context_name(),
            );
            hook_executor
                .execute_hooks(HookEvent::OnClusterAvailable, output_tx.clone())
                .await?;
        }

        let _ = output_tx
            .send(OutputLine::success("K3s cluster started from snapshot!"))
            .await;

        Ok(())
    }

    /// Cleanup old snapshots (static version for use from background tasks)
    pub(crate) async fn cleanup_old_snapshots_static(
        docker: &DockerManager,
        prefix: &str,
        cluster: &str,
        current_snapshot: &str,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        // Scoped to this cluster's prefix *and* label — the global
        // `k3dev-snapshot-` sweep would delete every other cluster's snapshot,
        // and the prefix alone still matches a cluster whose name extends this
        // one ("one" vs "one-two").
        let snapshots = docker.list_images_by_pattern(prefix, cluster).await?;

        if snapshots.is_empty() {
            return Ok(());
        }

        let mut removed_count = 0;
        for snapshot in snapshots {
            if snapshot.starts_with(current_snapshot) {
                continue;
            }
            match docker.remove_image(&snapshot).await {
                Ok(()) => {
                    tracing::debug!(snapshot = %snapshot, "Removed old snapshot");
                    removed_count += 1;
                }
                Err(e) => {
                    tracing::warn!(snapshot = %snapshot, error = %e, "Failed to remove old snapshot");
                }
            }
        }

        if removed_count > 0 {
            let _ = output_tx
                .send(OutputLine::info(format!(
                    "Cleaned up {} old snapshot(s)",
                    removed_count
                )))
                .await;
        }

        Ok(())
    }

    /// Cleanup old snapshots (delete all k3dev-snapshot-* images except current)
    pub(super) async fn cleanup_old_snapshots(
        &self,
        current_snapshot: &str,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        Self::cleanup_old_snapshots_static(
            &self.docker,
            &self.config.snapshot_prefix(),
            &self.config.cluster_name,
            current_snapshot,
            output_tx,
        )
        .await
    }

    /// Delete all snapshot images for this cluster
    pub async fn delete_snapshots(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let snapshots = self
            .docker
            .list_images_by_pattern(&self.config.snapshot_prefix(), &self.config.cluster_name)
            .await?;

        if snapshots.is_empty() {
            return Ok(());
        }

        let _ = output_tx
            .send(OutputLine::info("Removing snapshot images..."))
            .await;

        let mut removed_count = 0;
        for snapshot in snapshots {
            match self.docker.remove_image(&snapshot).await {
                Ok(()) => {
                    tracing::debug!(snapshot = %snapshot, "Removed snapshot");
                    removed_count += 1;
                }
                Err(e) => {
                    tracing::warn!(snapshot = %snapshot, error = %e, "Failed to remove snapshot");
                }
            }
        }

        if removed_count > 0 {
            let _ = output_tx
                .send(OutputLine::info(format!(
                    "Removed {} snapshot image(s)",
                    removed_count
                )))
                .await;
        }

        Ok(())
    }
}
