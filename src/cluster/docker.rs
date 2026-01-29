#![allow(deprecated)]

use anyhow::{anyhow, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, DownloadFromContainerOptions, InspectContainerOptions,
    ListContainersOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
    UploadToContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::models::{
    HostConfig, HostConfigCgroupnsModeEnum, Mount, MountBindOptions,
    MountBindOptionsPropagationEnum, MountTypeEnum, PortBinding,
};
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
use bollard::volume::{CreateVolumeOptions, RemoveVolumeOptions};
use bollard::{body_full, Docker};
use futures_util::StreamExt;
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

// === CPU Measurement Constants ===

/// Minimum time delta (in microseconds) between readings for reliable CPU measurement
/// Readings with smaller deltas are too noisy and will use the previous value
const MIN_CPU_SAMPLE_DELTA_USEC: f64 = 100_000.0; // 100ms

/// Maximum reasonable CPU percentage (64 cores worth)
/// Higher values are likely measurement errors
const MAX_CPU_PERCENT: f64 = 6400.0;

/// Spike detection threshold - if CPU jumps by more than this in one reading, it's suspicious
const CPU_SPIKE_THRESHOLD_PERCENT: f64 = 300.0;

/// Minimum CPU percentage to consider a spike suspicious (low values jumping around is normal)
const CPU_SPIKE_MIN_VALUE_PERCENT: f64 = 500.0;

/// Millicores per core for CPU limit calculations
const MILLICORES_PER_CORE: f64 = 1000.0;

/// Cached CPU stats for delta calculation between refreshes (cgroups v2)
#[derive(Debug, Clone, Default)]
struct CachedCpuStats {
    /// CPU usage in microseconds (from cgroups cpu.stat usage_usec)
    usage_usec: u64,
    /// Timestamp when this was recorded (for delta calculation)
    timestamp_usec: u64,
    /// Previous CPU percentage (for spike detection/smoothing)
    prev_cpu_percent: f64,
}

/// Global CPU cache that persists across DockerManager instances
static CPU_CACHE: Lazy<RwLock<HashMap<String, CachedCpuStats>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Aggregated resource statistics from Docker containers
#[derive(Debug, Clone, Default)]
pub struct ResourceStats {
    pub cpu_percent: f64,
    pub memory_used_mb: f64,
    pub memory_total_mb: f64,
    pub net_rx_mb: f64,
    pub net_tx_mb: f64,
}

impl ResourceStats {
    pub fn memory_percent(&self) -> f64 {
        if self.memory_total_mb > 0.0 {
            (self.memory_used_mb / self.memory_total_mb) * 100.0
        } else {
            0.0
        }
    }
}

/// Stats for a single container
#[derive(Debug, Clone, Default)]
pub struct ContainerStats {
    pub name: String,
    pub namespace: String,
    pub cpu_percent: f64,
    pub cpu_limit_millicores: f64,
    pub memory_used_mb: f64,
    pub memory_limit_mb: f64,
    pub status: String,
}

/// Docker container and network management
pub struct DockerManager {
    #[allow(dead_code)]
    socket_path: PathBuf,
    client: Docker,
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

    /// Copy a file from one container to another with rename support
    /// src_path: full path to source file (e.g., "/usr/bin/socat1")
    /// dst_path: full path including filename (e.g., "/usr/local/bin/socat")
    pub async fn copy_file_between_containers(
        &self,
        src_container: &str,
        src_path: &str,
        dst_container: &str,
        dst_path: &str,
    ) -> Result<()> {
        use std::io::{Cursor, Read};

        // Download from source as tar stream
        let mut stream = self.client.download_from_container(
            src_container,
            Some(DownloadFromContainerOptions { path: src_path }),
        );

        // Collect the tar data
        let mut tar_data = Vec::new();
        while let Some(chunk) = stream.next().await {
            let data = chunk.context("Failed to download from source container")?;
            tar_data.extend_from_slice(&data);
        }

        // Parse destination path
        let dst_path_obj = std::path::Path::new(dst_path);
        let dst_dir = dst_path_obj
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        let dst_filename = dst_path_obj
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("Invalid destination path: {}", dst_path))?;

        // Read the file content from the tar archive and create a new tar with renamed file
        let mut archive = tar::Archive::new(Cursor::new(&tar_data));
        let mut new_tar = tar::Builder::new(Vec::new());

        // Only copy the first file from the archive
        if let Some(entry) = archive
            .entries()
            .context("Failed to read tar entries")?
            .next()
        {
            let mut entry = entry.context("Failed to read tar entry")?;
            let mut content = Vec::new();
            entry.read_to_end(&mut content)?;

            // Create new header with the destination filename
            let mut header = tar::Header::new_gnu();
            header.set_path(&dst_filename)?;
            header.set_size(content.len() as u64);
            header.set_mode(0o755); // Executable
            header.set_cksum();

            new_tar.append(&header, Cursor::new(content))?;
        }

        let new_tar_data = new_tar.into_inner().context("Failed to finalize tar")?;

        // Upload to destination container (to the parent directory)
        let upload_opts = UploadToContainerOptions {
            path: dst_dir,
            ..Default::default()
        };
        self.client
            .upload_to_container(
                dst_container,
                Some(upload_opts),
                body_full(new_tar_data.into()),
            )
            .await
            .context("Failed to upload to destination container")?;

        Ok(())
    }

    /// Upload raw file content to a container
    pub async fn upload_file_content(
        &self,
        container: &str,
        dst_path: &str,
        content: &[u8],
        mode: u32,
    ) -> Result<()> {
        use std::io::Cursor;

        let dst_path_obj = std::path::Path::new(dst_path);
        let dst_dir = dst_path_obj
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        let dst_filename = dst_path_obj
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("Invalid destination path: {}", dst_path))?;

        // Create tar archive with the file
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path(&dst_filename)?;
        header.set_size(content.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        tar_builder.append(&header, Cursor::new(content))?;
        let tar_data = tar_builder.into_inner().context("Failed to create tar")?;

        // Upload to container
        let upload_opts = UploadToContainerOptions {
            path: dst_dir,
            ..Default::default()
        };
        self.client
            .upload_to_container(container, Some(upload_opts), body_full(tar_data.into()))
            .await
            .context("Failed to upload file to container")?;

        Ok(())
    }

    /// Create a container without starting it (useful for file extraction)
    pub async fn create_container_stopped(&self, name: &str, image: &str) -> Result<()> {
        // Ensure image exists
        if !self.image_exists(image).await {
            self.pull_image(image).await?;
        }

        let container_config = Config {
            image: Some(image.to_string()),
            cmd: Some(vec!["true".to_string()]), // Minimal command
            ..Default::default()
        };

        self.client
            .create_container(
                Some(CreateContainerOptions {
                    name: name.to_string(),
                    platform: None,
                }),
                container_config,
            )
            .await
            .with_context(|| format!("Failed to create container {}", name))?;

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

    /// Get aggregated resource stats from all cluster containers (main k3s container + k8s workloads)
    /// Uses cgroups v2 for fast stats reading
    pub async fn get_container_stats(&self, prefix: &str) -> Result<ResourceStats> {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Get the main k3s container
        let mut containers = self.list_containers_by_prefix(prefix).await?;

        // Also get all k8s workload containers
        let k8s_containers = self.list_containers_by_prefix("k8s_").await?;
        containers.extend(k8s_containers);

        // Deduplicate
        let containers: Vec<String> = containers
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        if containers.is_empty() {
            return Ok(ResourceStats::default());
        }

        let now_usec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let num_cpus = num_cpus::get() as f64;

        let mut stats = ResourceStats::default();
        let mut memory_total_set = false;

        for container in &containers {
            // Get full container ID
            let full_id = match self.get_container_id(container).await {
                Some(id) => id,
                None => continue,
            };

            // Find cgroup path
            let cgroup_path = match find_container_cgroup(&full_id) {
                Some(p) => p,
                None => continue,
            };

            // Read stats from cgroups (ignore cpu_limit for aggregate stats)
            let (cpu, _cpu_limit, mem_used, mem_limit) =
                self.read_cgroup_stats(&cgroup_path, &full_id, now_usec, num_cpus);

            stats.cpu_percent += cpu;
            stats.memory_used_mb += mem_used;
            if !memory_total_set && mem_limit > 0.0 {
                stats.memory_total_mb = mem_limit;
                memory_total_set = true;
            }
            // Note: Network stats not available via cgroups, would need /proc/net or docker API
        }

        Ok(stats)
    }

    /// Get per-pod stats using cgroups v2 (much faster than Docker API)
    /// Reads directly from /sys/fs/cgroup/kubepods for resource stats
    pub async fn get_pod_stats(&self, prefix: &str) -> Result<Vec<ContainerStats>> {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Get container list from Docker (fast - just listing, no stats)
        let mut containers = self.list_containers_by_prefix(prefix).await?;
        let k8s_containers = self.list_containers_by_prefix("k8s_").await?;
        containers.extend(k8s_containers);

        // Deduplicate and filter
        let containers: Vec<String> = containers
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|c| !c.starts_with("k8s_POD_"))
            .collect();

        if containers.is_empty() {
            return Ok(Vec::new());
        }

        // Get current timestamp for CPU delta calculation
        let now_usec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);

        // Get number of CPUs for percentage calculation
        let num_cpus = num_cpus::get() as f64;

        // Get full container IDs and their cgroup paths
        let mut pod_stats: HashMap<String, ContainerStats> = HashMap::new();

        for container_name in &containers {
            // Get full container ID from Docker
            let full_id = match self.get_container_id(container_name).await {
                Some(id) => id,
                None => continue,
            };

            // Find cgroup path for this container
            let cgroup_path = find_container_cgroup(&full_id);
            let cgroup_path = match cgroup_path {
                Some(p) => p,
                None => continue,
            };

            // Read stats from cgroups (very fast - just file reads)
            let (cpu_percent, cpu_limit_millicores, memory_used_mb, memory_limit_mb) =
                self.read_cgroup_stats(&cgroup_path, &full_id, now_usec, num_cpus);

            // Extract pod name and namespace from container name
            let (pod_name, namespace) = if container_name.starts_with("k8s_") {
                let parts: Vec<&str> = container_name.split('_').collect();
                if parts.len() >= 4 {
                    (parts[2].to_string(), parts[3].to_string())
                } else if parts.len() >= 3 {
                    (parts[2].to_string(), "default".to_string())
                } else {
                    (container_name.clone(), "default".to_string())
                }
            } else {
                (container_name.clone(), "system".to_string())
            };

            // Aggregate stats for this pod
            let key = format!("{}/{}", namespace, pod_name);
            pod_stats
                .entry(key)
                .and_modify(|stats| {
                    stats.cpu_percent += cpu_percent;
                    // CPU limit: if ANY container has no limit (0), pod is unlimited
                    // Otherwise sum the limits
                    if stats.cpu_limit_millicores > 0.0 && cpu_limit_millicores > 0.0 {
                        stats.cpu_limit_millicores += cpu_limit_millicores;
                    } else {
                        // One container has no limit = pod is effectively unlimited
                        stats.cpu_limit_millicores = 0.0;
                    }
                    stats.memory_used_mb += memory_used_mb;
                    // Memory limit: if ANY container has no limit (0), pod is unlimited
                    if stats.memory_limit_mb > 0.0 && memory_limit_mb > 0.0 {
                        stats.memory_limit_mb += memory_limit_mb;
                    } else {
                        stats.memory_limit_mb = 0.0;
                    }
                })
                .or_insert(ContainerStats {
                    name: pod_name,
                    namespace,
                    cpu_percent,
                    cpu_limit_millicores,
                    memory_used_mb,
                    memory_limit_mb,
                    status: "running".to_string(),
                });
        }

        // Final sanity check: cap each pod's CPU to system max
        let max_cpu_percent = num_cpus * 100.0;
        let mut stats_list: Vec<ContainerStats> = pod_stats
            .into_values()
            .map(|mut s| {
                s.cpu_percent = s.cpu_percent.min(max_cpu_percent);
                s
            })
            .collect();
        stats_list.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(stats_list)
    }

    /// Get full container ID from container name
    async fn get_container_id(&self, name: &str) -> Option<String> {
        let inspect = self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()?;
        inspect.id
    }

    /// Read stats from cgroup files (very fast)
    /// Returns (cpu_percent, cpu_limit_millicores, memory_used_mb, memory_limit_mb)
    fn read_cgroup_stats(
        &self,
        cgroup_path: &std::path::Path,
        container_id: &str,
        now_usec: u64,
        num_cpus: f64,
    ) -> (f64, f64, f64, f64) {
        use std::fs;

        // Read CPU usage from cpu.stat
        let cpu_stat_path = cgroup_path.join("cpu.stat");
        let usage_usec = fs::read_to_string(&cpu_stat_path)
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find(|line| line.starts_with("usage_usec"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0);

        // Read CPU limit from cpu.max
        // Format: "$MAX $PERIOD" e.g., "100000 100000" means 1 core
        // "max 100000" means no limit
        let cpu_max_path = cgroup_path.join("cpu.max");
        let cpu_limit_millicores = fs::read_to_string(&cpu_max_path)
            .ok()
            .and_then(|s| {
                let parts: Vec<&str> = s.split_whitespace().collect();
                if parts.len() >= 2 {
                    let quota = parts[0];
                    let period = parts[1].parse::<f64>().ok()?;
                    if quota == "max" || period <= 0.0 {
                        None // No limit
                    } else {
                        let quota_val = quota.parse::<f64>().ok()?;
                        // millicores = (quota / period) * 1000
                        Some((quota_val / period) * MILLICORES_PER_CORE)
                    }
                } else {
                    None
                }
            })
            .unwrap_or(0.0); // 0 means no limit

        // Calculate CPU percentage using global cached delta
        let cpu_percent = {
            let prev = CPU_CACHE
                .read()
                .ok()
                .and_then(|cache| cache.get(container_id).cloned());

            let (percent, prev_percent) = if let Some(prev) = prev {
                let usage_delta = usage_usec.saturating_sub(prev.usage_usec) as f64;
                let time_delta = now_usec.saturating_sub(prev.timestamp_usec) as f64;

                let raw_percent = if time_delta >= MIN_CPU_SAMPLE_DELTA_USEC {
                    // usage_usec is per-CPU, time_delta is wall clock
                    // CPU% = (usage_delta / time_delta) * 100
                    let calc = (usage_delta / time_delta) * 100.0;
                    calc.min(MAX_CPU_PERCENT)
                } else {
                    // Time delta too small for reliable measurement, use previous value
                    prev.prev_cpu_percent
                };

                // Spike detection: reject suspiciously large jumps
                let final_percent = if prev.prev_cpu_percent > 0.0
                    && raw_percent > prev.prev_cpu_percent + CPU_SPIKE_THRESHOLD_PERCENT
                    && raw_percent > CPU_SPIKE_MIN_VALUE_PERCENT
                {
                    // Likely a measurement artifact, keep previous value
                    prev.prev_cpu_percent
                } else {
                    raw_percent
                };

                (final_percent, prev.prev_cpu_percent)
            } else {
                (0.0, 0.0)
            };

            // Update global cache with both usage and calculated percent
            if let Ok(mut cache) = CPU_CACHE.write() {
                cache.insert(
                    container_id.to_string(),
                    CachedCpuStats {
                        usage_usec,
                        timestamp_usec: now_usec,
                        prev_cpu_percent: if percent > 0.0 { percent } else { prev_percent },
                    },
                );
            }

            percent.min(100.0 * num_cpus) // Cap at max possible
        };

        // Read memory stats
        let mem_current = cgroup_path.join("memory.current");
        let mem_max = cgroup_path.join("memory.max");

        let memory_used_mb = fs::read_to_string(&mem_current)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|b| b as f64 / (1024.0 * 1024.0))
            .unwrap_or(0.0);

        let memory_limit_mb = fs::read_to_string(&mem_max)
            .ok()
            .and_then(|s| {
                let trimmed = s.trim();
                if trimmed == "max" {
                    None // No limit set
                } else {
                    trimmed.parse::<u64>().ok()
                }
            })
            .map(|b| b as f64 / (1024.0 * 1024.0))
            .unwrap_or(0.0); // 0 means no limit (will be detected as unlimited)

        (
            cpu_percent,
            cpu_limit_millicores,
            memory_used_mb,
            memory_limit_mb,
        )
    }

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

        // Generate unique container name
        let container_name = format!("ephemeral-{}", std::process::id());

        // Build volume bindings
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

        // Create the container
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

        // Start the container
        self.client
            .start_container(&container_name, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| "Failed to start ephemeral container")?;

        // Wait for the container to finish
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

        // Try to remove container in case auto_remove didn't work
        let _ = self.remove_container(&container_name, true).await;

        Ok(())
    }

    /// Run a new container
    pub async fn run_container(&self, config: &ContainerRunConfig) -> Result<()> {
        // Build exposed ports and port bindings
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

        // Build environment variables
        let env: Vec<String> = config
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // Build volume bindings and mounts
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

        // Build host config
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

        // Build container config
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

        // Create the container
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

        // Start the container if detached
        if config.detach {
            self.client
                .start_container(&config.name, None::<StartContainerOptions<String>>)
                .await
                .with_context(|| format!("Failed to start container {}", config.name))?;
        }

        Ok(())
    }
}

/// Find cgroup path for a container ID under /sys/fs/cgroup/kubepods
/// Searches recursively through kubepods hierarchy (including QoS classes)
fn find_container_cgroup(container_id: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let kubepods_base = PathBuf::from("/sys/fs/cgroup/kubepods");
    if !kubepods_base.exists() {
        return None;
    }

    // Search function to find container cgroup recursively
    fn search_dir(dir: &std::path::Path, container_id: &str) -> Option<PathBuf> {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name()?.to_str()?;

                    // Check if this directory name matches the container ID
                    if name == container_id {
                        return Some(path);
                    }

                    // Recurse into pod directories and QoS class directories
                    if name.starts_with("pod")
                        || name == "burstable"
                        || name == "besteffort"
                        || name == "guaranteed"
                    {
                        if let Some(found) = search_dir(&path, container_id) {
                            return Some(found);
                        }
                    }
                }
            }
        }
        None
    }

    search_dir(&kubepods_base, container_id)
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
