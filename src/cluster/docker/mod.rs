//! Docker container and network management
//!
//! This module provides Docker operations for k3dev:
//! - Container lifecycle (create, start, stop, remove)
//! - Network and volume management
//! - Image operations (pull, commit, remove)
//! - Command execution in containers

#![allow(deprecated)]

mod stats;

pub use stats::{ContainerStats, ResourceStats};

use anyhow::{anyhow, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, DownloadFromContainerOptions, InspectContainerOptions,
    ListContainersOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::models::{
    HostConfig, HostConfigCgroupnsModeEnum, Mount, MountBindOptions,
    MountBindOptionsPropagationEnum, MountTypeEnum, PortBinding,
};
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
use bollard::volume::{CreateVolumeOptions, RemoveVolumeOptions};
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::HashMap;
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
        let client = Docker::connect_with_unix(
            &socket_path.to_string_lossy(),
            120,
            bollard::API_DEFAULT_VERSION,
        )
        .with_context(|| format!("Failed to connect to Docker at {:?}", socket_path))?;

        Ok(Self {
            socket_path,
            client,
        })
    }

    /// Check if Docker is accessible
    pub async fn is_accessible(&self) -> bool {
        self.client.ping().await.is_ok()
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
            .start_container(name, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("Failed to start container {}", name))
    }

    /// Stop a running container
    pub async fn stop_container(&self, name: &str) -> Result<()> {
        self.client
            .stop_container(name, Some(StopContainerOptions { t: 10 }))
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

        let output = self
            .client
            .start_exec(&exec.id, Some(StartExecOptions::default()))
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
        let mut stream = self
            .client
            .download_from_container(container, Some(DownloadFromContainerOptions { path: src }));

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
        filters.insert("name", vec![prefix]);

        let containers = self
            .client
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
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

    /// Stop and remove all containers with a name prefix
    pub async fn cleanup_containers_by_prefix(&self, prefix: &str) -> Result<()> {
        let containers = self.list_containers_by_prefix(prefix).await?;

        for container in containers {
            let _ = self.stop_container(&container).await;
            let _ = self.remove_container(&container, true).await;
        }

        Ok(())
    }

    // === Network Operations ===

    /// Create a Docker network
    pub async fn create_network(&self, name: &str) -> Result<()> {
        // Check if network exists
        if self
            .client
            .inspect_network(name, None::<InspectNetworkOptions<String>>)
            .await
            .is_ok()
        {
            return Ok(()); // Already exists
        }

        self.client
            .create_network(CreateNetworkOptions {
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
            .create_volume(CreateVolumeOptions {
                name: name.to_string(),
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
            from_image: image,
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

    /// Commit a running container to a new image
    pub async fn commit_container(
        &self,
        container: &str,
        image: &str,
        labels: HashMap<String, String>,
    ) -> Result<()> {
        use bollard::image::CommitContainerOptions;

        let options = CommitContainerOptions {
            container,
            repo: image,
            tag: "",
            comment: "k3dev snapshot",
            author: "k3dev",
            pause: false, // Don't pause container during commit
            changes: None,
        };

        let config = Config::<String> {
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
        use bollard::image::ListImagesOptions;

        let options = Some(ListImagesOptions::<String> {
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
        use bollard::image::RemoveImageOptions;

        let options = Some(RemoveImageOptions {
            force: true,
            noprune: false,
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

        let container_config = Config {
            image: Some(image.to_string()),
            cmd: Some(command.iter().map(|s| s.to_string()).collect()),
            host_config: Some(host_config),
            ..Default::default()
        };

        self.client
            .create_container(
                Some(CreateContainerOptions {
                    name: container_name.clone(),
                    platform: None,
                }),
                container_config,
            )
            .await
            .with_context(|| "Failed to create ephemeral container")?;

        self.client
            .start_container(&container_name, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| "Failed to start ephemeral container")?;

        let mut wait_stream = self.client.wait_container(
            &container_name,
            None::<bollard::container::WaitContainerOptions<String>>,
        );
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
        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();

        for (host, container) in &config.ports {
            let container_port = format!("{}/tcp", container);
            exposed_ports.insert(container_port.clone(), HashMap::new());
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
                Some(HostConfigCgroupnsModeEnum::HOST)
            } else {
                None
            },
            pid_mode: if config.pid_host {
                Some("host".to_string())
            } else {
                None
            },
            ..Default::default()
        };

        let container_config = Config {
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
                    name: config.name.clone(),
                    platform: None,
                }),
                container_config,
            )
            .await
            .with_context(|| format!("Failed to create container {}", config.name))?;

        if config.detach {
            self.client
                .start_container(&config.name, None::<StartContainerOptions<String>>)
                .await
                .with_context(|| format!("Failed to start container {}", config.name))?;
        }

        Ok(())
    }
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
}
