//! Docker container and network management
//!
//! This module provides Docker operations for k3dev:
//! - Container lifecycle (create, start, stop, remove)
//! - Network and volume management
//! - Image operations (pull, commit, remove)
//! - Command execution in containers

#![allow(deprecated)]

pub(crate) mod pull_progress;
mod stats;
mod volumes;

pub use pull_progress::{ContainerPullProgress, PullPhase};
pub use stats::ContainerStats;

use anyhow::{anyhow, Context, Result};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerConfig, ContainerCreateBody, ContainerSummary, HostConfig, HostConfigCgroupnsModeEnum,
    Mount, MountBindOptions, MountBindOptionsPropagationEnum, MountTypeEnum, NetworkConnectRequest,
    NetworkCreateRequest, NetworkDisconnectRequest, PortBinding, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CommitContainerOptions, CreateContainerOptions, CreateImageOptions, InspectContainerOptions,
    InspectNetworkOptions, ListContainersOptions, ListImagesOptions, RemoveContainerOptions,
    RemoveImageOptions, RemoveVolumeOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::ClientVersion;
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Docker container and network management
pub struct DockerManager {
    socket_path: PathBuf,
    pub(crate) client: Docker,
}

impl DockerManager {
    pub fn new(socket_path: PathBuf) -> Result<Self> {
        let client = Self::connect(&socket_path)?;

        Ok(Self {
            socket_path,
            client,
        })
    }

    /// Build a DockerManager using the auto-detected host socket path.
    pub fn from_default_socket() -> Result<Self> {
        Self::new(crate::cluster::PlatformInfo::find_docker_socket_sync())
    }

    /// Connect to Docker using the resolved socket path.
    /// Falls back to DOCKER_HOST / default if the path doesn't exist (TCP/remote).
    fn connect(socket_path: &Path) -> Result<Docker> {
        // If the socket file exists on disk, connect directly to it.
        // This handles Docker Desktop on macOS (~/.docker/run/docker.sock)
        // and other non-default socket locations.
        if socket_path.exists() {
            let uri = format!("unix://{}", socket_path.display());
            return Docker::connect_with_socket(&uri, 120, bollard::API_DEFAULT_VERSION)
                .with_context(|| {
                    format!(
                        "Failed to connect to Docker socket at {}",
                        socket_path.display()
                    )
                });
        }

        // For TCP/remote Docker or when socket path is a placeholder,
        // fall back to DOCKER_HOST env var or bollard defaults.
        Docker::connect_with_defaults()
            .context("Failed to connect to Docker. Check DOCKER_HOST or that Docker is running.")
    }

    /// Negotiate Docker API version with the server.
    /// This ensures compatibility with older Docker versions that reject newer API requests.
    /// Should be called once after construction when async context is available.
    pub async fn negotiate_api_version(&mut self) -> Result<()> {
        let old_client = std::mem::replace(
            &mut self.client,
            Self::connect(&self.socket_path)
                .context("Failed to reconnect to Docker for version negotiation")?,
        );
        self.client = old_client
            .negotiate_version()
            .await
            .context("Failed to negotiate Docker API version")?;
        Ok(())
    }

    /// Check if the Docker daemon's architecture matches the k3dev binary's compile-time target_arch.
    /// Logs a warning if there's a mismatch (e.g., k3dev compiled for aarch64 but Docker reports x86_64
    /// under Rosetta, or vice versa). Does not block startup.
    pub async fn check_architecture_mismatch(&self) {
        let docker_arch = match self.client.info().await {
            Ok(info) => match info.architecture {
                Some(arch) => arch,
                None => return, // Can't detect, skip
            },
            Err(_) => return, // Can't query, skip
        };

        let binary_arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            return; // Unknown arch, skip
        };

        // Normalize Docker's reported architecture for comparison
        let docker_arch_normalized = match docker_arch.as_str() {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            _ => docker_arch.as_str(),
        };

        if binary_arch != docker_arch_normalized {
            tracing::warn!(
                binary_arch = binary_arch,
                docker_arch = docker_arch.as_str(),
                "Architecture mismatch: k3dev binary is compiled for {} but Docker daemon reports {}. \
                 Embedded binaries (socat, agent) may not work in the container. \
                 Consider using a native {} build of k3dev.",
                binary_arch,
                docker_arch,
                docker_arch_normalized,
            );
        }
    }

    /// Get Docker's root directory (data-root)
    /// Returns the actual Docker data directory (e.g., "/var/lib/docker", "~/.local/share/docker/")
    /// Falls back to "/var/lib/docker" if detection fails
    pub async fn get_docker_root_dir(&self) -> String {
        match self.client.info().await {
            Ok(info) => info
                .docker_root_dir
                .unwrap_or_else(|| "/var/lib/docker".to_string()),
            Err(_) => "/var/lib/docker".to_string(),
        }
    }

    /// Check if Docker is accessible.
    /// Retries briefly to handle systemd socket activation delays.
    pub async fn is_accessible(&self) -> bool {
        if self.client.ping().await.is_ok() {
            return true;
        }

        // Socket activation: daemon may be starting. Retry with short delays.
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if self.client.ping().await.is_ok() {
                return true;
            }
        }

        false
    }

    /// Check that Docker's cgroup driver is cgroupfs (required for k3s-in-docker).
    /// k3s runs inside a container without systemd, so kubelet cannot use the
    /// systemd cgroup manager. Docker and kubelet must use the same driver.
    pub async fn check_cgroup_driver(&self) -> Result<()> {
        let driver = match self.client.info().await {
            Ok(info) => info
                .cgroup_driver
                .map(|d| d.to_string())
                .unwrap_or_else(|| "cgroupfs".to_string()),
            Err(_) => return Ok(()), // Can't detect, don't block
        };

        if driver == "systemd" {
            anyhow::bail!(
                "Docker is using the 'systemd' cgroup driver, which is incompatible with k3dev.\n\n\
                 k3s runs inside a Docker container without systemd as init, so kubelet\n\
                 cannot use the systemd cgroup manager. Both Docker and kubelet must use\n\
                 the 'cgroupfs' driver.\n\n\
                 To fix, create or edit /etc/docker/daemon.json:\n\n\
                   {{\n\
                     \"exec-opts\": [\"native.cgroupdriver=cgroupfs\"]\n\
                   }}\n\n\
                 Then restart Docker:\n\n\
                   sudo systemctl restart docker\n\n\
                 Note: This only affects cgroup accounting, not Docker functionality."
            );
        }
        Ok(())
    }

    // === Container Operations ===

    /// Check if a container exists
    pub async fn container_exists(&self, name: &str) -> bool {
        self.client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .is_ok()
    }

    /// Check if a container is running
    pub async fn container_running(&self, name: &str) -> bool {
        match self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => info.state.and_then(|s| s.running).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Host port a container publishes for `container_port`, if any.
    /// Reads the create-time binding, so it also answers for a stopped container.
    pub async fn published_host_port(&self, name: &str, container_port: u16) -> Option<u16> {
        let info = self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()?;

        info.host_config?
            .port_bindings?
            .get(&format!("{}/tcp", container_port))?
            .as_ref()?
            .first()?
            .host_port
            .as_ref()?
            .parse()
            .ok()
    }

    /// Get container status
    pub async fn container_status(&self, name: &str) -> Option<String> {
        self.client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()
            .and_then(|info| info.state)
            .and_then(|state| state.status)
            .map(|s| s.to_string())
    }

    /// Exit code of a stopped container (`None` while it runs or when unknown)
    pub async fn container_exit_code(&self, name: &str) -> Option<i64> {
        self.client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()
            .and_then(|info| info.state)
            .and_then(|state| state.exit_code)
    }

    /// Start a stopped container
    ///
    /// Docker publishes host ports at start, so this is where a port conflict
    /// surfaces; it comes back as a [`PortBindError`].
    pub async fn start_container(&self, name: &str) -> Result<()> {
        match self
            .client
            .start_container(name, None::<StartContainerOptions>)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => Err(self.start_error(name, e).await),
        }
    }

    /// Explain a failed start, naming the port and its holder on a port conflict
    async fn start_error(&self, name: &str, err: bollard::errors::Error) -> anyhow::Error {
        // Docker Desktop ends its message with a newline
        let docker_error = err.to_string().trim_end().to_string();
        let Some(failure) = parse_bind_failure(&docker_error) else {
            return anyhow!(err).context(format!("Failed to start container {}", name));
        };

        // A container that publishes the port twice collides with itself, and
        // the full list is all there is to go on when Docker names no port.
        let own = self.published_host_ports(name).await;
        let reused = failure
            .host_port
            .is_some_and(|port| own.iter().filter(|p| **p == port).count() > 1);
        let holder = match failure.host_port {
            Some(port) if !reused => self.container_publishing(port, name).await,
            _ => None,
        };
        let mut published = own;
        published.dedup();

        anyhow::Error::new(PortBindError {
            container: name.to_string(),
            host_port: failure.host_port,
            denied: failure.denied,
            reused,
            holder,
            published,
            settings: Vec::new(),
            macos: cfg!(target_os = "macos"),
            docker_error,
        })
    }

    /// Running container, other than `except`, that publishes host port `port`
    async fn container_publishing(&self, port: u16, except: &str) -> Option<String> {
        let containers = self
            .client
            .list_containers(Some(ListContainersOptions::default()))
            .await
            .ok()?;
        publisher_of(&containers, port, except)
    }

    /// Host ports a container asks Docker to publish, sorted, one per binding
    async fn published_host_ports(&self, name: &str) -> Vec<u16> {
        let bindings = self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()
            .and_then(|info| info.host_config)
            .and_then(|host| host.port_bindings)
            .unwrap_or_default();
        let mut ports: Vec<u16> = bindings
            .into_values()
            .flatten()
            .flatten()
            .filter_map(|b| b.host_port?.parse().ok())
            .filter(|port| *port != 0)
            .collect();
        ports.sort_unstable();
        ports
    }

    /// Stop a running container
    pub async fn stop_container(&self, name: &str) -> Result<()> {
        self.client
            .stop_container(
                name,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: None,
                }),
            )
            .await
            .with_context(|| format!("Failed to stop container {}", name))
    }

    /// Remove a container together with its anonymous volumes.
    ///
    /// All persistent k3dev state lives in *named* volumes, so the anonymous
    /// volumes Docker creates for the k3s image's `VOLUME` directories
    /// (`/var/lib/cni`, `/var/lib/kubelet`, `/var/log`) are throwaway. Without
    /// `v` they survive every container removal and pile up as dangling volumes.
    pub async fn remove_container(&self, name: &str, force: bool) -> Result<()> {
        self.client
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force,
                    v: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("Failed to remove container {}", name))
    }

    /// Get the image a container was created from
    pub async fn container_image(&self, name: &str) -> Option<String> {
        self.client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()
            .and_then(|info| info.config)
            .and_then(|config| config.image)
    }

    /// Send a signal to a running container.
    /// `signal` is a name like "SIGTERM" / "SIGKILL".
    pub async fn kill_container(&self, name: &str, signal: &str) -> Result<()> {
        use bollard::query_parameters::KillContainerOptions;
        self.client
            .kill_container(
                name,
                Some(KillContainerOptions {
                    signal: signal.to_string(),
                }),
            )
            .await
            .with_context(|| format!("Failed to send {} to container {}", signal, name))
    }

    /// Attach to a running container's multiplexed stdout/stderr stream.
    /// Returns the bollard `AttachContainerResults` whose `output` is a
    /// `Stream<LogOutput>` of stdout/stderr chunks. `input` is unused for
    /// capture but exposed for completeness.
    pub async fn attach_container_stream(
        &self,
        name: &str,
    ) -> Result<bollard::container::AttachContainerResults> {
        use bollard::query_parameters::AttachContainerOptions;
        self.client
            .attach_container(
                name,
                Some(AttachContainerOptions {
                    stream: true,
                    stdout: true,
                    stderr: true,
                    logs: false,
                    stdin: false,
                    detach_keys: None,
                }),
            )
            .await
            .with_context(|| format!("Failed to attach to container {}", name))
    }

    /// Inspect a container and return its `network_mode` string (e.g. "host",
    /// "bridge", "container:<id>"). Returns `None` if not set.
    pub async fn inspect_network_mode(&self, name: &str) -> Result<Option<String>> {
        let info = self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("Failed to inspect container {}", name))?;
        Ok(info.host_config.and_then(|h| h.network_mode))
    }

    /// Find the pause container for a pod managed by k3s `--docker`.
    ///
    /// k3s names pause containers as `k8s_POD_<pod>_<namespace>_<uid>_<attempt>`;
    /// the pause container owns the pod's network namespace and outlives any
    /// individual workload container, making it the canonical target for
    /// network-namespace-shared sidecars (e.g. tcpdump capture).
    pub async fn find_pod_pause_container(
        &self,
        pod: &str,
        namespace: &str,
        cgroup_root: &str,
    ) -> Result<String> {
        let prefix = format!("k8s_POD_{}_{}_", pod, namespace);
        let names = self.list_containers_by_prefix(&prefix).await?;

        // Pod names collide freely across clusters (`kube-system/coredns-…`
        // exists in every one), and Docker labels carry no cluster identity.
        // The cgroup parent kubelet assigns does, so filter on it.
        for name in names.into_iter().filter(|n| n.starts_with(&prefix)) {
            if self.container_in_cgroup_root(&name, cgroup_root).await {
                return Ok(name);
            }
        }

        Err(anyhow!(
            "No pause container found for pod {}/{} in cluster cgroup {}",
            namespace,
            pod,
            cgroup_root
        ))
    }

    /// Does this container's kubelet-assigned cgroup parent sit under `cgroup_root`?
    /// This is the only cluster discriminator available on a shared daemon.
    async fn container_in_cgroup_root(&self, name: &str, cgroup_root: &str) -> bool {
        let root = format!("/{}", cgroup_root.trim_start_matches('/'));
        match self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => info
                .host_config
                .and_then(|h| h.cgroup_parent)
                .is_some_and(|p| {
                    let p = format!("/{}", p.trim_start_matches('/'));
                    p == root || p.starts_with(&format!("{}/", root))
                }),
            Err(e) => {
                tracing::debug!(container = %name, error = %e, "cgroup parent lookup failed");
                false
            }
        }
    }

    /// Execute a command in a running container
    pub async fn exec_in_container(&self, container: &str, command: &[&str]) -> Result<String> {
        let exec = self
            .client
            .create_exec(
                container,
                CreateExecOptions {
                    cmd: Some(command.to_vec()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create exec")?;

        let exec_id = exec.id.clone();

        let output = self
            .client
            .start_exec(&exec_id, Some(StartExecOptions::default()))
            .await
            .context("Failed to start exec")?;

        let mut result = String::new();
        if let StartExecResults::Attached { mut output, .. } = output {
            while let Some(msg) = output.next().await {
                if let Ok(msg) = msg {
                    result.push_str(&msg.to_string());
                }
            }
        }

        // Check exit code
        let inspect = self.client.inspect_exec(&exec_id).await?;
        if let Some(code) = inspect.exit_code {
            if code != 0 {
                anyhow::bail!(
                    "Command {:?} exited with code {}: {}",
                    command,
                    code,
                    result.trim()
                );
            }
        }

        Ok(result)
    }

    /// Execute a command and return whether it exited 0. Errors propagate only
    /// when the exec itself couldn't be created/started — a non-zero exit is
    /// reported as `Ok(false)`.
    pub async fn exec_status(&self, container: &str, command: &[&str]) -> Result<bool> {
        let exec = self
            .client
            .create_exec(
                container,
                CreateExecOptions {
                    cmd: Some(command.to_vec()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create exec")?;

        let exec_id = exec.id.clone();

        let output = self
            .client
            .start_exec(&exec_id, Some(StartExecOptions::default()))
            .await
            .context("Failed to start exec")?;

        // Drain the output stream so the exec actually runs to completion;
        // otherwise bollard may report `exit_code: None`.
        if let StartExecResults::Attached { mut output, .. } = output {
            while let Some(msg) = output.next().await {
                let _ = msg;
            }
        }

        let inspect = self.client.inspect_exec(&exec_id).await?;
        Ok(inspect.exit_code == Some(0))
    }

    /// Copy a file into a container at the specified directory path.
    /// The file will be created with mode 0755 (executable).
    pub async fn copy_to_container(
        &self,
        container: &str,
        file_name: &str,
        data: &[u8],
        dest_dir: &str,
    ) -> Result<()> {
        // Build a tar archive in memory containing the file
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar_builder.append_data(&mut header, file_name, data)?;
        let tar_bytes = tar_builder.into_inner()?;

        self.client
            .upload_to_container(
                container,
                Some(bollard::query_parameters::UploadToContainerOptions {
                    path: dest_dir.to_string(),
                    ..Default::default()
                }),
                bollard::body_full(tar_bytes.into()),
            )
            .await
            .with_context(|| {
                format!("Failed to copy {} to {}:{}", file_name, container, dest_dir)
            })?;

        Ok(())
    }

    /// Get published port bindings for a container.
    /// Returns a map of container_port -> host_port.
    ///
    /// Reads the live bindings, so it also resolves ports Docker allocated.
    /// Only the macOS Docker relay needs that.
    #[cfg(target_os = "macos")]
    pub async fn get_container_ports(
        &self,
        container: &str,
    ) -> Result<std::collections::HashMap<u16, u16>> {
        let info = self
            .client
            .inspect_container(container, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("Failed to inspect container {}", container))?;

        let mut port_map = std::collections::HashMap::new();

        if let Some(network) = info.network_settings {
            if let Some(ports) = network.ports {
                for (container_port_str, bindings) in ports {
                    // container_port_str is like "2375/tcp"
                    let container_port: u16 = container_port_str
                        .split('/')
                        .next()
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(0);
                    if container_port == 0 {
                        continue;
                    }
                    if let Some(binding_list) = bindings {
                        for binding in binding_list {
                            if let Some(ref host_port_str) = binding.host_port {
                                if let Ok(host_port) = host_port_str.parse::<u16>() {
                                    port_map.insert(container_port, host_port);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(port_map)
    }

    /// List containers by name prefix.
    ///
    /// Docker's `name` filter matches a substring anywhere in the name, so the
    /// results are post-filtered on the real prefix - otherwise a `k8s_` lookup
    /// would also return unrelated containers like `my_k8s_thing`.
    pub async fn list_containers_by_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec![prefix.to_string()]);

        let containers = self
            .client
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .context("Failed to list containers")?;

        let names: Vec<String> = containers
            .into_iter()
            .filter_map(|c| c.names)
            .flatten()
            .map(|n| n.trim_start_matches('/').to_string())
            .filter(|n| n.starts_with(prefix))
            .collect();

        Ok(names)
    }

    /// List k8s containers with their volume mount sources.
    /// Parses pod name + namespace from container name format `k8s_{container}_{pod}_{namespace}_{uid}_{attempt}`.
    pub async fn list_containers_with_mounts(
        &self,
        prefix: &str,
    ) -> Result<Vec<ContainerMountInfo>> {
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec![prefix.to_string()]);

        let containers = self
            .client
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .context("Failed to list containers with mounts")?;

        let mut result = Vec::new();
        for c in containers {
            let container_name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();

            // Parse pod name from k8s container name
            let parts: Vec<&str> = container_name.split('_').collect();
            if parts.len() < 4 {
                continue;
            }
            let pod_name = parts[2].to_string();

            // Extract mount sources
            let mounts = c
                .mounts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| Some(MountSource { source: m.source? }))
                .collect();

            result.push(ContainerMountInfo {
                container_name,
                pod_name,
                mounts,
            });
        }

        Ok(result)
    }

    /// Force-remove the given containers in parallel (no need to stop first).
    /// Best effort: a failed removal is logged, not fatal.
    pub async fn remove_containers(&self, names: &[String]) {
        let futures: Vec<_> = names
            .iter()
            .map(|name| async move {
                if let Err(e) = self.remove_container(name, true).await {
                    tracing::warn!(container = %name, error = %e, "Failed to remove container");
                }
            })
            .collect();

        futures_util::future::join_all(futures).await;
    }

    /// Containers carrying every one of `labels` (`key` or `key=value` form).
    async fn list_containers_by_labels(
        &self,
        labels: Vec<String>,
        all: bool,
    ) -> Vec<ContainerSummary> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), labels);

        match self
            .client
            .list_containers(Some(ListContainersOptions {
                all,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
        {
            Ok(containers) => containers,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to list containers by label");
                Vec::new()
            }
        }
    }

    /// Cluster names taken from the `k3dev.cluster` label of running k3s server
    /// containers. Used to decide whether a host-wide cleanup would hit another
    /// cluster, and to mark clusters as up in the switcher.
    ///
    /// Telling the server apart from the rest is the whole job here: the CRI
    /// proxy stamps `k3dev.cluster` on every pod container too, and those keep
    /// running after their cluster is stopped, so the cluster label alone would
    /// report a stopped cluster as running. Either the server role or the
    /// `<cluster>-server` name settles it — the name also covers a container
    /// created before the role label existed, and under-reporting here would
    /// drop a live cluster's router route.
    pub async fn list_cluster_servers(&self) -> Vec<String> {
        self.list_containers_by_labels(vec!["k3dev.cluster".to_string()], false)
            .await
            .into_iter()
            .filter_map(|c| {
                let cluster = c.labels.as_ref()?.get("k3dev.cluster")?.clone();
                Self::is_cluster_server(&c, &cluster).then_some(cluster)
            })
            .collect()
    }

    /// Is this listed container the k3s server of `cluster`, as opposed to one
    /// of the pod containers carrying the same `k3dev.cluster` label?
    ///
    /// The role label answers it; the `<cluster>-server` name is the fallback
    /// for a container created before that label existed. Both the "which
    /// clusters are up" question and the pod-container sweep key on this, and
    /// they must agree: a server counted as a pod container would be swept.
    fn is_cluster_server(container: &ContainerSummary, cluster: &str) -> bool {
        let is_server_role = container
            .labels
            .as_ref()
            .and_then(|l| l.get("k3dev.role"))
            .map(String::as_str)
            == Some("server");

        let server_name = format!("{}-server", cluster);
        let named_server = container
            .names
            .iter()
            .flatten()
            .any(|n| n.trim_start_matches('/') == server_name);

        is_server_role || named_server
    }

    /// Names of one cluster's pod containers, running or not: everything the
    /// CRI proxy labelled for the cluster, minus the cluster's own server.
    ///
    /// Scoped to the label on purpose - the `k8s_*` names Docker shows carry no
    /// cluster identity, so a name sweep would reap another cluster's pods.
    pub async fn list_cluster_pod_containers(&self, cluster: &str) -> Vec<String> {
        self.list_containers_by_labels(vec![format!("k3dev.cluster={}", cluster)], true)
            .await
            .into_iter()
            .filter(|c| !Self::is_cluster_server(c, cluster))
            .filter_map(|c| c.names)
            .flatten()
            .map(|n| n.trim_start_matches('/').to_string())
            .collect()
    }

    // === Network Operations ===

    /// Create a Docker network
    pub async fn create_network(&self, name: &str) -> Result<()> {
        // Check if network exists
        if self
            .client
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
            .is_ok()
        {
            return Ok(()); // Already exists
        }

        self.client
            .create_network(NetworkCreateRequest {
                name: name.to_string(),
                ..Default::default()
            })
            .await
            .with_context(|| format!("Failed to create network {}", name))?;

        Ok(())
    }

    /// Remove a Docker network
    pub async fn remove_network(&self, name: &str) -> Result<()> {
        // Ignore errors - network might not exist
        let _ = self.client.remove_network(name).await;
        Ok(())
    }

    /// Attach a container to a network (no-op if already attached)
    pub async fn connect_network(&self, network: &str, container: &str) -> Result<()> {
        match self
            .client
            .connect_network(
                network,
                NetworkConnectRequest {
                    container: container.to_string(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(()) => Ok(()),
            // 403 is Docker's "endpoint already exists on this network"
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 403, ..
            }) => Ok(()),
            Err(e) => Err(e)
                .with_context(|| format!("Failed to connect {} to network {}", container, network)),
        }
    }

    /// Detach a container from a network (no-op if not attached)
    pub async fn disconnect_network(&self, network: &str, container: &str) -> Result<()> {
        // Errors here are always "not attached" or "no such network" — both mean
        // the endpoint is already gone, which is what the caller wanted.
        let _ = self
            .client
            .disconnect_network(
                network,
                NetworkDisconnectRequest {
                    container: container.to_string(),
                    force: Some(true),
                },
            )
            .await;
        Ok(())
    }

    // === Volume Operations ===

    /// Create a Docker volume
    pub async fn create_volume(&self, name: &str) -> Result<()> {
        self.client
            .create_volume(VolumeCreateRequest {
                name: Some(name.to_string()),
                ..Default::default()
            })
            .await
            .with_context(|| format!("Failed to create volume {}", name))?;

        Ok(())
    }

    /// Remove a Docker volume.
    ///
    /// The k3dev volume names are shared constants, so a stale container left
    /// over from a previous cluster (e.g. a differently-named config) can pin
    /// the volume and make removal fail with "volume is in use". Any container
    /// referencing these volumes belongs to k3dev, so force-remove them first
    /// to guarantee the volume (and its etcd/PV state) is actually deleted.
    ///
    /// Docker releases a volume's reference count asynchronously after a
    /// container is removed, so removal can still report "in use" immediately
    /// after force-removing the holder. Retry a few times to absorb that lag.
    pub async fn remove_volume(&self, name: &str) -> Result<()> {
        for attempt in 0..5 {
            // Force-remove any container still referencing this volume.
            let mut filters = HashMap::new();
            filters.insert("volume".to_string(), vec![name.to_string()]);
            if let Ok(containers) = self
                .client
                .list_containers(Some(ListContainersOptions {
                    all: true,
                    filters: Some(filters),
                    ..Default::default()
                }))
                .await
            {
                let names: Vec<String> = containers
                    .into_iter()
                    .filter_map(|c| c.names)
                    .flatten()
                    .map(|n| n.trim_start_matches('/').to_string())
                    .collect();
                for container in names {
                    let _ = self.remove_container(&container, true).await;
                }
            }

            match self
                .client
                .remove_volume(name, Some(RemoveVolumeOptions { force: true }))
                .await
            {
                // Success, or the volume doesn't exist — nothing more to do.
                Ok(()) => return Ok(()),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(()),
                Err(e) => {
                    if attempt == 4 {
                        tracing::warn!(volume = %name, error = %e, "Failed to remove volume after retries");
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        Ok(())
    }

    // === Image Operations ===

    /// Pull a Docker image
    pub async fn pull_image(&self, image: &str) -> Result<()> {
        let options = Some(CreateImageOptions {
            from_image: Some(image.to_string()),
            ..Default::default()
        });

        let mut stream = self.client.create_image(options, None, None);

        while let Some(result) = stream.next().await {
            result.with_context(|| format!("Failed to pull image {}", image))?;
        }

        Ok(())
    }

    /// Check if an image exists locally
    pub async fn image_exists(&self, image: &str) -> bool {
        self.client.inspect_image(image).await.is_ok()
    }

    /// Get the architecture of a Docker image (e.g., "amd64", "arm64")
    pub async fn get_image_architecture(&self, image: &str) -> Option<String> {
        self.client
            .inspect_image(image)
            .await
            .ok()
            .and_then(|info| info.architecture)
    }

    /// Get image architectures for all running k8s pod containers.
    /// Returns a map of "namespace/pod_name" → image architecture string.
    pub async fn get_pod_image_architectures(&self, cgroup_root: &str) -> HashMap<String, String> {
        let containers = match self
            .client
            .list_containers(Some(ListContainersOptions {
                all: false,
                ..Default::default()
            }))
            .await
        {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // Collect unique images and map pod keys to image names
        let mut pod_to_image: HashMap<String, String> = HashMap::new();
        let mut unique_images: HashSet<String> = HashSet::new();

        for container in &containers {
            let name = container
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();

            // Only k8s workload containers (skip pause containers)
            if !name.starts_with("k8s_") || name.starts_with("k8s_POD_") {
                continue;
            }

            let image = container.image.clone().unwrap_or_default();
            if image.is_empty() {
                continue;
            }

            // Skip pods owned by another cluster on this daemon
            if !self.container_in_cgroup_root(&name, cgroup_root).await {
                continue;
            }

            // Parse pod name and namespace from k8s_{container}_{pod}_{namespace}_{uid}_{attempt}
            let parts: Vec<&str> = name.split('_').collect();
            if parts.len() >= 4 {
                let pod_key = format!("{}/{}", parts[3], parts[2]);
                unique_images.insert(image.clone());
                pod_to_image.insert(pod_key, image);
            }
        }

        // Inspect each unique image for architecture
        let mut image_arch: HashMap<String, String> = HashMap::new();
        for image in &unique_images {
            if let Some(arch) = self.get_image_architecture(image).await {
                image_arch.insert(image.clone(), arch);
            }
        }

        // Map pod keys to their image's architecture
        let mut result = HashMap::new();
        for (pod_key, image) in &pod_to_image {
            if let Some(arch) = image_arch.get(image) {
                result.insert(pod_key.clone(), arch.clone());
            }
        }

        result
    }

    /// Get labels from a Docker image
    pub async fn get_image_labels(&self, image: &str) -> HashMap<String, String> {
        match self.client.inspect_image(image).await {
            Ok(info) => info.config.and_then(|c| c.labels).unwrap_or_default(),
            Err(_) => HashMap::new(),
        }
    }

    /// Commit a running container to a new image
    pub async fn commit_container(
        &self,
        container: &str,
        image: &str,
        labels: HashMap<String, String>,
    ) -> Result<()> {
        let options = CommitContainerOptions {
            container: Some(container.to_string()),
            repo: Some(image.to_string()),
            tag: Some(String::new()),
            comment: Some("k3dev snapshot".to_string()),
            author: Some("k3dev".to_string()),
            pause: false, // Don't pause container during commit
            changes: None,
        };

        let config = ContainerConfig {
            labels: Some(labels),
            ..Default::default()
        };

        self.client
            .commit_container(options, config)
            .await
            .with_context(|| {
                format!(
                    "Failed to commit container {} to image {}",
                    container, image
                )
            })?;

        tracing::info!(container = %container, image = %image, "Container committed to image");
        Ok(())
    }

    /// List this cluster's images whose tag starts with `pattern`.
    /// Scoped by the `k3dev.cluster` image label, not by name alone.
    pub async fn list_images_by_pattern(
        &self,
        pattern: &str,
        cluster: &str,
    ) -> Result<Vec<String>> {
        let options = Some(ListImagesOptions {
            all: false,
            ..Default::default()
        });

        let images = self
            .client
            .list_images(options)
            .await
            .context("Failed to list images")?;

        let mut matching_images = Vec::new();
        for image in images {
            // The name prefix alone is ambiguous: `k3dev-snapshot-one-` is also a
            // prefix of cluster `one-two`'s images, so a sweep scoped by name
            // would delete another cluster's snapshots. The label is exact.
            if image.labels.get("k3dev.cluster").map(String::as_str) != Some(cluster) {
                continue;
            }
            for tag in &image.repo_tags {
                if tag.starts_with(pattern) {
                    matching_images.push(tag.clone());
                }
            }
        }

        Ok(matching_images)
    }

    /// Remove an image
    pub async fn remove_image(&self, image: &str) -> Result<()> {
        let options = Some(RemoveImageOptions {
            force: true,
            noprune: false,
            platforms: None,
        });

        self.client
            .remove_image(image, options, None)
            .await
            .with_context(|| format!("Failed to remove image {}", image))?;

        tracing::debug!(image = %image, "Image removed");
        Ok(())
    }

    // === Container Run Operations ===

    /// Exposed ports and their host bindings for `config.ports`.
    ///
    /// A host port of 0 is left out of the binding so Docker allocates one:
    /// probing for a free port from here is racy, and on macOS the probe can't
    /// see ports Docker Desktop holds on the wildcard address.
    #[allow(clippy::type_complexity)]
    fn port_bindings(
        config: &ContainerRunConfig,
    ) -> (Vec<String>, HashMap<String, Option<Vec<PortBinding>>>) {
        let host_ip = config.host_ip.as_deref().unwrap_or("0.0.0.0");
        let mut exposed_ports: Vec<String> = Vec::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();

        for (host, container) in &config.ports {
            let container_port = format!("{}/tcp", container);
            exposed_ports.push(container_port.clone());
            port_bindings.insert(
                container_port,
                Some(vec![PortBinding {
                    host_ip: Some(host_ip.to_string()),
                    host_port: (*host != 0).then(|| host.to_string()),
                }]),
            );
        }

        (exposed_ports, port_bindings)
    }

    /// Run a new container
    pub async fn run_container(&self, config: &ContainerRunConfig) -> Result<()> {
        let (exposed_ports, port_bindings) = Self::port_bindings(config);

        let env: Vec<String> = config
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let mut binds = Vec::new();
        let mut mounts = Vec::new();

        for (src, dst, options) in &config.volumes {
            if options.is_empty() {
                // Simple volume mount
                binds.push(format!("{}:{}", src, dst));
            } else if options == "volume" {
                // Named Docker volume
                binds.push(format!("{}:{}", src, dst));
            } else if options.starts_with("bind-propagation=") {
                // Bind mount with propagation
                let propagation_str = options
                    .strip_prefix("bind-propagation=")
                    .unwrap_or("rprivate");
                let propagation = match propagation_str {
                    "private" => MountBindOptionsPropagationEnum::PRIVATE,
                    "rprivate" => MountBindOptionsPropagationEnum::RPRIVATE,
                    "shared" => MountBindOptionsPropagationEnum::SHARED,
                    "rshared" => MountBindOptionsPropagationEnum::RSHARED,
                    "slave" => MountBindOptionsPropagationEnum::SLAVE,
                    "rslave" => MountBindOptionsPropagationEnum::RSLAVE,
                    _ => MountBindOptionsPropagationEnum::RPRIVATE,
                };
                mounts.push(Mount {
                    target: Some(dst.clone()),
                    source: Some(src.clone()),
                    typ: Some(MountTypeEnum::BIND),
                    bind_options: Some(MountBindOptions {
                        propagation: Some(propagation),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            } else {
                // Bind mount with other options
                mounts.push(Mount {
                    target: Some(dst.clone()),
                    source: Some(src.clone()),
                    typ: Some(MountTypeEnum::BIND),
                    ..Default::default()
                });
            }
        }

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            binds: if binds.is_empty() { None } else { Some(binds) },
            mounts: if mounts.is_empty() {
                None
            } else {
                Some(mounts)
            },
            privileged: Some(config.privileged),
            network_mode: config.network.clone(),
            cgroupns_mode: if config.cgroupns_host {
                // HostConfigCgroupnsModeEnum was added in Docker API v1.41 (Docker 20.10+).
                // Older Docker versions reject container creation when this option is set.
                let api_v141 = ClientVersion {
                    major_version: 1,
                    minor_version: 41,
                };
                if self.client.client_version() >= api_v141 {
                    Some(HostConfigCgroupnsModeEnum::HOST)
                } else {
                    tracing::warn!(
                        "Docker API version {} < 1.41: skipping cgroupns_mode (not supported)",
                        self.client.client_version()
                    );
                    None
                }
            } else {
                None
            },
            pid_mode: if config.pid_host {
                Some("host".to_string())
            } else {
                None
            },
            security_opt: if config.security_opt.is_empty() {
                None
            } else {
                Some(config.security_opt.clone())
            },
            auto_remove: if config.auto_remove { Some(true) } else { None },
            ..Default::default()
        };

        let container_config = ContainerCreateBody {
            image: Some(config.image.clone()),
            hostname: config.hostname.clone(),
            env: if env.is_empty() { None } else { Some(env) },
            exposed_ports: if exposed_ports.is_empty() {
                None
            } else {
                Some(exposed_ports)
            },
            host_config: Some(host_config),
            entrypoint: config.entrypoint.as_ref().map(|e| {
                if e.is_empty() {
                    vec![]
                } else {
                    vec![e.clone()]
                }
            }),
            cmd: config.command.clone(),
            labels: if config.labels.is_empty() {
                None
            } else {
                Some(config.labels.clone())
            },
            ..Default::default()
        };

        self.client
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(config.name.clone()),
                    ..Default::default()
                }),
                container_config,
            )
            .await
            .with_context(|| format!("Failed to create container {}", config.name))?;

        if config.detach {
            // A container whose first start failed never had its networking set
            // up. Left behind, it blocks the name, and a later plain start
            // brings it up without its ports or network, so remove it.
            if let Err(e) = self.start_container(&config.name).await {
                if let Err(rm) = self.remove_container(&config.name, true).await {
                    tracing::warn!(
                        container = %config.name,
                        error = %rm,
                        "Failed to remove container after failed start"
                    );
                }
                return Err(e);
            }
        }

        Ok(())
    }
}

/// Container with parsed pod info and volume mounts
pub struct ContainerMountInfo {
    pub container_name: String,
    pub pod_name: String,
    pub mounts: Vec<MountSource>,
}

/// A single mount source from a container
pub struct MountSource {
    pub source: String,
}

/// Configuration for running a Docker container
#[derive(Debug, Clone, Default)]
pub struct ContainerRunConfig {
    pub name: String,
    pub hostname: Option<String>,
    pub image: String,
    pub detach: bool,
    pub privileged: bool,
    /// (host, container) pairs; a host port of 0 lets Docker pick a free one
    pub ports: Vec<(u16, u16)>,
    /// Host address the published ports bind to (`None` = all interfaces)
    pub host_ip: Option<String>,
    pub volumes: Vec<(String, String, String)>, // (src, dst, options)
    pub env: Vec<(String, String)>,
    pub network: Option<String>,
    pub cgroupns_host: bool,
    pub pid_host: bool,
    pub entrypoint: Option<String>,
    pub command: Option<Vec<String>>,
    /// Security options (e.g., "apparmor=unconfined", "seccomp=unconfined")
    pub security_opt: Vec<String>,
    /// Container labels (used for filtering/cleanup)
    pub labels: HashMap<String, String>,
    /// Auto-remove the container when it exits
    pub auto_remove: bool,
}

/// Docker could not publish one of a container's host ports at start.
///
/// Its `Display` is the whole message for the user: the CLI prints only the
/// outermost error, so wrapping this in context would hide it.
#[derive(Debug)]
pub struct PortBindError {
    container: String,
    /// The host port Docker's message names, when it could be parsed
    host_port: Option<u16>,
    /// The bind was refused (a privileged port), not found taken
    denied: bool,
    /// The container itself publishes `host_port` more than once
    reused: bool,
    /// Running container that already publishes `host_port`
    holder: Option<String>,
    /// Host ports the container publishes, for when `host_port` is unknown
    published: Vec<u16>,
    /// Config settings that chose `host_port`, see [`name_port_settings`]
    settings: Vec<String>,
    macos: bool,
    docker_error: String,
}

impl std::error::Error for PortBindError {}

impl std::fmt::Display for PortBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = &self.container;
        let Some(port) = self.host_port else {
            write!(
                f,
                "Failed to start container '{}': a host port could not be bound.\n\n\
                 Docker error: {}",
                name, self.docker_error
            )?;
            if !self.published.is_empty() {
                let ports: Vec<String> = self.published.iter().map(u16::to_string).collect();
                write!(f, "\n\nHost ports it publishes: {}", ports.join(", "))?;
            }
            if self.denied && self.published.iter().any(|p| *p < 1024) {
                write!(f, "\n\nIf a port below 1024 is the one refused:")?;
                write!(f, "\n- {}", privileged_port_fix(self.macos))?;
            }
            return Ok(());
        };

        let privileged = self.denied && port < 1024;
        let freeing = if self.denied {
            write!(
                f,
                "Failed to start container '{}': not allowed to bind host port {}.",
                name, port
            )?;
            privileged.then(|| privileged_port_fix(self.macos).to_string())
        } else if self.reused {
            write!(
                f,
                "Failed to start container '{}': it publishes host port {} more than once.",
                name, port
            )?;
            None
        } else if let Some(holder) = &self.holder {
            write!(
                f,
                "Failed to start container '{}': host port {} is already used by container '{}'.",
                name, port, holder
            )?;
            Some(format!("Stop container '{}'", holder))
        } else {
            write!(
                f,
                "Failed to start container '{}': host port {} is already in use on this host.",
                name, port
            )?;
            Some(format!(
                "Stop the process holding it (find it with: sudo lsof -nP -iTCP:{} -sTCP:LISTEN)",
                port
            ))
        };
        let moving = if self.settings.is_empty() {
            None
        } else if self.reused && !privileged {
            Some(format!(
                "Give {} different host ports",
                self.settings.join(" and ")
            ))
        } else {
            let target = if privileged {
                "a host port above 1023"
            } else {
                "a free host port"
            };
            Some(format!(
                "Change {} to {}",
                self.settings.join(" or "),
                target
            ))
        };
        // The holder is often another cluster or the shared router, so moving
        // off the port comes first. A refused port below 1024 is usually
        // 80/443 on purpose, so allowing it comes first.
        let fixes = if privileged {
            [freeing, moving]
        } else {
            [moving, freeing]
        };

        write!(f, "\n\nDocker error: {}", self.docker_error)?;
        for (i, fix) in fixes.iter().flatten().enumerate() {
            if i == 0 {
                write!(f, "\n\nTo fix:\n- {}", fix)?;
            } else {
                // Every fix starts with an ASCII capital
                let (first, rest) = fix.split_at(1);
                write!(f, "\n- Or {}{}", first.to_lowercase(), rest)?;
            }
        }
        Ok(())
    }
}

fn privileged_port_fix(macos: bool) -> &'static str {
    if macos {
        "Enable privileged port mapping in Docker Desktop → Settings → Advanced"
    } else {
        "Let the Docker daemon bind ports below 1024 (rootless Docker: \
         lower net.ipv4.ip_unprivileged_port_start)"
    }
}

/// Point a [`PortBindError`] at the config settings that chose its host port,
/// so the message says what to change. Other errors pass through.
///
/// Only for containers created from the current config: an existing container
/// keeps the ports it was created with, so changing a setting would not help.
pub fn name_port_settings(
    mut err: anyhow::Error,
    settings_for: impl FnOnce(u16) -> Vec<String>,
) -> anyhow::Error {
    if let Some(bind) = err.downcast_mut::<PortBindError>() {
        bind.settings = bind.host_port.map(settings_for).unwrap_or_default();
    }
    err
}

/// What a failed start says about publishing a host port
#[derive(Debug, PartialEq)]
struct BindFailure {
    host_port: Option<u16>,
    denied: bool,
}

/// Phrases Docker puts right before the host `address:port` it failed to bind.
/// Docker Engine and Docker Desktop word this differently, and so do releases.
const BIND_ADDRESS_MARKERS: &[&str] = &[
    // Another container already publishes it
    "bind for ",
    // Docker Engine 28: `<host ip>:<host port>:<container ip>:<port>/tcp`
    "bind host port for ",
    // Docker Engine 29+: `<host ip>:<host port>/tcp`
    "bind host port ",
    // Docker Desktop, before the inner `listen tcp` error
    "exposing port tcp ",
    "exposing port udp ",
    // Userland proxy, rootlesskit and Docker Desktop: `listen tcp4 0.0.0.0:80`
    "listen tcp",
    "listen udp",
];

/// Recognize a failed start as a host port problem, and find the port
fn parse_bind_failure(message: &str) -> Option<BindFailure> {
    let lower = message.to_lowercase();
    let host_port = BIND_ADDRESS_MARKERS.iter().find_map(|marker| {
        let rest = &lower[lower.find(marker)? + marker.len()..];
        // `listen tcp4 0.0.0.0:80` puts the address family first
        let address = rest.split_whitespace().take(2).find(|w| w.contains(':'))?;
        host_port_of(address)
    });

    // The port can't always be parsed, but these say it is a port problem.
    // "permission denied" alone is not enough: a mount can fail with it too.
    let port_problem = host_port.is_some()
        || [
            "port is already allocated",
            "address already in use",
            "ports are not available",
        ]
        .iter()
        .any(|phrase| lower.contains(phrase));

    port_problem.then(|| BindFailure {
        host_port,
        denied: lower.contains("permission denied"),
    })
}

/// The host port in an address Docker printed. Anything may follow the port
/// (`/tcp`, `:`, or the container side), so it is read after the host IP.
fn host_port_of(address: &str) -> Option<u16> {
    let after_ip = match address.strip_prefix('[') {
        Some(bracketed) => bracketed.split_once("]:")?.1,
        None => match address.split_once(':')? {
            (ip, rest) if ip.contains('.') => rest,
            // No host IP at all
            (port, _) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => port,
            // Unbracketed IPv6, as in `Bind for :::80`: the port comes last
            _ => address.trim_end_matches(':').rsplit_once(':')?.1,
        },
    };
    let digits = after_ip
        .find(|c: char| !c.is_ascii_digit())
        .map_or(after_ip, |end| &after_ip[..end]);
    digits.parse().ok().filter(|port| *port != 0)
}

/// Name of the container, other than `except`, that publishes host port `port`
fn publisher_of(containers: &[ContainerSummary], port: u16, except: &str) -> Option<String> {
    containers.iter().find_map(|c| {
        let publishes = c
            .ports
            .as_ref()?
            .iter()
            .any(|p| p.public_port == Some(port));
        let name = c.names.as_ref()?.first()?.trim_start_matches('/');
        (publishes && name != except).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::PortSummary;

    fn summary(name: &str, labels: &[(&str, &str)]) -> ContainerSummary {
        ContainerSummary {
            names: Some(vec![format!("/{}", name)]),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn server_is_identified_by_role_or_name() {
        let by_role = summary(
            "custom-name",
            &[("k3dev.cluster", "alpha"), ("k3dev.role", "server")],
        );
        assert!(DockerManager::is_cluster_server(&by_role, "alpha"));

        // Containers created before the role label existed only have the name.
        let by_name = summary("alpha-server", &[("k3dev.cluster", "alpha")]);
        assert!(DockerManager::is_cluster_server(&by_name, "alpha"));
    }

    #[test]
    fn pod_containers_are_not_mistaken_for_the_server() {
        // The CRI proxy stamps the cluster label on pod containers too, so the
        // sweep that removes them has to spare the server and nothing else.
        let pause = summary(
            "k8s_POD_nginx-0_default_1234_0",
            &[("k3dev.cluster", "alpha")],
        );
        assert!(!DockerManager::is_cluster_server(&pause, "alpha"));

        // The name fallback is scoped to the cluster being asked about.
        let other = summary("beta-server", &[("k3dev.cluster", "beta")]);
        assert!(!DockerManager::is_cluster_server(&other, "alpha"));
    }

    fn binding(config: &ContainerRunConfig, container_port: u16) -> PortBinding {
        let (exposed, bindings) = DockerManager::port_bindings(config);
        let key = format!("{}/tcp", container_port);
        assert!(exposed.contains(&key));
        bindings[&key].as_ref().unwrap()[0].clone()
    }

    #[test]
    fn ports_publish_on_all_interfaces_by_default() {
        let config = ContainerRunConfig {
            ports: vec![(6451, 6443)],
            ..Default::default()
        };
        let b = binding(&config, 6443);
        assert_eq!(b.host_ip.as_deref(), Some("0.0.0.0"));
        assert_eq!(b.host_port.as_deref(), Some("6451"));
    }

    #[test]
    fn host_ip_and_docker_assigned_port() {
        let config = ContainerRunConfig {
            ports: vec![(0, 2375)],
            host_ip: Some("127.0.0.1".to_string()),
            ..Default::default()
        };
        let b = binding(&config, 2375);
        assert_eq!(b.host_ip.as_deref(), Some("127.0.0.1"));
        // No host port: Docker allocates a free one
        assert_eq!(b.host_port, None);
    }

    fn failure(host_port: u16, denied: bool) -> Option<BindFailure> {
        Some(BindFailure {
            host_port: Some(host_port),
            denied,
        })
    }

    #[test]
    fn bind_failures_name_the_host_port() {
        let cases = [
            // Docker Desktop: another container publishes the port
            (
                "Docker responded with status code 500: failed to set up container networking: \
                 driver failed programming external connectivity on endpoint two-server (e001): \
                 Bind for 0.0.0.0:6451 failed: port is already allocated",
                failure(6451, false),
            ),
            // Docker Desktop: a host process holds it
            (
                "ports are not available: exposing port TCP 0.0.0.0:18432 -> 127.0.0.1:0: \
                 listen tcp 0.0.0.0:18432: bind: address already in use",
                failure(18432, false),
            ),
            // Docker Desktop without privileged port mapping
            (
                "ports are not available: exposing port TCP 0.0.0.0:80 -> 0.0.0.0:0: \
                 listen tcp 0.0.0.0:80: bind: permission denied",
                failure(80, true),
            ),
            // Engine 28 prints the whole binding, container side included
            (
                "driver failed programming external connectivity on endpoint web (abc): \
                 failed to bind host port for 0.0.0.0:6451:172.18.0.2:6443/tcp: \
                 address already in use",
                failure(6451, false),
            ),
            (
                "failed to bind host port for [::]:6451:[fd00::2]:6443/tcp: address already in use",
                failure(6451, false),
            ),
            // Engine 29+
            (
                "driver failed programming external connectivity on endpoint web (abc): \
                 failed to bind host port 0.0.0.0:8080/tcp: address already in use",
                failure(8080, false),
            ),
            (
                "failed to bind host port [::]:8443/tcp: address already in use",
                failure(8443, false),
            ),
            // Engine allocator with an IPv6 host address, printed unbracketed
            (
                "Bind for :::8080 failed: port is already allocated",
                failure(8080, false),
            ),
            // Older Engine userland proxy
            (
                "Error starting userland proxy: listen tcp4 0.0.0.0:443: bind: address already in use",
                failure(443, false),
            ),
            // Rootless Docker
            (
                "error while calling RootlessKit PortManager.AddPort(): cannot expose privileged \
                 port 80, you can add 'net.ipv4.ip_unprivileged_port_start=80' to \
                 /etc/sysctl.conf (currently 1024): listen tcp4 0.0.0.0:80: bind: permission denied",
                failure(80, true),
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(parse_bind_failure(message), expected, "{}", message);
        }
    }

    #[test]
    fn bind_failures_without_a_parsable_port_are_still_recognized() {
        assert_eq!(
            parse_bind_failure("ports are not available: something new"),
            Some(BindFailure {
                host_port: None,
                denied: false
            })
        );
    }

    #[test]
    fn mount_errors_are_not_port_errors() {
        // Both used to match the old "bind" / "permission denied" test
        for message in [
            "invalid mount config for type \"bind\": bind source path does not exist: /nope",
            "error mounting \"/data\" to rootfs at \"/data\": permission denied",
        ] {
            assert_eq!(parse_bind_failure(message), None, "{}", message);
        }
    }

    fn published(name: &str, ports: &[u16]) -> ContainerSummary {
        ContainerSummary {
            names: Some(vec![format!("/{}", name)]),
            ports: Some(
                ports
                    .iter()
                    .map(|p| PortSummary {
                        public_port: Some(*p),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn the_holder_is_another_container_publishing_the_port() {
        let containers = [
            published("k3dev-router", &[80, 443]),
            published("one-server", &[6451]),
            published("two-server", &[6451]),
        ];
        assert_eq!(
            publisher_of(&containers, 6451, "two-server").as_deref(),
            Some("one-server")
        );
        assert_eq!(
            publisher_of(&containers, 6451, "one-server").as_deref(),
            Some("two-server")
        );
        assert_eq!(publisher_of(&containers, 8080, "two-server"), None);
    }

    fn bind_error(host_port: Option<u16>, denied: bool) -> PortBindError {
        PortBindError {
            container: "two-server".to_string(),
            host_port,
            denied,
            reused: false,
            holder: None,
            published: Vec::new(),
            settings: Vec::new(),
            macos: true,
            docker_error: "docker said no".to_string(),
        }
    }

    fn api_port() -> Vec<String> {
        vec!["`infrastructure.api_port`".to_string()]
    }

    #[test]
    fn a_taken_port_names_its_holder_and_setting_not_privileged_ports() {
        let err = PortBindError {
            holder: Some("one-server".to_string()),
            settings: api_port(),
            ..bind_error(Some(6451), false)
        };
        let msg = err.to_string();
        assert!(msg.starts_with(
            "Failed to start container 'two-server': \
             host port 6451 is already used by container 'one-server'."
        ));
        // Moving off the port comes before stopping another cluster
        assert!(msg.ends_with(
            "Docker error: docker said no\n\n\
             To fix:\n\
             - Change `infrastructure.api_port` to a free host port\n\
             - Or stop container 'one-server'"
        ));
        assert!(!msg.contains("rivileged"), "{}", msg);
        assert!(!msg.contains("http_port"), "{}", msg);
    }

    #[test]
    fn a_port_held_outside_docker_points_at_lsof() {
        let msg = bind_error(Some(6451), false).to_string();
        assert!(msg.contains("host port 6451 is already in use on this host"));
        assert!(msg.contains("lsof -nP -iTCP:6451 -sTCP:LISTEN"));
        // No setting is known, so none is suggested
        assert!(!msg.contains("infrastructure"), "{}", msg);
    }

    #[test]
    fn a_port_published_twice_names_both_settings() {
        let err = PortBindError {
            reused: true,
            settings: vec![
                "`infrastructure.http_port`".to_string(),
                "the `8080:8080` entry in `infrastructure.additional_ports`".to_string(),
            ],
            ..bind_error(Some(8080), false)
        };
        let msg = err.to_string();
        assert!(msg.starts_with(
            "Failed to start container 'two-server': it publishes host port 8080 more than once."
        ));
        assert!(msg.ends_with(
            "To fix:\n\
             - Give `infrastructure.http_port` and the `8080:8080` entry in \
             `infrastructure.additional_ports` different host ports"
        ));
        assert!(!msg.contains("lsof"), "{}", msg);
    }

    #[test]
    fn a_refused_privileged_port_gets_the_platform_fix() {
        let mac = PortBindError {
            settings: vec!["`infrastructure.http_port`".to_string()],
            ..bind_error(Some(80), true)
        };
        let msg = mac.to_string();
        assert!(msg.contains("not allowed to bind host port 80"));
        assert!(msg.contains("- Enable privileged port mapping in Docker Desktop"));
        assert!(msg.ends_with("- Or change `infrastructure.http_port` to a host port above 1023"));

        let linux = PortBindError {
            macos: false,
            ..bind_error(Some(80), true)
        };
        let msg = linux.to_string();
        assert!(msg.contains("ip_unprivileged_port_start"));
        assert!(!msg.contains("Docker Desktop"), "{}", msg);
    }

    #[test]
    fn a_refused_high_port_suggests_only_the_setting() {
        let err = PortBindError {
            settings: api_port(),
            ..bind_error(Some(6451), true)
        };
        let msg = err.to_string();
        assert!(msg.ends_with("To fix:\n- Change `infrastructure.api_port` to a free host port"));
        assert!(!msg.contains("rivileged"), "{}", msg);
    }

    #[test]
    fn an_unknown_port_gets_privileged_advice_only_when_refused() {
        let taken = PortBindError {
            published: vec![80, 443, 6451],
            ..bind_error(None, false)
        };
        let msg = taken.to_string();
        assert!(msg.contains("a host port could not be bound"));
        assert!(msg.contains("Host ports it publishes: 80, 443, 6451"));
        assert!(!msg.contains("rivileged"), "{}", msg);

        let high = PortBindError {
            published: vec![6451],
            ..bind_error(None, true)
        };
        assert!(!high.to_string().contains("rivileged"));

        let low = PortBindError {
            published: vec![80, 443, 6451],
            ..bind_error(None, true)
        };
        assert!(low.to_string().contains("Enable privileged port mapping"));
    }

    #[test]
    fn the_settings_are_named_through_context() {
        let err = anyhow::Error::new(bind_error(Some(6451), false)).context("creating");
        let err = name_port_settings(err, |port| if port == 6451 { api_port() } else { vec![] });
        let bind = err.downcast_ref::<PortBindError>().unwrap();
        assert_eq!(bind.settings, api_port());

        // Other errors pass through untouched
        let other = name_port_settings(anyhow!("boom"), |_| api_port());
        assert_eq!(other.to_string(), "boom");
    }
}
