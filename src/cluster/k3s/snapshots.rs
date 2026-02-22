//! Snapshot-based startup optimization
//!
//! This module provides snapshot functionality for faster cluster startup:
//! - Creating snapshots of initialized clusters
//! - Starting clusters from snapshots
//! - Deep snapshots (post-Traefik) for skipping wait_for_cluster_ready
//! - Cleaning up old snapshots

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::K3sManager;
use crate::cluster::config::ClusterConfig;
use crate::cluster::docker::{ContainerRunConfig, DockerManager};
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
    /// Excludes: cluster_name, speedup settings, logging config
    pub(super) fn calculate_config_hash(&self) -> String {
        Self::calculate_config_hash_static(&self.config)
    }

    /// Compute snapshot image name from config (static version)
    pub(crate) fn compute_snapshot_image_name(config: &ClusterConfig) -> String {
        let version = Self::sanitize_version(&config.k3s_version);
        let hash = Self::calculate_config_hash_static(config);
        format!("k3dev-snapshot-{}-{}", version, hash)
    }

    /// Get snapshot image name based on config hash
    /// Format: k3dev-snapshot-{version}-{hash}
    /// Example: k3dev-snapshot-v1-33-4-k3s1-a7b3c2d1
    pub(super) fn get_snapshot_image_name(&self) -> String {
        Self::compute_snapshot_image_name(&self.config)
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
    fn calculate_config_hash_static(config: &ClusterConfig) -> String {
        let mut hasher = Sha256::new();
        hasher.update(config.k3s_version.as_bytes());
        hasher.update(config.domain.as_bytes());
        hasher.update(config.api_port.to_string().as_bytes());
        hasher.update(config.http_port.to_string().as_bytes());
        hasher.update(config.https_port.to_string().as_bytes());
        for (host, container) in &config.additional_ports {
            hasher.update(format!("{}:{}", host, container).as_bytes());
        }
        hasher.update(Self::RANCHER_DATA_PATH.as_bytes());
        hasher.update(Self::LOCAL_PV_STORAGE_PATH.as_bytes());
        hasher.update(b"--docker");
        hasher.update(b"--disable=metrics-server");
        hasher.update(b"--disable=servicelb");
        let result = hasher.finalize();
        format!("{:x}", result)[..8].to_string()
    }

    /// Create a snapshot of the current running cluster
    pub(super) async fn create_snapshot(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let snapshot_image = self.get_snapshot_image_name();

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
            Self::LOCAL_PV_STORAGE_PATH
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
            self.calculate_config_hash(),
        );
        labels.insert("k3dev.domain".to_string(), self.config.domain.clone());

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
        let snapshot_image = Self::compute_snapshot_image_name(config);

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
            Self::LOCAL_PV_STORAGE_PATH
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
            Self::calculate_config_hash_static(config),
        );
        labels.insert("k3dev.domain".to_string(), config.domain.clone());
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

        // Get docker socket path
        let socket_path = self.platform.docker_socket_path().await?;

        // Build port mappings
        let ports: Vec<(u16, u16)> = self
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

        // K3s server command with snapshot data restoration
        let k3s_command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "set -e && \
                 if [ -d /snapshot-data ]; then \
                   echo 'Restoring cluster state from snapshot...' && \
                   mkdir -p {} {} && \
                   cp -a /snapshot-data/rancher/. {}/ 2>/dev/null || true && \
                   cp -a /snapshot-data/pv/. {}/ 2>/dev/null || true && \
                   echo 'Cluster state restored'; \
                 fi && \
                 nsenter --mount=/proc/1/ns/mnt modprobe br_netfilter 2>/dev/null || true && \
                 sysctl -w net.bridge.bridge-nf-call-iptables=1 2>/dev/null || true && \
                 mkdir -p /run/k3s /sys/fs/cgroup/kubepods && \
                 /bin/k3s server \
                 --docker \
                 --disable=metrics-server \
                 --disable=servicelb \
                 --disable-cloud-controller \
                 --disable-network-policy \
                 --default-local-storage-path {} \
                 --service-node-port-range 80-32767 \
                 --kubelet-arg=root-dir=/var/lib/docker/kubelet \
                 --kubelet-arg=cgroup-driver=cgroupfs \
                 --kube-apiserver-arg=profiling=false \
                 --kube-apiserver-arg=enable-admission-plugins=NodeRestriction \
                 --kube-controller-manager-arg=concurrent-deployment-syncs=1",
                Self::RANCHER_DATA_PATH,
                Self::LOCAL_PV_STORAGE_PATH,
                Self::RANCHER_DATA_PATH,
                Self::LOCAL_PV_STORAGE_PATH,
                Self::LOCAL_PV_STORAGE_PATH
            ),
        ];

        // Run container from snapshot image
        let run_config = ContainerRunConfig {
            name: self.config.container_name.clone(),
            hostname: Some(self.config.container_name.clone()),
            image: snapshot_image.to_string(),
            detach: true,
            privileged: true,
            ports,
            volumes: vec![
                // Docker socket for k3s --docker mode
                (
                    socket_path.to_string_lossy().to_string(),
                    "/var/run/docker.sock".to_string(),
                    String::new(),
                ),
                // Mount real /var/lib/docker
                (
                    "/var/lib/docker".to_string(),
                    "/var/lib/docker".to_string(),
                    "bind-propagation=rshared".to_string(),
                ),
                // Docker volume for rancher data
                (
                    Self::RANCHER_VOLUME_NAME.to_string(),
                    Self::RANCHER_DATA_PATH.to_string(),
                    "volume".to_string(),
                ),
                // Docker volume for local PV storage
                (
                    Self::LOCAL_PV_VOLUME_NAME.to_string(),
                    Self::LOCAL_PV_STORAGE_PATH.to_string(),
                    "volume".to_string(),
                ),
            ],
            env: vec![],
            network: Some(self.config.network_name.clone()),
            cgroupns_host: true,
            pid_host: true,
            entrypoint: Some(String::new()),
            command: Some(k3s_command),
        };

        // Ensure prerequisites exist (volumes, network)
        let _ = output_tx
            .send(OutputLine::info("Ensuring prerequisites..."))
            .await;
        tokio::try_join!(
            self.docker.create_volume(Self::RANCHER_VOLUME_NAME),
            self.docker.create_volume(Self::LOCAL_PV_VOLUME_NAME),
            self.docker.create_network(&self.config.network_name),
        )?;

        // Start container from snapshot
        let _ = output_tx
            .send(OutputLine::info("Starting container from snapshot..."))
            .await;
        self.docker.run_container(&run_config).await?;

        // Wait for API (should be fast since cluster is pre-initialized)
        self.wait_for_api(output_tx).await?;

        // Setup kubeconfig on host (even though snapshot has it in container, we need it on host)
        let _ = output_tx
            .send(OutputLine::info("Setting up kubeconfig..."))
            .await;
        self.setup_kubeconfig().await?;

        if !is_deep {
            // Legacy snapshot: wait for cluster to be fully ready (deployments, etc.)
            self.wait_for_cluster_ready(output_tx).await?;
        }

        // Execute on_cluster_available hooks
        if self.config.hooks.has_hooks() {
            let hook_executor = HookExecutor::new(self.config.hooks.clone());
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
        current_snapshot: &str,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let snapshots = docker.list_images_by_pattern("k3dev-snapshot-").await?;

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
        Self::cleanup_old_snapshots_static(&self.docker, current_snapshot, output_tx).await
    }

    /// Delete all snapshot images for this cluster
    pub async fn delete_snapshots(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let snapshots = self
            .docker
            .list_images_by_pattern("k3dev-snapshot-")
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
