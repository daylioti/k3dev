//! K3s cluster lifecycle manager
//!
//! This module provides the K3sManager for managing K3s clusters:
//! - Starting, stopping, and deleting clusters
//! - Status checking
//! - Cluster info display
//!
//! The implementation is split across multiple files:
//! - `mod.rs` - Core struct and lifecycle methods
//! - `setup.rs` - Setup utilities (API wait, socat, kubeconfig, etc.)
//! - `snapshots.rs` - Snapshot-based startup optimization
//! - `status.rs` - ClusterStatus enum

mod setup;
mod snapshots;
mod status;

pub use status::ClusterStatus;

use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::config::ClusterConfig;
use super::docker::{ContainerRunConfig, DockerManager};
use super::kube_ops::KubeOps;
use super::platform::PlatformInfo;
use crate::config::HookEvent;
use crate::hooks::HookExecutor;
use crate::ui::components::OutputLine;

/// K3s cluster lifecycle manager
pub struct K3sManager {
    pub(crate) config: Arc<ClusterConfig>,
    pub(crate) docker: DockerManager,
    pub(crate) platform: PlatformInfo,
    pub(crate) kube_ops: KubeOps,
}

impl K3sManager {
    /// Docker volume name for rancher data (no sudo required)
    pub(crate) const RANCHER_VOLUME_NAME: &'static str = "k3s-rancher-data";

    /// Rancher data directory inside container
    pub(crate) const RANCHER_DATA_PATH: &'static str = "/var/lib/rancher/k3s";

    /// Docker volume name for local PV storage (no sudo required)
    pub(crate) const LOCAL_PV_VOLUME_NAME: &'static str = "k3s-local-pv-data";

    /// PV storage path - points to Docker volume's internal path
    /// This path is accessible to pod containers since they run on the same Docker daemon
    pub(crate) const LOCAL_PV_STORAGE_PATH: &'static str =
        "/var/lib/docker/volumes/k3s-local-pv-data/_data";

    pub async fn new(config: Arc<ClusterConfig>) -> Result<Self> {
        let platform = PlatformInfo::detect()?;
        let socket_path = platform.docker_socket_path().await?;
        let docker = DockerManager::new(socket_path)?;
        let kube_ops = KubeOps::new();

        Ok(Self {
            config,
            docker,
            platform,
            kube_ops,
        })
    }

    /// Get cluster status
    pub async fn get_status(&self) -> ClusterStatus {
        if !self.docker.is_accessible().await {
            return ClusterStatus::RuntimeNotRunning;
        }

        match self
            .docker
            .container_status(&self.config.container_name)
            .await
        {
            Some(status) => match status.as_str() {
                "running" => ClusterStatus::Running,
                "exited" | "dead" => ClusterStatus::Stopped,
                "restarting" => ClusterStatus::Starting,
                "paused" => ClusterStatus::Paused,
                _ => ClusterStatus::Unknown,
            },
            None => ClusterStatus::NotCreated,
        }
    }

    /// Start the k3s cluster (create if not exists)
    pub async fn start(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        tracing::info!(
            container_name = %self.config.container_name,
            k3s_version = %self.config.k3s_version,
            "Starting k3s cluster"
        );

        let _ = output_tx
            .send(OutputLine::info("Starting k3s cluster..."))
            .await;

        // Check Docker accessibility
        if !self.docker.is_accessible().await {
            return Err(anyhow!(
                "Docker is not accessible. Please start Docker first."
            ));
        }

        // Check if container already running
        if self
            .docker
            .container_running(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Cluster is already running"))
                .await;
            return Ok(());
        }

        // Check if container exists but stopped
        if self
            .docker
            .container_exists(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Starting existing cluster container..."))
                .await;
            self.docker
                .start_container(&self.config.container_name)
                .await?;
            self.wait_for_api(&output_tx).await?;

            // Execute on_cluster_available hooks
            if self.config.hooks.has_hooks() {
                let hook_executor = HookExecutor::new(self.config.hooks.clone());
                hook_executor
                    .execute_hooks(HookEvent::OnClusterAvailable, output_tx.clone())
                    .await?;
            }

            return Ok(());
        }

        // Create new cluster - check snapshot first
        if self.config.speedup.use_snapshot {
            let snapshot_image = self.get_snapshot_image_name();

            // Fast path: use snapshot if it exists
            if self.docker.image_exists(&snapshot_image).await {
                let _ = output_tx
                    .send(OutputLine::info("Using snapshot for faster startup..."))
                    .await;
                return self.start_from_snapshot(&snapshot_image, &output_tx).await;
            }

            // Slow path: create cluster and snapshot
            let _ = output_tx
                .send(OutputLine::info(
                    "No snapshot found, creating cluster (this will be faster next time)...",
                ))
                .await;
            self.create_cluster(&output_tx).await?;

            // Create snapshot for next time (warn but don't fail if this fails)
            if let Err(e) = self.create_snapshot(&output_tx).await {
                tracing::warn!(error = %e, "Snapshot creation failed but cluster is running");
            } else {
                // Cleanup old snapshots if enabled
                if self.config.speedup.snapshot_auto_cleanup {
                    if let Err(e) = self.cleanup_old_snapshots(&snapshot_image, &output_tx).await {
                        tracing::warn!(error = %e, "Snapshot cleanup failed");
                    }
                }
            }

            Ok(())
        } else {
            // Snapshots disabled, use normal path
            self.create_cluster(&output_tx).await
        }
    }

    /// Create a new k3s cluster
    async fn create_cluster(&mut self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Creating new k3s cluster..."))
            .await;

        // Run pre-container setup tasks in parallel
        let _ = output_tx
            .send(OutputLine::info("Setting up cluster prerequisites..."))
            .await;
        let image = self.config.k3s_image();
        let image_exists = self.docker.image_exists(&image).await;

        // Create volume, PV directory, network, and pull image in parallel
        let pull_future = async {
            if !image_exists {
                let _ = output_tx
                    .send(OutputLine::info(format!("Pulling k3s image: {}...", image)))
                    .await;
                self.docker.pull_image(&image).await
            } else {
                Ok(())
            }
        };

        tokio::try_join!(
            async {
                let _ = output_tx
                    .send(OutputLine::info(
                        "Creating Docker volume for rancher data...",
                    ))
                    .await;
                self.docker.create_volume(Self::RANCHER_VOLUME_NAME).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Creating Docker volume for PV storage..."))
                    .await;
                self.docker.create_volume(Self::LOCAL_PV_VOLUME_NAME).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Creating Docker network..."))
                    .await;
                self.docker.create_network(&self.config.network_name).await
            },
            pull_future,
        )?;

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

        // K3s server command
        // Note: metrics-server is disabled because we use Docker API for metrics
        // servicelb is disabled as it's rarely needed for local development
        // Traefik is enabled (K3s built-in) and configured via HelmChartConfig CRD
        // Optimized flags to disable unnecessary components for faster startup
        let k3s_command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "mkdir -p /run/k3s /sys/fs/cgroup/kubepods && \
             /bin/k3s server \
             --docker \
             --disable=metrics-server \
             --disable=servicelb \
             --disable-cloud-controller \
             --disable-network-policy \
             --service-node-port-range 80-32767 \
             --kubelet-arg=root-dir=/var/lib/docker/kubelet \
             --kubelet-arg=cgroup-driver=cgroupfs \
             --kube-apiserver-arg=profiling=false \
             --kube-apiserver-arg=enable-admission-plugins=NodeRestriction \
             --kube-controller-manager-arg=concurrent-deployment-syncs=1"
                .to_string(),
        ];

        // Run k3s container
        let _ = output_tx
            .send(OutputLine::info("Starting k3s container..."))
            .await;

        let run_config = ContainerRunConfig {
            name: self.config.container_name.clone(),
            hostname: Some(self.config.container_name.clone()),
            image,
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
                // Mount real /var/lib/docker - required for k3s --docker mode to access host Docker data
                (
                    "/var/lib/docker".to_string(),
                    "/var/lib/docker".to_string(),
                    "bind-propagation=rshared".to_string(),
                ),
                // Docker volume for rancher data (server config, agent data) - no sudo required
                (
                    Self::RANCHER_VOLUME_NAME.to_string(),
                    Self::RANCHER_DATA_PATH.to_string(),
                    "volume".to_string(),
                ),
                // Docker volume for local PV storage - accessible to pod containers via Docker's volume path
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

        self.docker.run_container(&run_config).await?;

        // Wait for k3s API
        self.wait_for_api(output_tx).await?;

        // Install socat and setup kubeconfig in parallel
        let _ = output_tx
            .send(OutputLine::info("Configuring cluster access..."))
            .await;
        let (socat_result, kubeconfig_result) = tokio::join!(
            async {
                let _ = output_tx
                    .send(OutputLine::info("Installing socat in container..."))
                    .await;
                self.install_socat().await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Setting up kubeconfig..."))
                    .await;
                self.setup_kubeconfig().await
            },
        );

        // Report errors with context
        if let Err(e) = &socat_result {
            let _ = output_tx
                .send(OutputLine::error(format!("Socat install failed: {:#}", e)))
                .await;
        }
        if let Err(e) = &kubeconfig_result {
            let _ = output_tx
                .send(OutputLine::error(format!(
                    "Kubeconfig setup failed: {:#}",
                    e
                )))
                .await;
        }
        socat_result?;
        kubeconfig_result?;

        // Wait for cluster to be fully ready
        self.wait_for_cluster_ready(output_tx).await?;

        // Execute on_cluster_available hooks
        if self.config.hooks.has_hooks() {
            let hook_executor = HookExecutor::new(self.config.hooks.clone());
            hook_executor
                .execute_hooks(HookEvent::OnClusterAvailable, output_tx.clone())
                .await?;
        }

        let _ = output_tx
            .send(OutputLine::success("K3s cluster is ready!"))
            .await;

        Ok(())
    }

    /// Stop the k3s cluster
    pub async fn stop(&self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        tracing::info!(
            container_name = %self.config.container_name,
            "Stopping k3s cluster"
        );

        let _ = output_tx
            .send(OutputLine::info("Stopping k3s cluster..."))
            .await;

        if !self
            .docker
            .container_exists(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Cluster is not running"))
                .await;
            return Ok(());
        }

        self.docker
            .stop_container(&self.config.container_name)
            .await?;

        let _ = output_tx
            .send(OutputLine::success("K3s cluster stopped"))
            .await;
        Ok(())
    }

    /// Delete the k3s cluster and cleanup
    pub async fn delete(&self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        tracing::warn!(
            container_name = %self.config.container_name,
            "Deleting k3s cluster and all data"
        );

        let _ = output_tx
            .send(OutputLine::info("Deleting k3s cluster..."))
            .await;

        // Stop and remove k3s container
        if self
            .docker
            .container_exists(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Stopping k3s container..."))
                .await;
            let _ = self
                .docker
                .stop_container(&self.config.container_name)
                .await;
            self.docker
                .remove_container(&self.config.container_name, true)
                .await?;
        }

        // Cleanup k8s_* containers (managed pods) - must complete before network removal
        let _ = output_tx
            .send(OutputLine::info("Cleaning up pod containers..."))
            .await;
        self.docker.cleanup_containers_by_prefix("k8s_").await?;

        // Run remaining cleanup tasks in parallel (all independent after containers are gone)
        let _ = output_tx
            .send(OutputLine::info("Cleaning up cluster resources..."))
            .await;
        let (network_result, rancher_volume_result, pv_volume_result, kubeconfig_result) =
            tokio::join!(
                async {
                    let _ = output_tx
                        .send(OutputLine::info("Removing Docker network..."))
                        .await;
                    self.docker.remove_network(&self.config.network_name).await
                },
                async {
                    let _ = output_tx
                        .send(OutputLine::info("Removing rancher data volume..."))
                        .await;
                    self.docker.remove_volume(Self::RANCHER_VOLUME_NAME).await
                },
                async {
                    let _ = output_tx
                        .send(OutputLine::info("Removing PV storage volume..."))
                        .await;
                    self.docker.remove_volume(Self::LOCAL_PV_VOLUME_NAME).await
                },
                async {
                    let _ = output_tx
                        .send(OutputLine::info("Cleaning kubeconfig..."))
                        .await;
                    self.cleanup_kubeconfig().await
                },
            );
        // Propagate errors from cleanup operations
        network_result?;
        rancher_volume_result?;
        pv_volume_result?;
        kubeconfig_result?;

        let _ = output_tx
            .send(OutputLine::success("K3s cluster deleted"))
            .await;
        Ok(())
    }

    /// Restart the k3s cluster
    #[allow(dead_code)]
    pub async fn restart(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        self.stop(output_tx.clone()).await?;
        sleep(Duration::from_secs(2)).await;
        self.start(output_tx).await
    }

    /// Get cluster info
    pub async fn info(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("=== K3s Cluster Info ==="))
            .await;

        // Cluster status
        let status = self.get_status().await;
        let _ = output_tx
            .send(OutputLine::info(format!("Status: {:?}", status)))
            .await;

        if status != ClusterStatus::Running {
            return Ok(());
        }

        // Kubernetes version
        if let Ok(version) = self.kube_ops.get_version().await {
            let _ = output_tx.send(OutputLine::info(version)).await;
        }

        // Nodes
        let _ = output_tx.send(OutputLine::info("\n=== Nodes ===")).await;
        let _ = output_tx
            .send(OutputLine::info(format!(
                "{:<20} {:<10} {:<15} {:<15} {}",
                "NAME", "STATUS", "ROLES", "INTERNAL-IP", "VERSION"
            )))
            .await;
        if let Ok(nodes) = self.kube_ops.list_nodes().await {
            for node in nodes {
                let _ = output_tx
                    .send(OutputLine::info(node.to_wide_string()))
                    .await;
            }
        }

        // Namespaces
        let _ = output_tx
            .send(OutputLine::info("\n=== Namespaces ==="))
            .await;
        if let Ok(namespaces) = self.kube_ops.list_namespaces().await {
            for ns in namespaces {
                let _ = output_tx.send(OutputLine::info(format!("  {}", ns))).await;
            }
        }

        // System pods
        let _ = output_tx
            .send(OutputLine::info("\n=== System Pods ==="))
            .await;
        let _ = output_tx
            .send(OutputLine::info(format!(
                "{:<50} {:<10} {}",
                "NAME", "READY", "STATUS"
            )))
            .await;
        if let Ok(pods) = self.kube_ops.list_pods("kube-system").await {
            for pod in pods {
                let _ = output_tx.send(OutputLine::info(pod.to_string_line())).await;
            }
        }

        Ok(())
    }

    /// Reset the kube client (call after kubeconfig changes)
    #[allow(dead_code)]
    pub fn reset_kube_client(&mut self) {
        self.kube_ops.reset();
    }
}
