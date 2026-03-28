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
pub use stats::{ContainerStats, ResourceStats};

use anyhow::{anyhow, Context, Result};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerConfig, ContainerCreateBody, HostConfig, HostConfigCgroupnsModeEnum, Mount,
    MountBindOptions, MountBindOptionsPropagationEnum, MountTypeEnum, NetworkCreateRequest,
    PortBinding, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CommitContainerOptions, CreateContainerOptions, CreateImageOptions,
    DownloadFromContainerOptions, InspectContainerOptions, InspectNetworkOptions,
    ListContainersOptions, ListImagesOptions, RemoveContainerOptions, RemoveImageOptions,
    RemoveVolumeOptions, StartContainerOptions, StopContainerOptions, WaitContainerOptions,
};
use bollard::ClientVersion;
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

/// Docker container and network management
pub struct DockerManager {
    #[allow(dead_code)]
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

    /// Connect to Docker using the resolved socket path.
    /// Falls back to DOCKER_HOST / default if the path doesn't exist (TCP/remote).
    fn connect(socket_path: &PathBuf) -> Result<Docker> {
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

    /// Wait for Docker to become accessible
    #[allow(dead_code)]
    pub async fn wait_for_docker(&self, max_retries: u32, interval: Duration) -> Result<()> {
        for i in 0..max_retries {
            if self.is_accessible().await {
                return Ok(());
            }
            if i < max_retries - 1 {
                sleep(interval).await;
            }
        }
        Err(anyhow!(
            "Docker not accessible after {} retries",
            max_retries
        ))
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

    /// Start a stopped container
    pub async fn start_container(&self, name: &str) -> Result<()> {
        self.client
            .start_container(name, None::<StartContainerOptions>)
            .await
            .with_context(|| format!("Failed to start container {}", name))
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

    /// Remove a container
    pub async fn remove_container(&self, name: &str, force: bool) -> Result<()> {
        self.client
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("Failed to remove container {}", name))
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

    /// Copy a file from a container
    #[allow(dead_code)]
    pub async fn copy_from_container(
        &self,
        container: &str,
        src: &str,
        dst: &PathBuf,
    ) -> Result<()> {
        let mut stream = self.client.download_from_container(
            container,
            Some(DownloadFromContainerOptions {
                path: src.to_string(),
            }),
        );

        let mut file = tokio::fs::File::create(dst)
            .await
            .with_context(|| format!("Failed to create file {:?}", dst))?;

        while let Some(chunk) = stream.next().await {
            let data = chunk.context("Failed to read from container")?;
            file.write_all(&data)
                .await
                .context("Failed to write to file")?;
        }

        Ok(())
    }

    /// List containers by name prefix
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

            // Parse pod name + namespace from k8s container name
            let parts: Vec<&str> = container_name.split('_').collect();
            if parts.len() < 4 {
                continue;
            }
            let pod_name = parts[2].to_string();
            let namespace = parts[3].to_string();

            // Extract mount sources
            let mounts = c
                .mounts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| {
                    Some(MountSource {
                        source: m.source?,
                        destination: m.destination.unwrap_or_default(),
                    })
                })
                .collect();

            result.push(ContainerMountInfo {
                container_name,
                pod_name,
                namespace,
                mounts,
            });
        }

        Ok(result)
    }

    /// Force-remove all containers with a name prefix (parallel)
    pub async fn cleanup_containers_by_prefix(&self, prefix: &str) -> Result<()> {
        let containers = self.list_containers_by_prefix(prefix).await?;

        if containers.is_empty() {
            return Ok(());
        }

        // Force-remove all containers in parallel (no need to stop first)
        let futures: Vec<_> = containers
            .into_iter()
            .map(|container| async move {
                let _ = self.remove_container(&container, true).await;
            })
            .collect();

        futures_util::future::join_all(futures).await;

        Ok(())
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

    /// Remove a Docker volume
    pub async fn remove_volume(&self, name: &str) -> Result<()> {
        // Ignore errors - volume might not exist
        let _ = self
            .client
            .remove_volume(name, Some(RemoveVolumeOptions { force: true }))
            .await;
        Ok(())
    }

    /// Check if a Docker volume exists
    #[allow(dead_code)]
    pub async fn volume_exists(&self, name: &str) -> bool {
        self.client.inspect_volume(name).await.is_ok()
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
    pub async fn get_pod_image_architectures(&self) -> HashMap<String, String> {
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

    /// List images matching a pattern (simple prefix match)
    pub async fn list_images_by_pattern(&self, pattern: &str) -> Result<Vec<String>> {
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

    /// Run an ephemeral container (like `docker run --rm`)
    /// Useful for running one-off commands with specific mounts
    #[allow(dead_code)]
    pub async fn run_ephemeral_container(
        &self,
        image: &str,
        command: &[&str],
        volumes: &[(&str, &str)],
    ) -> Result<()> {
        // Ensure image exists
        if !self.image_exists(image).await {
            self.pull_image(image).await?;
        }

        let container_name = format!("ephemeral-{}", std::process::id());

        let binds: Vec<String> = volumes
            .iter()
            .map(|(src, dst)| format!("{}:{}", src, dst))
            .collect();

        let host_config = HostConfig {
            binds: if binds.is_empty() { None } else { Some(binds) },
            auto_remove: Some(true),
            ..Default::default()
        };

        let container_config = ContainerCreateBody {
            image: Some(image.to_string()),
            cmd: Some(command.iter().map(|s| s.to_string()).collect()),
            host_config: Some(host_config),
            ..Default::default()
        };

        self.client
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    ..Default::default()
                }),
                container_config,
            )
            .await
            .with_context(|| "Failed to create ephemeral container")?;

        self.client
            .start_container(&container_name, None::<StartContainerOptions>)
            .await
            .with_context(|| "Failed to start ephemeral container")?;

        let mut wait_stream = self
            .client
            .wait_container(&container_name, None::<WaitContainerOptions>);
        while let Some(result) = wait_stream.next().await {
            match result {
                Ok(response) => {
                    if response.status_code != 0 {
                        // Try to remove container in case auto_remove didn't work
                        let _ = self.remove_container(&container_name, true).await;
                        return Err(anyhow!(
                            "Ephemeral container exited with code {}",
                            response.status_code
                        ));
                    }
                }
                Err(e) => {
                    // Container might have been auto-removed, check if it's a "not found" error
                    let err_str = e.to_string();
                    if !err_str.contains("No such container") && !err_str.contains("not found") {
                        let _ = self.remove_container(&container_name, true).await;
                        return Err(anyhow!("Error waiting for ephemeral container: {}", e));
                    }
                }
            }
        }

        let _ = self.remove_container(&container_name, true).await;

        Ok(())
    }

    /// Run a new container
    pub async fn run_container(&self, config: &ContainerRunConfig) -> Result<()> {
        let mut exposed_ports: Vec<String> = Vec::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();

        for (host, container) in &config.ports {
            let container_port = format!("{}/tcp", container);
            exposed_ports.push(container_port.clone());
            port_bindings.insert(
                container_port,
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some(host.to_string()),
                }]),
            );
        }

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
            self.client
                .start_container(&config.name, None::<StartContainerOptions>)
                .await
                .map_err(|e| {
                    let err_msg = e.to_string().to_lowercase();
                    let is_port_error = err_msg.contains("bind")
                        || err_msg.contains("address already in use")
                        || err_msg.contains("permission denied")
                        || err_msg.contains("port is already allocated");

                    if is_port_error {
                        let privileged_ports: Vec<String> = config
                            .ports
                            .iter()
                            .filter(|(host, _)| *host < 1024)
                            .map(|(host, container)| format!("{}:{}", host, container))
                            .collect();

                        let mut context_msg = format!(
                            "Failed to start container '{}': port binding error.\n\n\
                             Error: {}\n\n",
                            config.name, e
                        );

                        if !privileged_ports.is_empty() {
                            context_msg.push_str(&format!(
                                "Privileged ports configured: {}\n\n",
                                privileged_ports.join(", ")
                            ));
                        }

                        if cfg!(target_os = "macos") {
                            context_msg.push_str(
                                "On macOS, binding to ports below 1024 (like 80/443) requires \
                                 Docker Desktop's privileged port mapping.\n\n\
                                 To fix:\n\
                                 - Open Docker Desktop → Settings → Advanced → \
                                   Enable privileged port mapping\n\
                                 - Or configure alternative ports in k3dev.yml:\n\
                                   http_port: 8080\n\
                                   https_port: 8443\n",
                            );
                        } else {
                            context_msg.push_str(
                                "Possible causes:\n\
                                 - Another process is already using the port(s)\n\
                                 - Insufficient permissions to bind to ports below 1024\n\n\
                                 To fix, configure alternative ports in k3dev.yml:\n\
                                   http_port: 8080\n\
                                   https_port: 8443\n",
                            );
                        }

                        anyhow!(context_msg)
                    } else {
                        anyhow!(e).context(format!("Failed to start container {}", config.name))
                    }
                })?;
        }

        Ok(())
    }
}

/// Container with parsed pod info and volume mounts
pub struct ContainerMountInfo {
    #[allow(dead_code)]
    pub container_name: String,
    pub pod_name: String,
    #[allow(dead_code)]
    pub namespace: String,
    pub mounts: Vec<MountSource>,
}

/// A single mount source/destination pair from a container
pub struct MountSource {
    pub source: String,
    #[allow(dead_code)]
    pub destination: String,
}

/// Configuration for running a Docker container
#[derive(Debug, Clone, Default)]
pub struct ContainerRunConfig {
    pub name: String,
    pub hostname: Option<String>,
    pub image: String,
    pub detach: bool,
    pub privileged: bool,
    pub ports: Vec<(u16, u16)>,
    pub volumes: Vec<(String, String, String)>, // (src, dst, options)
    pub env: Vec<(String, String)>,
    pub network: Option<String>,
    pub cgroupns_host: bool,
    pub pid_host: bool,
    pub entrypoint: Option<String>,
    pub command: Option<Vec<String>>,
    /// Security options (e.g., "apparmor=unconfined", "seccomp=unconfined")
    pub security_opt: Vec<String>,
}
