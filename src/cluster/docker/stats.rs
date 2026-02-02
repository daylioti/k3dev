//! Container resource statistics
//!
//! This module provides container resource monitoring:
//! - Aggregated cluster resource stats
//! - Per-pod stats using cgroups v2
//! - CPU delta calculation with spike detection

use anyhow::Result;
use bollard::container::InspectContainerOptions;
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use super::DockerManager;

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

impl DockerManager {
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
                read_cgroup_stats(&cgroup_path, &full_id, now_usec, num_cpus);

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
                read_cgroup_stats(&cgroup_path, &full_id, now_usec, num_cpus);

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
    pub(super) async fn get_container_id(&self, name: &str) -> Option<String> {
        let inspect = self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()?;
        inspect.id
    }
}

/// Read stats from cgroup files (very fast)
/// Returns (cpu_percent, cpu_limit_millicores, memory_used_mb, memory_limit_mb)
fn read_cgroup_stats(
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
