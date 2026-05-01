//! Container resource statistics
//!
//! This module provides container resource monitoring:
//! - Aggregated cluster resource stats
//! - Per-pod stats using cgroups v2
//! - CPU delta calculation with spike detection

use anyhow::{Context, Result};
use bollard::query_parameters::InspectContainerOptions;
use once_cell::sync::Lazy;
use serde::Deserialize;
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

/// Stats for a single container
#[derive(Debug, Clone, Default)]
pub struct ContainerStats {
    pub name: String,
    pub namespace: String,
    pub cpu_percent: f64,
    pub cpu_limit_millicores: f64,
    pub memory_used_mb: f64,
    pub memory_limit_mb: f64,
}

impl DockerManager {
    /// Get per-pod stats using cgroups v2 (much faster than Docker API)
    /// Reads directly from /sys/fs/cgroup/kubepods for resource stats
    pub async fn get_pod_stats(&self, prefix: &str) -> Result<Vec<ContainerStats>> {
        use crate::cluster::platform::PlatformInfo;
        use std::time::{SystemTime, UNIX_EPOCH};

        // Remote Docker: local cgroups don't reflect remote containers — use agent instead
        if PlatformInfo::is_docker_remote() {
            anyhow::bail!("Host-side cgroup stats unavailable with remote Docker");
        }

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

/// Detected cgroup version for the system
#[derive(Debug, Clone, Copy, PartialEq)]
enum CgroupVersion {
    V1,
    V2,
}

/// Detect whether the system uses cgroup v1 or v2
fn detect_cgroup_version() -> CgroupVersion {
    use std::path::Path;
    // cgroup v2 unified hierarchy has cgroup.controllers at the root
    if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        CgroupVersion::V2
    } else {
        CgroupVersion::V1
    }
}

/// Cached cgroup version detection (called frequently during stats reads)
static CGROUP_VERSION: Lazy<CgroupVersion> = Lazy::new(detect_cgroup_version);

/// Read stats from cgroup files (very fast)
/// Supports both cgroup v1 and v2
/// Returns (cpu_percent, cpu_limit_millicores, memory_used_mb, memory_limit_mb)
fn read_cgroup_stats(
    cgroup_path: &std::path::Path,
    container_id: &str,
    now_usec: u64,
    num_cpus: f64,
) -> (f64, f64, f64, f64) {
    match *CGROUP_VERSION {
        CgroupVersion::V2 => read_cgroup_v2_stats(cgroup_path, container_id, now_usec, num_cpus),
        CgroupVersion::V1 => read_cgroup_v1_stats(cgroup_path, container_id, now_usec, num_cpus),
    }
}

/// Read stats from cgroup v2 files
fn read_cgroup_v2_stats(
    cgroup_path: &std::path::Path,
    container_id: &str,
    now_usec: u64,
    num_cpus: f64,
) -> (f64, f64, f64, f64) {
    use std::fs;

    // Read CPU usage from cpu.stat (usage_usec field)
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
                    Some((quota_val / period) * MILLICORES_PER_CORE)
                }
            } else {
                None
            }
        })
        .unwrap_or(0.0);

    let cpu_percent = calculate_cpu_percent(container_id, usage_usec, now_usec, num_cpus);

    // Read memory stats
    let memory_used_mb = fs::read_to_string(cgroup_path.join("memory.current"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|b| b as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);

    let memory_limit_mb = fs::read_to_string(cgroup_path.join("memory.max"))
        .ok()
        .and_then(|s| {
            let trimmed = s.trim();
            if trimmed == "max" {
                None
            } else {
                trimmed.parse::<u64>().ok()
            }
        })
        .map(|b| b as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);

    (
        cpu_percent,
        cpu_limit_millicores,
        memory_used_mb,
        memory_limit_mb,
    )
}

/// Read stats from cgroup v1 files
/// v1 splits controllers: cpu/cpuacct and memory are in separate hierarchies
/// The `cgroup_path` here is the cpu controller path; memory path is derived
fn read_cgroup_v1_stats(
    cgroup_path: &std::path::Path,
    container_id: &str,
    now_usec: u64,
    num_cpus: f64,
) -> (f64, f64, f64, f64) {
    use std::fs;

    // CPU usage: cpuacct.usage is in nanoseconds (convert to microseconds)
    let usage_usec = fs::read_to_string(cgroup_path.join("cpuacct.usage"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|ns| ns / 1000) // nanoseconds → microseconds
        .unwrap_or(0);

    // CPU limit: cpu.cfs_quota_us / cpu.cfs_period_us
    // quota of -1 means no limit
    let cpu_quota = fs::read_to_string(cgroup_path.join("cpu.cfs_quota_us"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(-1);
    let cpu_period = fs::read_to_string(cgroup_path.join("cpu.cfs_period_us"))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(100_000.0);

    let cpu_limit_millicores = if cpu_quota > 0 && cpu_period > 0.0 {
        (cpu_quota as f64 / cpu_period) * MILLICORES_PER_CORE
    } else {
        0.0
    };

    let cpu_percent = calculate_cpu_percent(container_id, usage_usec, now_usec, num_cpus);

    // Memory: derive memory cgroup path from cpu path
    // /sys/fs/cgroup/cpu,cpuacct/kubepods/... → /sys/fs/cgroup/memory/kubepods/...
    let memory_path = derive_v1_memory_path(cgroup_path);

    let memory_used_mb = memory_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p.join("memory.usage_in_bytes")).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|b| b as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);

    let memory_limit_mb = memory_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p.join("memory.limit_in_bytes")).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .and_then(|b| {
            // v1 uses a very large number instead of "max" for no-limit
            // Typically PAGE_COUNTER_MAX * PAGE_SIZE, ~= 2^63 on 64-bit
            if b > (1u64 << 60) {
                None // Effectively unlimited
            } else {
                Some(b as f64 / (1024.0 * 1024.0))
            }
        })
        .unwrap_or(0.0);

    (
        cpu_percent,
        cpu_limit_millicores,
        memory_used_mb,
        memory_limit_mb,
    )
}

/// Derive the memory cgroup path from a cpu cgroup path for cgroup v1
/// e.g., /sys/fs/cgroup/cpu,cpuacct/kubepods/.../container_id
///     → /sys/fs/cgroup/memory/kubepods/.../container_id
fn derive_v1_memory_path(cpu_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let path_str = cpu_path.to_str()?;
    // Find the controller part and replace it with "memory"
    // Handles both /sys/fs/cgroup/cpu/... and /sys/fs/cgroup/cpu,cpuacct/...
    let memory_path = if path_str.contains("/cpu,cpuacct/") {
        path_str.replacen("/cpu,cpuacct/", "/memory/", 1)
    } else if path_str.contains("/cpu/") {
        path_str.replacen("/cpu/", "/memory/", 1)
    } else {
        return None;
    };
    let p = std::path::PathBuf::from(memory_path);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Calculate CPU percentage using global cached delta (shared between v1 and v2)
fn calculate_cpu_percent(container_id: &str, usage_usec: u64, now_usec: u64, num_cpus: f64) -> f64 {
    let prev = CPU_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.get(container_id).cloned());

    let (percent, prev_percent) = if let Some(prev) = prev {
        let usage_delta = usage_usec.saturating_sub(prev.usage_usec) as f64;
        let time_delta = now_usec.saturating_sub(prev.timestamp_usec) as f64;

        let raw_percent = if time_delta >= MIN_CPU_SAMPLE_DELTA_USEC {
            let calc = (usage_delta / time_delta) * 100.0;
            calc.min(MAX_CPU_PERCENT)
        } else {
            prev.prev_cpu_percent
        };

        // Spike detection: reject suspiciously large jumps
        let final_percent = if prev.prev_cpu_percent > 0.0
            && raw_percent > prev.prev_cpu_percent + CPU_SPIKE_THRESHOLD_PERCENT
            && raw_percent > CPU_SPIKE_MIN_VALUE_PERCENT
        {
            prev.prev_cpu_percent
        } else {
            raw_percent
        };

        (final_percent, prev.prev_cpu_percent)
    } else {
        (0.0, 0.0)
    };

    // Update global cache
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

    percent.min(100.0 * num_cpus)
}

/// Find cgroup path for a container ID
/// Supports both cgroup v2 (unified /sys/fs/cgroup/kubepods) and
/// cgroup v1 (split controllers like /sys/fs/cgroup/cpu,cpuacct/kubepods)
///
/// For rootless Docker, cgroups are nested under user slices instead of kubepods:
///   /sys/fs/cgroup/user.slice/user-<UID>.slice/user@<UID>.service/docker.service/...
/// When the standard kubepods path isn't found, this falls back to searching
/// rootless Docker cgroup paths using the current UID.
fn find_container_cgroup(container_id: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    match *CGROUP_VERSION {
        CgroupVersion::V2 => {
            // Standard path: kubepods (rootful Docker with k3s)
            let kubepods_base = PathBuf::from("/sys/fs/cgroup/kubepods");
            if kubepods_base.exists() {
                if let Some(found) = search_cgroup_dir(&kubepods_base, container_id) {
                    return Some(found);
                }
            }

            // Rootless Docker fallback: search under user slice cgroup paths
            if let Some(found) = find_rootless_cgroup_v2(container_id) {
                return Some(found);
            }

            None
        }
        CgroupVersion::V1 => {
            // v1: try cpu,cpuacct first (common combined mount), then cpu alone
            for controller in &["cpu,cpuacct", "cpu"] {
                let base = PathBuf::from(format!("/sys/fs/cgroup/{}/kubepods", controller));
                if base.exists() {
                    if let Some(found) = search_cgroup_dir(&base, container_id) {
                        return Some(found);
                    }
                }
            }

            // Rootless Docker fallback for v1: search under user slice
            if let Some(found) = find_rootless_cgroup_v1(container_id) {
                return Some(found);
            }

            None
        }
    }
}

/// Get current UID for rootless cgroup path construction
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .ok()
        .map(|m| m.uid())
        .or_else(|| std::fs::metadata(".").ok().map(|m| m.uid()))
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

/// Find a container's cgroup under rootless Docker paths (cgroup v2)
/// Rootless Docker with systemd driver places containers under:
///   /sys/fs/cgroup/user.slice/user-<UID>.slice/user@<UID>.service/docker.service/<container_id>
/// or with cgroupfs driver:
///   /sys/fs/cgroup/user.slice/user-<UID>.slice/user@<UID>.service/docker/<container_id>
/// K3s inside rootless Docker may also create kubepods under the user slice.
fn find_rootless_cgroup_v2(container_id: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let uid = current_uid()?;
    let user_service = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{}.slice/user@{}.service",
        uid, uid
    ));

    if !user_service.exists() {
        return None;
    }

    // Check docker.service/ (systemd cgroup driver)
    let docker_service = user_service.join("docker.service");
    if docker_service.exists() {
        // Direct container ID match
        let direct = docker_service.join(container_id);
        if direct.exists() {
            return Some(direct);
        }
        // Search recursively (kubepods may exist under docker.service)
        if let Some(found) = search_cgroup_dir_broad(&docker_service, container_id) {
            return Some(found);
        }
    }

    // Check docker/ (cgroupfs driver)
    let docker_dir = user_service.join("docker");
    if docker_dir.exists() {
        let direct = docker_dir.join(container_id);
        if direct.exists() {
            return Some(direct);
        }
        if let Some(found) = search_cgroup_dir_broad(&docker_dir, container_id) {
            return Some(found);
        }
    }

    None
}

/// Find a container's cgroup under rootless Docker paths (cgroup v1)
/// Rootless Docker v1 places containers under:
///   /sys/fs/cgroup/cpu,cpuacct/user.slice/user-<UID>.slice/docker/<container_id>
fn find_rootless_cgroup_v1(container_id: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let uid = current_uid()?;

    for controller in &["cpu,cpuacct", "cpu"] {
        // Rootless v1 with cgroupfs driver
        let base = PathBuf::from(format!(
            "/sys/fs/cgroup/{}/user.slice/user-{}.slice",
            controller, uid
        ));
        if base.exists() {
            // Search for container ID under docker/ or other subdirectories
            if let Some(found) = search_cgroup_dir_broad(&base, container_id) {
                return Some(found);
            }
        }
    }

    None
}

/// Broadly search for a container cgroup directory by container ID
/// Unlike `search_cgroup_dir`, this recurses into any subdirectory (not just kubepods-related ones)
/// to handle arbitrary rootless Docker cgroup hierarchies. Limited to 3 levels deep.
fn search_cgroup_dir_broad(
    dir: &std::path::Path,
    container_id: &str,
) -> Option<std::path::PathBuf> {
    search_cgroup_dir_broad_depth(dir, container_id, 0)
}

fn search_cgroup_dir_broad_depth(
    dir: &std::path::Path,
    container_id: &str,
    depth: u32,
) -> Option<std::path::PathBuf> {
    const MAX_DEPTH: u32 = 5;
    if depth >= MAX_DEPTH {
        return None;
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name()?.to_str()?;

                if name == container_id {
                    return Some(path);
                }

                // Recurse into subdirectories
                if let Some(found) = search_cgroup_dir_broad_depth(&path, container_id, depth + 1) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Recursively search for a container cgroup directory by container ID
fn search_cgroup_dir(dir: &std::path::Path, container_id: &str) -> Option<std::path::PathBuf> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name()?.to_str()?;

                if name == container_id {
                    return Some(path);
                }

                // Recurse into pod directories and QoS class directories
                if name.starts_with("pod")
                    || name == "burstable"
                    || name == "besteffort"
                    || name == "guaranteed"
                    // v1 with systemd driver uses docker-<id>.scope inside slices
                    || name.starts_with("kubepods")
                {
                    if let Some(found) = search_cgroup_dir(&path, container_id) {
                        return Some(found);
                    }
                }
            }
        }
    }
    None
}

// === Agent-based stats collection ===
// Runs k3dev-agent inside the k3s container via exec.
// One exec call replaces N Docker API calls + N cgroup reads.

/// Raw container stats from the agent JSON output
#[derive(Debug, Deserialize)]
struct AgentContainer {
    id: String,
    pod: String,
    ns: String,
    #[serde(rename = "cpu")]
    cpu_usec: u64,
    #[serde(rename = "cq")]
    cpu_quota: i64,
    #[serde(rename = "cp")]
    cpu_period: u64,
    #[serde(rename = "mem")]
    mem_current: u64,
    #[serde(rename = "ml")]
    mem_max: i64,
}

#[derive(Debug, Deserialize)]
struct AgentOutput {
    ts: u64,
    containers: Vec<AgentContainer>,
}

impl DockerManager {
    /// Get per-pod stats via the injected k3dev-agent binary.
    /// Falls back to the direct cgroup method if agent is unavailable.
    pub async fn get_pod_stats_via_agent(
        &self,
        k3s_container: &str,
    ) -> Result<Vec<ContainerStats>> {
        let json = self
            .exec_in_container(k3s_container, &["/usr/local/bin/k3dev-agent"])
            .await?;

        let agent = parse_agent_output(&json)?;
        let num_cpus = num_cpus::get() as f64;

        let mut pod_stats: HashMap<String, ContainerStats> = HashMap::new();

        for c in &agent.containers {
            // Skip unknown containers (no Docker name mapping)
            if c.pod == "_unknown" {
                continue;
            }

            // CPU limit in millicores
            let cpu_limit_millicores = if c.cpu_quota > 0 && c.cpu_period > 0 {
                (c.cpu_quota as f64 / c.cpu_period as f64) * MILLICORES_PER_CORE
            } else {
                0.0
            };

            // Memory
            let memory_used_mb = c.mem_current as f64 / (1024.0 * 1024.0);
            let memory_limit_mb = if c.mem_max > 0 {
                c.mem_max as f64 / (1024.0 * 1024.0)
            } else {
                0.0
            };

            // CPU delta using same global cache
            let cpu_percent = {
                let prev = CPU_CACHE
                    .read()
                    .ok()
                    .and_then(|cache| cache.get(&c.id).cloned());

                let (percent, prev_percent) = if let Some(prev) = prev {
                    let usage_delta = c.cpu_usec.saturating_sub(prev.usage_usec) as f64;
                    let time_delta = agent.ts.saturating_sub(prev.timestamp_usec) as f64;

                    let raw_percent = if time_delta >= MIN_CPU_SAMPLE_DELTA_USEC {
                        (usage_delta / time_delta) * 100.0
                    } else {
                        prev.prev_cpu_percent
                    };

                    // Spike detection
                    let final_percent = if prev.prev_cpu_percent > 0.0
                        && raw_percent > prev.prev_cpu_percent + CPU_SPIKE_THRESHOLD_PERCENT
                        && raw_percent > CPU_SPIKE_MIN_VALUE_PERCENT
                    {
                        prev.prev_cpu_percent
                    } else {
                        raw_percent.min(MAX_CPU_PERCENT)
                    };

                    (final_percent, prev.prev_cpu_percent)
                } else {
                    (0.0, 0.0)
                };

                if let Ok(mut cache) = CPU_CACHE.write() {
                    cache.insert(
                        c.id.clone(),
                        CachedCpuStats {
                            usage_usec: c.cpu_usec,
                            timestamp_usec: agent.ts,
                            prev_cpu_percent: if percent > 0.0 { percent } else { prev_percent },
                        },
                    );
                }

                percent.min(100.0 * num_cpus)
            };

            // Aggregate per pod (same logic as existing method)
            let key = format!("{}/{}", c.ns, c.pod);
            pod_stats
                .entry(key)
                .and_modify(|stats| {
                    stats.cpu_percent += cpu_percent;
                    if stats.cpu_limit_millicores > 0.0 && cpu_limit_millicores > 0.0 {
                        stats.cpu_limit_millicores += cpu_limit_millicores;
                    } else {
                        stats.cpu_limit_millicores = 0.0;
                    }
                    stats.memory_used_mb += memory_used_mb;
                    if stats.memory_limit_mb > 0.0 && memory_limit_mb > 0.0 {
                        stats.memory_limit_mb += memory_limit_mb;
                    } else {
                        stats.memory_limit_mb = 0.0;
                    }
                })
                .or_insert(ContainerStats {
                    name: c.pod.clone(),
                    namespace: c.ns.clone(),
                    cpu_percent,
                    cpu_limit_millicores,
                    memory_used_mb,
                    memory_limit_mb,
                });
        }

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
}

/// Parse the JSON output from k3dev-agent.
fn parse_agent_output(json: &str) -> Result<AgentOutput> {
    let json = json.trim();
    if json.is_empty() || json.starts_with("{\"error\"") {
        return Err(anyhow::anyhow!(
            "Agent returned error: {}",
            json.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(json).context("Failed to parse agent JSON output")
}
