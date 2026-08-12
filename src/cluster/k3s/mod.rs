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

/// Outcome of a cluster start operation
pub enum StartOutcome {
    /// Cluster was already running
    AlreadyRunning,
    /// Existing stopped container was started
    StartedExisting,
    /// Cluster was started from a snapshot
    StartedFromSnapshot,
    /// Fresh cluster was created (no snapshot existed)
    FreshCreated,
}

use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::config::ClusterConfig;
use super::docker::{ContainerRunConfig, DockerManager};
use super::kube_ops::KubeOps;
use super::platform::{docker_host_tcp_url, PlatformInfo};
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

    /// Default PV storage path (used when Docker root is not yet detected)
    pub(crate) const LOCAL_PV_STORAGE_PATH: &'static str =
        "/var/lib/docker/volumes/k3s-local-pv-data/_data";

    /// Get PV storage path based on Docker's actual data root directory
    pub(crate) fn local_pv_storage_path(docker_root: &str) -> String {
        format!("{}/volumes/k3s-local-pv-data/_data", docker_root)
    }

    /// Get kubelet root dir based on Docker's actual data root directory
    pub(crate) fn kubelet_root_dir(docker_root: &str) -> String {
        format!("{}/kubelet", docker_root)
    }

    pub async fn new(config: Arc<ClusterConfig>) -> Result<Self> {
        let platform = PlatformInfo::detect()?;
        let socket_path = platform.docker_socket_path().await?;
        let mut docker = DockerManager::new(socket_path)?;

        // Negotiate API version for compatibility with older Docker versions
        if let Err(e) = docker.negotiate_api_version().await {
            tracing::warn!(
                "Docker API version negotiation failed (using default): {:#}",
                e
            );
        }

        // Warn if Docker daemon architecture differs from binary's compile-time target_arch
        docker.check_architecture_mismatch().await;

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
    pub async fn start(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<StartOutcome> {
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

        // Check Docker cgroup driver compatibility
        self.docker.check_cgroup_driver().await?;

        // Check if container already running
        if self
            .docker
            .container_running(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Cluster is already running"))
                .await;
            return Ok(StartOutcome::AlreadyRunning);
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

            return Ok(StartOutcome::StartedExisting);
        }

        // Create new cluster - check snapshot first
        if self.config.speedup.use_snapshot {
            let snapshot_image = self.get_snapshot_image_name();

            // Fast path: use snapshot if it exists
            if self.docker.image_exists(&snapshot_image).await {
                let is_deep = K3sManager::is_deep_snapshot(&self.docker, &snapshot_image).await;
                let _ = output_tx
                    .send(OutputLine::info("Using snapshot for faster startup..."))
                    .await;
                self.start_from_snapshot(&snapshot_image, is_deep, &output_tx)
                    .await?;
                return Ok(StartOutcome::StartedFromSnapshot);
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
                    if let Err(e) = self
                        .cleanup_old_snapshots(&snapshot_image, &output_tx)
                        .await
                    {
                        tracing::warn!(error = %e, "Snapshot cleanup failed");
                    }
                }
            }

            Ok(StartOutcome::FreshCreated)
        } else {
            // Snapshots disabled, use normal path
            self.create_cluster(&output_tx).await?;
            Ok(StartOutcome::FreshCreated)
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

        // Get docker socket path, docker root, and iptables mode
        let socket_path = self.platform.docker_socket_path().await?;
        let cgroup_driver = "cgroupfs";
        let docker_root = self.docker.get_docker_root_dir().await;
        let pv_storage_path = Self::local_pv_storage_path(&docker_root);
        let kubelet_root = Self::kubelet_root_dir(&docker_root);
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

        // On macOS, publish a port for the Docker API relay (socat) so
        // `k3dev docker` can access the raw Docker daemon from the host.
        // Find an available host port starting from 2375.
        #[cfg(target_os = "macos")]
        {
            let relay_port = crate::cluster::platform::find_available_port(2375).unwrap_or(2375);
            ports.push((relay_port, relay_port));
        }

        // K3s server command
        // Note: metrics-server is disabled because we use Docker API for metrics
        // servicelb is disabled as it's rarely needed for local development
        // Traefik is enabled (K3s built-in) and configured via HelmChartConfig CRD
        // Optimized flags to disable unnecessary components for faster startup
        //
        // On macOS (Docker Desktop), the mounted Docker socket is a proxy that filters
        // container visibility, breaking cri-dockerd. The container runs with --pid=host,
        // so we can access the VM's raw Docker socket at /proc/1/root/run/docker.sock.
        // Use --container-runtime-endpoint to tell cri-dockerd to use the raw socket.
        let docker_endpoint = if cfg!(target_os = "macos") {
            " --container-runtime-endpoint /proc/1/root/run/docker.sock"
        } else {
            ""
        };

        let k3s_command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "nsenter --mount=/proc/1/ns/mnt modprobe br_netfilter 2>/dev/null || true && \
                 sysctl -w net.bridge.bridge-nf-call-iptables=1 2>/dev/null || true && \
                 mkdir -p /run/k3s /sys/fs/cgroup/kubepods && \
                 /bin/k3s server \
                 --docker{docker_endpoint} \
                 --disable=metrics-server \
                 --disable=servicelb \
                 --disable-cloud-controller \
                 --disable-network-policy \
                 --flannel-backend=host-gw \
                 --default-local-storage-path {pv} \
                 --service-node-port-range 80-32767 \
                 --kubelet-arg=root-dir={kubelet} \
                 --kubelet-arg=cgroup-driver={cgroup} \
                 --kube-apiserver-arg=profiling=false \
                 --kube-apiserver-arg=enable-admission-plugins=NodeRestriction \
                 --kube-controller-manager-arg=concurrent-deployment-syncs=1",
                docker_endpoint = docker_endpoint,
                pv = pv_storage_path,
                kubelet = kubelet_root,
                cgroup = cgroup_driver
            ),
        ];

        // Run k3s container
        let _ = output_tx
            .send(OutputLine::info("Starting k3s container..."))
            .await;

        // Build volumes and env - handle TCP Docker (no socket file to mount)
        let tcp_url = docker_host_tcp_url();
        let mut volumes = vec![
            // Mount Docker data directory - required for k3s --docker mode to access host Docker data
            (
                docker_root.clone(),
                docker_root.clone(),
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
                pv_storage_path.clone(),
                "volume".to_string(),
            ),
        ];
        let mut env = vec![
            // Tell K3s to use the same iptables backend as the host
            ("IPTABLES_MODE".to_string(), iptables_mode.to_string()),
        ];

        if let Some(ref url) = tcp_url {
            // TCP Docker (e.g., Colima/OrbStack with tcp://127.0.0.1:2375):
            // No local socket file to mount — pass DOCKER_HOST to the container instead
            tracing::info!(docker_host = %url, "TCP Docker detected, passing DOCKER_HOST to k3s container");
            env.push(("DOCKER_HOST".to_string(), url.clone()));
        } else {
            // Mount the Docker socket for the container
            volumes.insert(
                0,
                (
                    self.platform.docker_socket_mount_source(&socket_path),
                    "/var/run/docker.sock".to_string(),
                    String::new(),
                ),
            );
        }

        let run_config = ContainerRunConfig {
            name: self.config.container_name.clone(),
            hostname: Some(self.config.container_name.clone()),
            image,
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
            labels: Default::default(),
            auto_remove: false,
        };

        self.docker.run_container(&run_config).await?;

        // Wait for k3s API
        self.wait_for_api(output_tx).await?;

        // Install socat and setup kubeconfig in parallel
        let _ = output_tx
            .send(OutputLine::info("Configuring cluster access..."))
            .await;
        let (socat_result, kubeconfig_result, agent_result) = tokio::join!(
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
            async {
                let _ = output_tx
                    .send(OutputLine::info("Installing stats agent..."))
                    .await;
                self.install_agent().await
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
        if let Err(e) = &agent_result {
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Agent install failed (stats will use fallback): {:#}",
                    e
                )))
                .await;
        }
        socat_result?;
        kubeconfig_result?;
        // Agent failure is non-fatal — stats fall back to direct Docker API

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

    /// Delete the k3s cluster and cleanup. With `delete_snapshots` the snapshot
    /// images are removed too — they carry a full copy of the cluster state (k3s
    /// database + local PV data) and are restored on the next start, so keeping
    /// them brings the deleted workloads back.
    pub async fn delete(
        &self,
        output_tx: mpsc::Sender<OutputLine>,
        delete_snapshots: bool,
    ) -> Result<()> {
        tracing::warn!(
            container_name = %self.config.container_name,
            "Deleting k3s cluster and all data"
        );

        let _ = output_tx
            .send(OutputLine::info("Deleting k3s cluster..."))
            .await;

        // Resolve the image for the host cleanup container while the cluster
        // container still exists. Prefer the configured k3s image; a cluster
        // started from a snapshot runs a snapshot image, which is only usable
        // before the snapshots are (optionally) deleted below.
        let cleanup_image = if self.docker.image_exists(&self.config.k3s_image()).await {
            Some(self.config.k3s_image())
        } else {
            self.docker
                .container_image(&self.config.container_name)
                .await
        };

        // Force-remove the k3s container first (skip stop - force remove handles
        // it). Killing kubelet up front means nothing recreates pod containers
        // or re-mounts volumes while the rest of the teardown runs.
        if self
            .docker
            .container_exists(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Removing k3s container..."))
                .await;
            let _ = self
                .docker
                .remove_container(&self.config.container_name, true)
                .await;
        }

        // Pod containers live in the HOST Docker daemon (k3s runs with --docker),
        // so they outlive the k3s container and keep running until removed.
        let _ = output_tx
            .send(OutputLine::info("Removing pod containers..."))
            .await;
        // Best effort: a transient Docker listing failure must not skip the
        // host-state, network, volume and kubeconfig cleanup that follows.
        if let Err(e) = self.docker.cleanup_containers_by_prefix("k8s_").await {
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Could not list pod containers: {}",
                    e
                )))
                .await;
        }

        // Kubelet and local-path mounts made inside the k3s container propagate
        // into the host mount namespace (/var/lib/docker is bind-mounted
        // rshared) and are NOT cleaned up when the container dies. Left behind
        // they pin the k3dev volumes ("device or resource busy"), so the old
        // cluster state survives the destroy and its pods reappear on the next
        // start. Unmounting needs root, hence a throwaway privileged container.
        match cleanup_image {
            Some(image) => {
                let _ = output_tx
                    .send(OutputLine::info("Unmounting cluster volumes..."))
                    .await;
                self.cleanup_host_state(&image).await;
            }
            None => {
                let _ = output_tx
                    .send(OutputLine::warning(
                        "No local k3s image: skipping host mount cleanup",
                    ))
                    .await;
            }
        }

        // Remaining resources are independent of each other:
        // - Network removal will fail if containers still attached, but we retry
        // - Volumes, kubeconfig and snapshot images are independent
        let _ = output_tx
            .send(OutputLine::info("Cleaning up cluster resources..."))
            .await;

        let (
            network_result,
            rancher_volume_result,
            pv_volume_result,
            kubeconfig_result,
            snapshot_result,
        ) = tokio::join!(
            self.docker.remove_network(&self.config.network_name),
            self.docker.remove_volume(Self::RANCHER_VOLUME_NAME),
            self.docker.remove_volume(Self::LOCAL_PV_VOLUME_NAME),
            self.cleanup_kubeconfig(),
            async {
                if delete_snapshots {
                    self.delete_snapshots(&output_tx).await
                } else {
                    Ok(())
                }
            },
        );

        // Propagate errors (most operations ignore errors gracefully)
        network_result?;
        rancher_volume_result?;
        pv_volume_result?;
        kubeconfig_result?;
        snapshot_result?;

        let _ = output_tx
            .send(OutputLine::success("K3s cluster deleted"))
            .await;

        if !delete_snapshots {
            let kept = self
                .docker
                .list_images_by_pattern("k3dev-snapshot-")
                .await
                .unwrap_or_default();
            if !kept.is_empty() {
                let _ = output_tx
                    .send(OutputLine::info(format!(
                        "{} snapshot image(s) kept - the next start restores cluster state from them",
                        kept.len()
                    )))
                    .await;
            }
        }

        Ok(())
    }

    /// Shell script that releases the cluster state left in the host mount
    /// namespace: lazily unmount everything k3s mounted under Docker's data
    /// root, then delete the kubelet state directory (it lives on the host
    /// filesystem, not in a volume, so removing volumes does not clear it).
    ///
    /// Returns `None` unless the data root is a plain absolute path. The root is
    /// interpolated into a script that runs `rm -rf` as root in a privileged
    /// container, so a path holding a space or a quote would delete the wrong
    /// directory - skipping cleanup is the safe answer for those.
    fn host_cleanup_script(docker_root: &str) -> Option<String> {
        let root = docker_root.trim_end_matches('/');
        if !root.starts_with('/')
            || !root
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
        {
            return None;
        }
        let kubelet = Self::kubelet_root_dir(root);
        // `.` is the only regex metacharacter the guard above lets through, and
        // it shows up in a rootless data root (~/.local/share/docker), where an
        // unescaped `.` would widen the pattern to unrelated mounts.
        let root_pattern = root.replace('.', "\\.");
        let kubelet_pattern = kubelet.replace('.', "\\.");
        // Unmount deepest paths first (sort -r) so parent mounts are freed after
        // their children. Covers pod mounts under kubelet/ and the local-path/PV
        // and rancher volume data directories. The kubelet directory is only
        // deleted once nothing is mounted under it, so a failed unmount can
        // never make `rm -rf` delete through a live bind mount.
        Some(format!(
            "awk '{{print $2}}' /proc/mounts | \
             grep -E '^{root}/(kubelet|volumes/({rancher}|{pv}))(/|$)' | \
             sort -r | while IFS= read -r m; do umount -l \"$m\" 2>/dev/null || true; done; \
             awk '{{print $2}}' /proc/mounts | grep -qE '^{kubelet_pattern}(/|$)' || rm -rf \"{kubelet}\"",
            root = root_pattern,
            rancher = Self::RANCHER_VOLUME_NAME,
            pv = Self::LOCAL_PV_VOLUME_NAME,
            kubelet_pattern = kubelet_pattern,
            kubelet = kubelet,
        ))
    }

    /// Run the host cleanup script in a throwaway privileged container. Docker's
    /// data root is bind-mounted rshared, so unmounts inside the container
    /// propagate back to the host and free the k3dev volumes for removal.
    /// Errors are non-fatal (best-effort cleanup).
    async fn cleanup_host_state(&self, image: &str) {
        let docker_root = self.docker.get_docker_root_dir().await;
        let Some(script) = Self::host_cleanup_script(&docker_root) else {
            tracing::warn!(
                docker_root = %docker_root,
                "Docker data root is not a plain absolute path: skipping host mount cleanup"
            );
            return;
        };
        let name = format!("{}-cleanup", self.config.container_name);

        // A leftover helper from an interrupted destroy would block the name.
        let _ = self.docker.remove_container(&name, true).await;

        let run_config = ContainerRunConfig {
            name: name.clone(),
            image: image.to_string(),
            detach: true,
            privileged: true,
            volumes: vec![(
                docker_root.clone(),
                docker_root.clone(),
                "bind-propagation=rshared".to_string(),
            )],
            entrypoint: Some(String::new()),
            command: Some(vec!["/bin/sh".to_string(), "-c".to_string(), script]),
            security_opt: vec!["apparmor=unconfined".to_string()],
            ..Default::default()
        };

        if let Err(e) = self.docker.run_container(&run_config).await {
            tracing::warn!(error = %e, "Failed to start host cleanup container");
            return;
        }

        // The script only unmounts and deletes, so it finishes in well under a
        // second; wait for it so volume removal sees the released mounts.
        for _ in 0..50 {
            if !self.docker.container_running(&name).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // A failed cleanup surfaces later as "device or resource busy" on volume
        // removal, so record the cause here.
        if self.docker.container_running(&name).await {
            tracing::warn!(container = %name, "Host cleanup container did not exit in time");
        } else if let Some(code) = self.docker.container_exit_code(&name).await {
            if code != 0 {
                tracing::warn!(
                    container = %name,
                    exit_code = code,
                    "Host cleanup script failed: cluster volumes may still be mounted"
                );
            }
        }

        let _ = self.docker.remove_container(&name, true).await;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_cleanup_script_targets_cluster_paths_only() {
        let script = K3sManager::host_cleanup_script("/mnt/data/docker").unwrap();

        // Unmounts only the kubelet root and the two k3dev volumes, deepest first
        assert!(script.contains(
            "^/mnt/data/docker/(kubelet|volumes/(k3s-rancher-data|k3s-local-pv-data))(/|$)"
        ));
        assert!(script.contains("sort -r"));
        assert!(script.contains("while IFS= read -r m"));
        assert!(script.contains("umount -l \"$m\""));

        // Kubelet state is only deleted when nothing is mounted under it
        assert!(script.contains(
            "grep -qE '^/mnt/data/docker/kubelet(/|$)' || rm -rf \"/mnt/data/docker/kubelet\""
        ));
    }

    #[test]
    fn host_cleanup_script_normalizes_trailing_slash() {
        let script = K3sManager::host_cleanup_script("/var/lib/docker/").unwrap();

        assert!(!script.contains("//"));
        assert!(script.contains("rm -rf \"/var/lib/docker/kubelet\""));
    }

    #[test]
    fn host_cleanup_script_escapes_dots_in_grep_patterns() {
        let script = K3sManager::host_cleanup_script("/home/dev/.local/share/docker").unwrap();

        // Patterns escape the dot so it cannot match unrelated mount paths...
        assert!(script.contains("^/home/dev/\\.local/share/docker/(kubelet|"));
        assert!(script.contains("grep -qE '^/home/dev/\\.local/share/docker/kubelet(/|$)'"));
        // ...while the rm target stays a literal path
        assert!(script.contains("rm -rf \"/home/dev/.local/share/docker/kubelet\""));
    }

    #[test]
    fn host_cleanup_script_rejects_unsafe_roots() {
        // A space would make the unquoted-word split delete the wrong directory
        assert!(K3sManager::host_cleanup_script("/Users/jane doe/docker").is_none());
        // Shell metacharacters and relative roots never reach the script
        assert!(K3sManager::host_cleanup_script("/var/lib/docker\"; rm -rf /").is_none());
        assert!(K3sManager::host_cleanup_script("/var/lib/$(whoami)").is_none());
        assert!(K3sManager::host_cleanup_script("var/lib/docker").is_none());
        assert!(K3sManager::host_cleanup_script("").is_none());
    }
}
