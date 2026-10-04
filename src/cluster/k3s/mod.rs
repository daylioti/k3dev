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

use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
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
    /// Rancher data directory inside container
    pub(crate) const RANCHER_DATA_PATH: &'static str = "/var/lib/rancher/k3s";

    /// CLI contract version of the embedded k3dev-agent. Bump in lockstep with
    /// `agent/src/main.rs::VERSION` so a stale agent baked into a prebuilt image
    /// is replaced instead of silently reported as present.
    pub(crate) const AGENT_VERSION: &'static str = "2";

    /// CLI contract version of the embedded k3dev-criproxy
    pub(crate) const CRIPROXY_VERSION: &'static str = "1";

    /// Socket cri-dockerd talks to — served by k3dev-criproxy
    pub(crate) const PROXY_DOCKER_SOCK: &'static str = "/var/run/docker.sock";

    /// Where the real host Docker socket is bind-mounted, behind the proxy
    pub(crate) const HOST_DOCKER_SOCK: &'static str = "/var/run/docker-host.sock";

    /// The real Docker socket the proxy forwards to.
    ///
    /// On macOS the mounted Docker Desktop socket is a proxy that filters
    /// container visibility and breaks cri-dockerd; the container runs with
    /// `--pid=host`, so the VM's raw socket is reachable through /proc.
    pub(crate) fn upstream_docker_sock() -> &'static str {
        if cfg!(target_os = "macos") {
            "/proc/1/root/run/docker.sock"
        } else {
            Self::HOST_DOCKER_SOCK
        }
    }

    /// Shell prologue that brings up the CRI filtering proxy before k3s.
    ///
    /// Without it, two clusters sharing the host daemon see each other's pod
    /// containers through cri-dockerd, decide the pod UIDs are unknown, and
    /// garbage-collect each other's running sandboxes in a loop. The proxy
    /// stamps `k3dev.cluster` on every container this cluster creates and
    /// filters container listings by it, so each kubelet only ever sees its own.
    ///
    /// The binary is uploaded right after the container starts, so wait for it;
    /// the restart loop keeps CRI alive if the proxy ever dies.
    pub(crate) fn criproxy_prologue(config: &ClusterConfig) -> String {
        format!(
            "while [ ! -x /usr/local/bin/k3dev-criproxy ]; do sleep 0.2; done; \
             (while true; do /usr/local/bin/k3dev-criproxy \
               --listen {listen} --upstream {upstream} --cluster {cluster}; \
               sleep 1; done) & \
             while [ ! -S {listen} ]; do sleep 0.1; done; ",
            listen = Self::PROXY_DOCKER_SOCK,
            upstream = Self::upstream_docker_sock(),
            cluster = config.cluster_name,
        )
    }

    /// Build the `k3s server` flag list shared by fresh and snapshot startup.
    ///
    /// Every path and CIDR here is namespaced by cluster name or index so two
    /// clusters can run against the same host Docker daemon without corrupting
    /// each other's kubelet state, cgroups or DNS.
    pub(crate) fn k3s_server_args(
        config: &ClusterConfig,
        docker_root: &str,
        use_proxy: bool,
    ) -> String {
        // Point cri-dockerd at the filtering proxy rather than the raw daemon.
        let docker_endpoint = if use_proxy {
            format!(" --container-runtime-endpoint {}", Self::PROXY_DOCKER_SOCK)
        } else {
            String::new()
        };

        format!(
            "/bin/k3s server \
             --docker{docker_endpoint} \
             --node-name={node} \
             --disable=metrics-server \
             --disable=servicelb \
             --disable-cloud-controller \
             --disable-network-policy \
             --flannel-backend=host-gw \
             --cluster-cidr={cluster_cidr} \
             --service-cidr={service_cidr} \
             --cluster-dns={cluster_dns} \
             --default-local-storage-path {pv} \
             --service-node-port-range 80-32767 \
             --kubelet-arg=root-dir={kubelet} \
             --kubelet-arg=cgroup-root={cgroup_root} \
             --kubelet-arg=cgroup-driver=cgroupfs \
             --kubelet-arg=image-gc-high-threshold=100 \
             --kube-apiserver-arg=profiling=false \
             --kube-apiserver-arg=enable-admission-plugins=NodeRestriction \
             --kube-controller-manager-arg=concurrent-deployment-syncs=1",
            docker_endpoint = docker_endpoint,
            node = config.node_name(),
            cluster_cidr = config.cluster_cidr(),
            service_cidr = config.service_cidr(),
            cluster_dns = config.cluster_dns(),
            pv = config.local_pv_storage_path(docker_root),
            kubelet = config.kubelet_root_dir(docker_root),
            cgroup_root = config.cgroup_root(),
        )
    }

    /// Docker labels stamped on this cluster's server container and images.
    ///
    /// `k3dev.role` is what separates the server from the pod containers the CRI
    /// proxy stamps with the same `k3dev.cluster` value: those outlive a stopped
    /// cluster, so "is this cluster up?" has to key on the role.
    pub(crate) fn cluster_labels(
        config: &ClusterConfig,
    ) -> std::collections::HashMap<String, String> {
        let mut labels = std::collections::HashMap::new();
        labels.insert("k3dev.cluster".to_string(), config.cluster_name.clone());
        labels.insert("k3dev.role".to_string(), "server".to_string());
        labels
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

        let kube_ops = KubeOps::for_cluster(&config);

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
            // Pod containers left over from a stop that did not sweep them - an
            // older build, a plain `docker stop`, a host reboot - still hold a
            // sandbox netns whose veth peer died with the previous server
            // container. Remove them so kubelet builds fresh ones.
            self.sweep_pod_containers(&output_tx).await;
            self.docker
                .start_container(&self.config.container_name)
                .await?;
            self.wait_for_api(&output_tx).await?;

            // Refresh kubeconfig before hooks run: a container created by an
            // older version has no pinned kubeconfig yet, and the merged entry
            // can be stale if the API port changed while it was stopped.
            self.setup_kubeconfig().await?;

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

            return Ok(StartOutcome::StartedExisting);
        }

        // Create new cluster - check snapshot first
        if self.config.speedup.use_snapshot {
            let docker_root = self.docker.get_docker_root_dir().await;
            let snapshot_image = self.get_snapshot_image_name(&docker_root);

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
                // Execute on_snapshot_created hooks (non-fatal, cluster is already up)
                if self.config.hooks.has_hooks() {
                    let hook_executor = HookExecutor::new(
                        self.config.hooks.clone(),
                        self.config.pinned_kubeconfig(),
                        self.config.context_name(),
                    );
                    if let Err(e) = hook_executor
                        .execute_hooks(HookEvent::OnSnapshotCreated, output_tx.clone())
                        .await
                    {
                        let _ = output_tx
                            .send(OutputLine::error(format!("Hook execution failed: {}", e)))
                            .await;
                    }
                }

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
        let rancher_volume = self.config.rancher_volume_name();
        let pv_volume = self.config.local_pv_volume_name();

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
                self.docker.create_volume(&rancher_volume).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Creating Docker volume for PV storage..."))
                    .await;
                self.docker.create_volume(&pv_volume).await
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
        // The proxy speaks unix sockets only; a TCP DOCKER_HOST bypasses it,
        // which means that setup supports one cluster at a time.
        let use_proxy = docker_host_tcp_url().is_none();
        if !use_proxy {
            tracing::warn!(
                "TCP Docker endpoint: CRI filtering proxy disabled, \
                 running more than one cluster concurrently is unsafe"
            );
        }

        let k3s_command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "nsenter --mount=/proc/1/ns/mnt modprobe br_netfilter 2>/dev/null || true && \
                 sysctl -w net.bridge.bridge-nf-call-iptables=1 2>/dev/null || true && \
                 mkdir -p /run/k3s {cgroup_path} && \
                 {proxy}{server}",
                cgroup_path = self.config.cgroup_root_path(),
                proxy = if use_proxy {
                    Self::criproxy_prologue(&self.config)
                } else {
                    String::new()
                },
                server = Self::k3s_server_args(&self.config, &docker_root, use_proxy),
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
                self.config.rancher_volume_name(),
                Self::RANCHER_DATA_PATH.to_string(),
                "volume".to_string(),
            ),
            // Docker volume for local PV storage - accessible to pod containers via Docker's volume path
            (
                self.config.local_pv_volume_name(),
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
            // Mounted behind the proxy: cri-dockerd gets the filtered socket at
            // /var/run/docker.sock, the proxy forwards here.
            volumes.insert(
                0,
                (
                    self.platform.docker_socket_mount_source(&socket_path),
                    Self::HOST_DOCKER_SOCK.to_string(),
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
            labels: Self::cluster_labels(&self.config),
            auto_remove: false,
        };

        self.docker.run_container(&run_config).await?;

        // The container's entrypoint blocks until this lands, so it has to be
        // uploaded before anything waits on the API.
        if use_proxy {
            let _ = output_tx
                .send(OutputLine::info("Installing CRI filtering proxy..."))
                .await;
            self.install_criproxy()
                .await
                .context("Failed to install CRI filtering proxy")?;
        }

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

        self.sweep_pod_containers(&output_tx).await;

        let _ = output_tx
            .send(OutputLine::success("K3s cluster stopped"))
            .await;
        Ok(())
    }

    /// Remove this cluster's pod containers.
    ///
    /// The CNI bridge and every veth peer live in the server container's
    /// network namespace, which is destroyed and rebuilt whenever that
    /// container stops and starts. A pod container that survives keeps a
    /// sandbox namespace holding nothing but `lo`, and kubelet only rebuilds
    /// the sandboxes whose probes fail - a pod without a probe stays Ready
    /// with no network at all. Pod containers therefore cannot outlive the
    /// namespace they were wired into.
    ///
    /// Best-effort: a container that fails to go is logged, not fatal, because
    /// the cluster still has to stop.
    async fn sweep_pod_containers(&self, output_tx: &mpsc::Sender<OutputLine>) {
        let names = self
            .docker
            .list_cluster_pod_containers(&self.config.cluster_name)
            .await;

        if names.is_empty() {
            return;
        }

        let _ = output_tx
            .send(OutputLine::info(format!(
                "Removing {} pod containers (recreated on start)...",
                names.len()
            )))
            .await;

        self.docker.remove_containers(&names).await;
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

        // Ask kubelet which pods are ours while it can still answer: its state
        // directory names one directory per pod UID, including pods that never
        // got past the sandbox and so own no container mounts to be found by
        // `owned_pod_containers` later.
        let pod_uids = self.kubelet_pod_uids().await;

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
        match self.owned_pod_containers(&pod_uids).await {
            Ok((mine, foreign)) => {
                self.docker.remove_containers(&mine).await;
                if foreign > 0 {
                    let _ = output_tx
                        .send(OutputLine::info(format!(
                            "Left {} pod container(s) of other clusters untouched",
                            foreign
                        )))
                        .await;
                }
            }
            Err(e) => {
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "Could not list pod containers: {}",
                        e
                    )))
                    .await;
            }
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

        let rancher_volume = self.config.rancher_volume_name();
        let pv_volume = self.config.local_pv_volume_name();

        let (
            network_result,
            rancher_volume_result,
            pv_volume_result,
            kubeconfig_result,
            snapshot_result,
        ) = tokio::join!(
            self.docker.remove_network(&self.config.network_name),
            self.docker.remove_volume(&rancher_volume),
            self.docker.remove_volume(&pv_volume),
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

        crate::cluster::registry::release_index(&self.config.cluster_name);

        let _ = output_tx
            .send(OutputLine::success("K3s cluster deleted"))
            .await;

        if !delete_snapshots {
            let kept = self
                .docker
                .list_images_by_pattern(&self.config.snapshot_prefix(), &self.config.cluster_name)
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

    /// Pod UID from a kubelet container name, which Docker formats as
    /// `k8s_{container}_{pod}_{namespace}_{uid}_{attempt}` (pod, namespace and
    /// container names are DNS-1123, so they never contain an underscore).
    fn pod_uid(container_name: &str) -> Option<&str> {
        container_name
            .strip_prefix("k8s_")
            .and_then(|rest| rest.split('_').nth(3))
    }

    /// Pod UIDs kubelet currently tracks, read from the one-directory-per-pod
    /// layout of its state dir. Empty when the cluster is not running - the
    /// state dir lives under Docker's root and is only readable as root.
    async fn kubelet_pod_uids(&self) -> Vec<String> {
        if !self
            .docker
            .container_running(&self.config.container_name)
            .await
        {
            return Vec::new();
        }
        let docker_root = self.docker.get_docker_root_dir().await;
        let pods_dir = format!("{}/pods", self.config.kubelet_root_dir(&docker_root));
        // Destroy exists to clean up broken clusters, so a container that is
        // running but wedged must not stall it: give up and fall back to the
        // mounts of the pod containers themselves.
        let exec = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.docker
                .exec_in_container(&self.config.container_name, &["ls", "-1", &pods_dir]),
        )
        .await;
        match exec {
            // Any non-UID noise is harmless: a UID only matters when it also
            // appears in a container name.
            Ok(Ok(out)) => out.lines().map(|l| l.trim().to_string()).collect(),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Could not list kubelet pod state");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!("Timed out listing kubelet pod state");
                Vec::new()
            }
        }
    }

    /// Splits the host daemon's `k8s_*` containers into the ones this cluster
    /// created and a count of the ones it did not, so `delete` never touches
    /// another cluster's workloads.
    ///
    /// Every kubelet on a shared Docker daemon names its containers `k8s_*`, so
    /// the prefix identifies a pod container but not its owner. Ownership comes
    /// from three places. The CRI proxy labels every container it creates for
    /// this cluster, which holds even once the cluster is stopped. Kubelet runs
    /// with a per-cluster `root-dir`, so pod state mounted from that path is
    /// ours, and `kubelet_pod_uids` covers the pods that have no such mount yet.
    /// A UID then claims the whole pod, including its sandbox (`k8s_POD_*`),
    /// which carries no mounts of its own.
    async fn owned_pod_containers(
        &self,
        kubelet_pod_uids: &[String],
    ) -> Result<(Vec<String>, usize)> {
        let docker_root = self.docker.get_docker_root_dir().await;
        let pods_dir = format!("{}/pods/", self.config.kubelet_root_dir(&docker_root));
        let containers = self.docker.list_containers_with_mounts("k8s_").await?;
        let labelled: HashSet<String> = self
            .docker
            .list_cluster_pod_containers(&self.config.cluster_name)
            .await
            .into_iter()
            .collect();

        let mut owned: HashSet<&str> = kubelet_pod_uids.iter().map(String::as_str).collect();
        owned.extend(
            containers
                .iter()
                .flat_map(|c| &c.mounts)
                .filter_map(|m| m.source.strip_prefix(pods_dir.as_str()))
                .filter_map(|pod_path| pod_path.split('/').next()),
        );

        // Docker's name filter matches a substring, so drop what only happens to
        // contain `k8s_` before splitting the rest by owner.
        let (mine, foreign): (Vec<_>, Vec<_>) = containers
            .iter()
            .filter(|c| c.container_name.starts_with("k8s_"))
            .partition(|c| {
                labelled.contains(&c.container_name)
                    || Self::pod_uid(&c.container_name).is_some_and(|uid| owned.contains(uid))
            });

        Ok((
            mine.into_iter().map(|c| c.container_name.clone()).collect(),
            foreign.len(),
        ))
    }

    /// Shell script that releases the cluster state left in the host mount
    /// namespace: lazily unmount everything this cluster's k3s mounted under
    /// Docker's data root, then delete its kubelet state directory (it lives on
    /// the host filesystem, not in a volume, so removing volumes does not clear
    /// it).
    ///
    /// Returns `None` unless the data root is a plain absolute path and the
    /// cluster name a plain path component. Both are interpolated into a script
    /// that runs `rm -rf` as root in a privileged container, so a space, a
    /// quote or a `/` would delete the wrong directory - skipping cleanup is
    /// the safe answer for those.
    fn host_cleanup_script(config: &ClusterConfig, docker_root: &str) -> Option<String> {
        let plain = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
        };
        let root = docker_root.trim_end_matches('/');
        let name = &config.cluster_name;
        if !root.starts_with('/')
            || !plain(root)
            || name.is_empty()
            || name.contains('/')
            || !plain(name)
        {
            return None;
        }
        let kubelet = config.kubelet_root_dir(root);
        // `.` is the only regex metacharacter the guard above lets through, and
        // it shows up in a rootless data root (~/.local/share/docker) or a
        // dotted cluster name, where an unescaped `.` would widen the pattern
        // to unrelated mounts.
        let escape = |s: &str| s.replace('.', "\\.");
        // Unmount deepest paths first (sort -r) so parent mounts are freed after
        // their children. Covers pod mounts under the kubelet root and the
        // local-path/PV and rancher volume data directories. The kubelet
        // directory is only deleted once nothing is mounted under it, so a
        // failed unmount can never make `rm -rf` delete through a live bind mount.
        Some(format!(
            "awk '{{print $2}}' /proc/mounts | \
             grep -E '^({kubelet_pattern}|{root}/volumes/({rancher}|{pv}))(/|$)' | \
             sort -r | while IFS= read -r m; do umount -l \"$m\" 2>/dev/null || true; done; \
             awk '{{print $2}}' /proc/mounts | grep -qE '^{kubelet_pattern}(/|$)' || rm -rf \"{kubelet}\"",
            kubelet_pattern = escape(&kubelet),
            root = escape(root),
            rancher = escape(&config.rancher_volume_name()),
            pv = escape(&config.local_pv_volume_name()),
            kubelet = kubelet,
        ))
    }

    /// Run the host cleanup script in a throwaway privileged container. Docker's
    /// data root is bind-mounted rshared, so unmounts inside the container
    /// propagate back to the host and free the k3dev volumes for removal.
    /// Errors are non-fatal (best-effort cleanup).
    async fn cleanup_host_state(&self, image: &str) {
        let docker_root = self.docker.get_docker_root_dir().await;
        let Some(script) = Self::host_cleanup_script(&self.config, &docker_root) else {
            tracing::warn!(
                docker_root = %docker_root,
                cluster = %self.config.cluster_name,
                "Docker data root or cluster name is not a plain path: skipping host mount cleanup"
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

    fn named(cluster: &str) -> ClusterConfig {
        ClusterConfig {
            cluster_name: cluster.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn pod_uid_reads_the_uid_field_of_kubelet_container_names() {
        let uid = "7117180a-b65d-4815-adaa-bdc9a993c36a";

        assert_eq!(
            K3sManager::pod_uid(&format!(
                "k8s_coredns_coredns-54996dc9b4-94mw9_kube-system_{uid}_1"
            )),
            Some(uid)
        );
        // The sandbox of the same pod resolves to the same UID
        assert_eq!(
            K3sManager::pod_uid(&format!(
                "k8s_POD_coredns-54996dc9b4-94mw9_kube-system_{uid}_1"
            )),
            Some(uid)
        );
        // Containers that merely contain `k8s_` are not pod containers
        assert_eq!(K3sManager::pod_uid("my_k8s_thing"), None);
        assert_eq!(K3sManager::pod_uid("k8s_too_short"), None);
    }

    #[test]
    fn host_cleanup_script_targets_cluster_paths_only() {
        let script = K3sManager::host_cleanup_script(&named("alpha"), "/mnt/data/docker").unwrap();

        // Unmounts only this cluster's kubelet root and volumes, deepest first
        assert!(script.contains(
            "^(/mnt/data/docker/kubelet-alpha|/mnt/data/docker/volumes/(alpha-rancher-data|alpha-local-pv-data))(/|$)"
        ));
        assert!(script.contains("sort -r"));
        assert!(script.contains("while IFS= read -r m"));
        assert!(script.contains("umount -l \"$m\""));

        // Kubelet state is only deleted when nothing is mounted under it
        assert!(script.contains(
            "grep -qE '^/mnt/data/docker/kubelet-alpha(/|$)' || rm -rf \"/mnt/data/docker/kubelet-alpha\""
        ));
    }

    #[test]
    fn host_cleanup_script_normalizes_trailing_slash() {
        let script = K3sManager::host_cleanup_script(&named("alpha"), "/var/lib/docker/").unwrap();

        assert!(!script.contains("//"));
        assert!(script.contains("rm -rf \"/var/lib/docker/kubelet-alpha\""));
    }

    #[test]
    fn host_cleanup_script_escapes_dots_in_grep_patterns() {
        let script =
            K3sManager::host_cleanup_script(&named("my.app"), "/home/dev/.local/share/docker")
                .unwrap();

        // Patterns escape the dot so it cannot match unrelated mount paths...
        assert!(script.contains("^(/home/dev/\\.local/share/docker/kubelet-my\\.app|"));
        assert!(script.contains("volumes/(my\\.app-rancher-data|my\\.app-local-pv-data)"));
        assert!(
            script.contains("grep -qE '^/home/dev/\\.local/share/docker/kubelet-my\\.app(/|$)'")
        );
        // ...while the rm target stays a literal path
        assert!(script.contains("rm -rf \"/home/dev/.local/share/docker/kubelet-my.app\""));
    }

    #[test]
    fn host_cleanup_script_rejects_unsafe_roots() {
        let config = named("alpha");
        // A space would make the unquoted-word split delete the wrong directory
        assert!(K3sManager::host_cleanup_script(&config, "/Users/jane doe/docker").is_none());
        // Shell metacharacters and relative roots never reach the script
        assert!(K3sManager::host_cleanup_script(&config, "/var/lib/docker\"; rm -rf /").is_none());
        assert!(K3sManager::host_cleanup_script(&config, "/var/lib/$(whoami)").is_none());
        assert!(K3sManager::host_cleanup_script(&config, "var/lib/docker").is_none());
        assert!(K3sManager::host_cleanup_script(&config, "").is_none());
    }

    #[test]
    fn host_cleanup_script_rejects_unsafe_cluster_names() {
        // The name becomes part of the `rm -rf` target, so a slash could walk
        // out of the data root and anything else could break the quoting
        for name in ["", "a/../../etc", "a b", "a\"b", "$(whoami)"] {
            assert!(
                K3sManager::host_cleanup_script(&named(name), "/var/lib/docker").is_none(),
                "accepted cluster name {:?}",
                name
            );
        }
    }

    #[test]
    fn cluster_labels_mark_the_server_role() {
        let config = named("alpha");
        let labels = K3sManager::cluster_labels(&config);

        assert_eq!(
            labels.get("k3dev.cluster").map(String::as_str),
            Some("alpha")
        );
        // The CRI proxy stamps `k3dev.cluster` on pod containers too, and those
        // outlive a stopped cluster; the role is what identifies the server.
        assert_eq!(labels.get("k3dev.role").map(String::as_str), Some("server"));
    }

    #[test]
    fn criproxy_prologue_waits_for_binary_and_socket() {
        let config = named("alpha");
        let prologue = K3sManager::criproxy_prologue(&config);

        assert!(prologue.contains("--cluster alpha"));
        // k3s must not come up before the filtered socket exists, or its
        // cri-dockerd connects straight to the unfiltered daemon.
        assert!(prologue.contains(&format!("while [ ! -S {} ]", K3sManager::PROXY_DOCKER_SOCK)));
    }
}
