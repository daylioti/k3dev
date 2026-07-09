//! Background refresh tasks
//!
//! This module contains all spawn_* methods for background data refresh.

use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::cluster::docker::pull_progress::monitor_image_pull;
use crate::cluster::{
    ClusterManager, ClusterStatus, DockerManager, IngressHealthChecker, IngressManager,
    PortForwardDetector,
};
use crate::commands::{capture_exec, check_visible, strip_ansi, trim_output};
use crate::config::{ExecutionTarget, VisibleCheck};
use crate::k8s::K8sClient;

use super::messages::{InfoBlockResult, InfoBlockStatus};
use super::{App, AppMessage};

/// Maximum concurrent manifest fetches across all pull monitors
static MANIFEST_SEMAPHORE: once_cell::sync::Lazy<Arc<Semaphore>> =
    once_cell::sync::Lazy::new(|| Arc::new(Semaphore::new(5)));

impl App {
    /// Whether the cluster is fully running (spawn_* helpers guard on this).
    fn cluster_is_running(&self) -> bool {
        matches!(self.cluster_status, ClusterStatus::Running)
    }

    /// Lazily build and cache a shared `DockerManager`, returning a clone of the
    /// cached `Arc`. Periodic refresh tasks reuse this instead of reconnecting
    /// to the Docker socket on every tick.
    pub(super) fn ensure_docker_manager(&mut self) -> Option<Arc<DockerManager>> {
        if self.docker_manager.is_none() {
            match DockerManager::from_default_socket() {
                Ok(m) => self.docker_manager = Some(Arc::new(m)),
                Err(e) => {
                    tracing::warn!("Failed to connect shared DockerManager: {}", e);
                    return None;
                }
            }
        }
        self.docker_manager.clone()
    }

    pub(super) fn spawn_status_check(&self) {
        let message_tx = self.message_tx.clone();
        let cluster_config = Arc::clone(&self.cluster_config);
        let timeout = self.refresh_config.status_check_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let manager = match ClusterManager::new(cluster_config).await {
                    Ok(m) => m,
                    Err(_) => return ClusterStatus::Unknown,
                };
                manager.get_status().await
            })
            .await;

            let status = result.unwrap_or(ClusterStatus::Unknown);
            let _ = message_tx
                .send(AppMessage::ClusterStatusUpdate(status))
                .await;
        });
    }

    pub(super) fn spawn_ingress_refresh(&self) {
        if !self.cluster_is_running() {
            return;
        }

        let message_tx = self.message_tx.clone();
        let domain = self.cluster_config.domain.clone();
        let timeout = self.refresh_config.ingress_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let mut ingress_manager = IngressManager::with_domain(domain);
                ingress_manager.get_ingress_entries().await
            })
            .await;

            if let Ok(Ok(entries)) = result {
                let _ = message_tx
                    .send(AppMessage::IngressEntriesLoaded(entries))
                    .await;
            }
        });
    }

    pub(super) fn spawn_ingress_health_check(&self) {
        if !self.cluster_is_running() {
            return;
        }

        let message_tx = self.message_tx.clone();
        let entries = self.menu.get_ingress_entries().to_vec();
        let timeout = self.refresh_config.ingress_health_timeout;

        if entries.is_empty() {
            return;
        }

        tokio::spawn(async move {
            let result =
                tokio::time::timeout(timeout, IngressHealthChecker::check_endpoints(&entries))
                    .await;

            if let Ok(health) = result {
                let _ = message_tx
                    .send(AppMessage::IngressHealthUpdated(health))
                    .await;
            }
        });
    }

    pub(super) fn spawn_missing_hosts_check(&self) {
        if !self.cluster_is_running() {
            return;
        }

        let message_tx = self.message_tx.clone();
        let domain = self.cluster_config.domain.clone();
        let timeout = self.refresh_config.ingress_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let mut ingress_manager = IngressManager::with_domain(domain);
                ingress_manager.get_missing_hosts().await
            })
            .await;

            if let Ok(Ok(missing)) = result {
                let _ = message_tx
                    .send(AppMessage::MissingHostsUpdated(missing))
                    .await;
            }
        });
    }

    pub(super) fn spawn_pod_stats_check(&mut self) {
        if !self.cluster_is_running() {
            let message_tx = self.message_tx.clone();
            tokio::spawn(async move {
                let _ = message_tx.send(AppMessage::PodStatsUpdated(vec![])).await;
            });
            return;
        }

        let docker = match self.ensure_docker_manager() {
            Some(d) => d,
            None => {
                let message_tx = self.message_tx.clone();
                tokio::spawn(async move {
                    let _ = message_tx.send(AppMessage::PodStatsUpdated(vec![])).await;
                });
                return;
            }
        };
        let message_tx = self.message_tx.clone();
        let container_name = self.cluster_config.container_name.clone();
        let timeout = self.refresh_config.docker_stats_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                // Try agent first, fall back to direct cgroup reads
                match docker.get_pod_stats_via_agent(&container_name).await {
                    Ok(stats) => Ok::<_, anyhow::Error>(stats),
                    Err(_) => docker.get_pod_stats(&container_name).await,
                }
            })
            .await;

            let stats = match result {
                Ok(Ok(stats)) => stats,
                _ => vec![],
            };

            // Send all pod stats - filtering is now done during merge
            // with pending pods data (which has K8s status info)
            let _ = message_tx.send(AppMessage::PodStatsUpdated(stats)).await;
        });
    }

    pub(super) fn spawn_pending_pods_check(&self) {
        if !self.cluster_is_running() {
            let message_tx = self.message_tx.clone();
            tokio::spawn(async move {
                let _ = message_tx
                    .send(AppMessage::PendingPodsUpdated(vec![]))
                    .await;
            });
            return;
        }

        let cached_k8s = self.k8s_client.clone();
        let message_tx = self.message_tx.clone();
        let kubeconfig = self.cluster_config.kubeconfig.clone();
        let context = self.cluster_config.context.clone();
        let timeout = self.refresh_config.docker_stats_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                // Reuse the lazily-initialized client when available instead of
                // re-reading kubeconfig + TLS setup on every tick.
                let k8s_client = match cached_k8s {
                    Some(c) => c,
                    None => K8sClient::new(kubeconfig.as_deref(), context.as_deref()).await?,
                };
                k8s_client.list_pending_pods().await
            })
            .await;

            let pending = match result {
                Ok(Ok(pods)) => pods,
                _ => vec![],
            };

            let _ = message_tx
                .send(AppMessage::PendingPodsUpdated(pending))
                .await;
        });
    }

    /// Spawn streaming monitors for images currently being pulled.
    /// Each monitor joins Docker's create_image stream for real-time byte-level progress.
    pub(super) fn spawn_pull_progress_check(&mut self) {
        if !self.cluster_is_running() {
            return;
        }

        let docker = match &self.docker_client {
            Some(d) => d,
            None => return,
        };

        // Get unique images from pending pods cache that are in pulling state
        let pulling_images: std::collections::HashSet<String> = self
            .pending_pods_cache
            .iter()
            .flat_map(|p| {
                p.containers
                    .iter()
                    .filter(|c| c.reason == "ContainerCreating" || c.reason == "PodInitializing")
                    .map(|c| c.image.clone())
            })
            .collect();

        if pulling_images.is_empty() {
            return;
        }

        // Spawn monitors for NEW pulling images only
        for image in pulling_images {
            if self.active_pull_monitors.contains(&image) {
                continue; // Already monitoring this image
            }
            self.active_pull_monitors.insert(image.clone());

            // Find the container name for this image (first match)
            let container_name = self
                .pending_pods_cache
                .iter()
                .flat_map(|p| &p.containers)
                .find(|c| c.image == image)
                .map(|c| c.name.clone())
                .unwrap_or_default();

            let docker = docker.clone();
            let message_tx = self.message_tx.clone();
            let semaphore = Arc::clone(&MANIFEST_SEMAPHORE);

            tokio::spawn(async move {
                monitor_image_pull(docker, image, container_name, message_tx, semaphore).await;
            });
        }
    }

    pub(super) fn spawn_volume_stats_check(&mut self) {
        if !self.cluster_is_running() {
            return;
        }

        let docker = match self.ensure_docker_manager() {
            Some(d) => d,
            None => return,
        };
        let cached_k8s = self.k8s_client.clone();
        let message_tx = self.message_tx.clone();
        let kubeconfig = self.cluster_config.kubeconfig.clone();
        let context = self.cluster_config.context.clone();
        let timeout = self.refresh_config.volume_timeout;
        let storage_path = crate::cluster::K3sManager::LOCAL_PV_STORAGE_PATH.to_string();
        let container_name = self.cluster_config.container_name.clone();

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                // 1. Get volume stats via docker exec + container mounts (PVC dirs, sizes, pod mapping)
                let volume_stats = docker
                    .get_volume_stats(&container_name, &storage_path)
                    .await;

                // 2. Get PVC metadata from K8s API (single call: capacity, phase, storage_class).
                //    Reuse the cached client when available, otherwise connect once.
                let pvc_metadata = match cached_k8s {
                    Some(k8s) => k8s.list_pvc_metadata().await.unwrap_or_default(),
                    None => match K8sClient::new(kubeconfig.as_deref(), context.as_deref()).await {
                        Ok(k8s) => k8s.list_pvc_metadata().await.unwrap_or_default(),
                        Err(_) => std::collections::HashMap::new(),
                    },
                };

                // 3. Merge: filesystem data + K8s metadata → Vec<PvcInfo>
                let fs_stats = volume_stats.unwrap_or_default();
                let mut results = Vec::new();

                // PVCs found on filesystem
                let mut seen_keys = std::collections::HashSet::new();
                for vs in &fs_stats {
                    let key = format!("{}/{}", vs.namespace, vs.pvc_name);
                    seen_keys.insert(key.clone());

                    if let Some(meta) = pvc_metadata.get(&key) {
                        results.push(crate::k8s::PvcInfo {
                            name: vs.pvc_name.clone(),
                            namespace: vs.namespace.clone(),
                            capacity_bytes: meta.capacity_bytes,
                            used_bytes: Some(vs.used_bytes),
                            phase: meta.phase.clone(),
                            storage_class: meta.storage_class.clone(),
                            pods: vs.pods.clone(),
                        });
                    } else {
                        // On filesystem but not in K8s (cleanup in progress?)
                        results.push(crate::k8s::PvcInfo {
                            name: vs.pvc_name.clone(),
                            namespace: vs.namespace.clone(),
                            capacity_bytes: 0,
                            used_bytes: Some(vs.used_bytes),
                            phase: "Unknown".to_string(),
                            storage_class: String::new(),
                            pods: vs.pods.clone(),
                        });
                    }
                }

                // PVCs in K8s but not on filesystem (Pending, not yet provisioned)
                for (key, meta) in &pvc_metadata {
                    if !seen_keys.contains(key) {
                        results.push(crate::k8s::PvcInfo {
                            name: meta.name.clone(),
                            namespace: meta.namespace.clone(),
                            capacity_bytes: meta.capacity_bytes,
                            used_bytes: None,
                            phase: meta.phase.clone(),
                            storage_class: meta.storage_class.clone(),
                            pods: Vec::new(),
                        });
                    }
                }

                results.sort_by(|a, b| a.namespace.cmp(&b.namespace).then(a.name.cmp(&b.name)));

                Ok::<Vec<crate::k8s::PvcInfo>, anyhow::Error>(results)
            })
            .await;

            let entries = match result {
                Ok(Ok(e)) => e,
                _ => vec![],
            };

            let _ = message_tx
                .send(AppMessage::VolumeStatsUpdated(entries))
                .await;
        });
    }

    pub(super) fn spawn_port_forwards_check(&self) {
        if !self.cluster_is_running() {
            let message_tx = self.message_tx.clone();
            tokio::spawn(async move {
                let _ = message_tx
                    .send(AppMessage::ActivePortForwardsUpdated(vec![]))
                    .await;
            });
            return;
        }

        let message_tx = self.message_tx.clone();
        let timeout = self.refresh_config.port_forward_timeout;

        tokio::spawn(async move {
            let mut detector = PortForwardDetector::new();
            let result = tokio::time::timeout(timeout, detector.detect()).await;

            let forwards = result.unwrap_or_default();
            let _ = message_tx
                .send(AppMessage::ActivePortForwardsUpdated(forwards))
                .await;
        });
    }

    /// Check image architectures for running pods (spawned when new pods appear)
    pub(super) fn spawn_image_arch_check(&mut self) {
        if !self.cluster_is_running() {
            return;
        }

        let docker = match self.ensure_docker_manager() {
            Some(d) => d,
            None => return,
        };
        let message_tx = self.message_tx.clone();
        let timeout = self.refresh_config.docker_stats_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                Ok::<_, anyhow::Error>(docker.get_pod_image_architectures().await)
            })
            .await;

            if let Ok(Ok(arch_data)) = result {
                if !arch_data.is_empty() {
                    let _ = message_tx
                        .send(AppMessage::ImageArchUpdated(arch_data))
                        .await;
                }
            }
        });
    }

    /// Check each configured info block; spawn a refresh task if its interval has elapsed.
    pub(super) fn info_block_tick(&mut self) {
        let now = std::time::Instant::now();
        let len = self.info_blocks.len();
        for i in 0..len {
            let rt = &self.info_blocks[i];
            if rt.in_flight {
                continue;
            }
            if now.duration_since(rt.last_run) < rt.cfg.interval {
                continue;
            }

            let target_needs_cluster =
                matches!(rt.cfg.exec.target, ExecutionTarget::Kubernetes { .. });
            if target_needs_cluster && !matches!(self.cluster_status, ClusterStatus::Running) {
                // Cluster is not running; emit a Skipped result so the row renders
                // a placeholder, and back off until the next interval.
                self.info_blocks[i].last_run = now;
                let tx = self.message_tx.clone();
                tokio::spawn(async move {
                    let _ = tx
                        .send(AppMessage::InfoBlockUpdated {
                            index: i,
                            result: InfoBlockResult {
                                output: String::new(),
                                status: InfoBlockStatus::Skipped,
                            },
                        })
                        .await;
                });
                continue;
            }

            self.info_blocks[i].in_flight = true;
            self.info_blocks[i].last_run = now;
            self.spawn_info_block(i);
        }
    }

    /// Fire any visibility probes whose interval has elapsed.
    pub(super) fn visibility_tick(&mut self) {
        let now = std::time::Instant::now();
        let len = self.visibility_tasks.len();
        for i in 0..len {
            let task = &self.visibility_tasks[i];
            if task.in_flight {
                continue;
            }
            // `type: pod` gates are evaluated in-memory against the pod list
            // (see `recompute_pod_visibility`), not via a per-probe K8s query.
            if matches!(task.check, VisibleCheck::Pod { .. }) {
                continue;
            }
            if now.duration_since(task.last_run) < task.interval {
                continue;
            }
            self.visibility_tasks[i].in_flight = true;
            self.visibility_tasks[i].last_run = now;
            self.spawn_visibility_check(i);
        }
    }

    fn spawn_visibility_check(&self, id: usize) {
        let task = &self.visibility_tasks[id];
        let check = task.check.clone();
        let interval = task.interval;
        // Keep the probe bounded well inside the interval so a slow check
        // doesn't stall the next tick. Clamp to a sane [1s, 30s] window.
        let timeout = interval
            .saturating_sub(std::time::Duration::from_millis(250))
            .max(std::time::Duration::from_secs(1))
            .min(std::time::Duration::from_secs(30));

        let k8s_client = self.k8s_client.clone();
        let message_tx = self.message_tx.clone();

        tokio::spawn(async move {
            // Build a DockerManager on-demand for checks that need it (mirrors
            // how info-block exec probes handle Docker access).
            let docker = match &check {
                VisibleCheck::Container { .. } => DockerManager::from_default_socket().ok(),
                VisibleCheck::Exec(cfg) if matches!(cfg.target, ExecutionTarget::Docker { .. }) => {
                    DockerManager::from_default_socket().ok()
                }
                _ => None,
            };
            let (visible, error) =
                match check_visible(&check, k8s_client.as_ref(), docker.as_ref(), timeout).await {
                    Ok(v) => (v, None),
                    Err(e) => (false, Some(e.to_string())),
                };
            let _ = message_tx
                .send(AppMessage::VisibilityUpdated { id, visible, error })
                .await;
        });
    }

    fn spawn_info_block(&self, index: usize) {
        let rt = &self.info_blocks[index];
        let exec = rt.cfg.exec.clone();
        let interval = rt.cfg.interval;
        let max_lines = rt.cfg.max_lines;
        let max_length = rt.cfg.max_length;
        // Cap per-run timeout so a slow script can't block the next interval indefinitely.
        let timeout = interval
            .saturating_sub(std::time::Duration::from_millis(250))
            .max(std::time::Duration::from_secs(2))
            .min(std::time::Duration::from_secs(60));
        let k8s_client = self.k8s_client.clone();
        let message_tx = self.message_tx.clone();

        tokio::spawn(async move {
            let docker = match &exec.target {
                ExecutionTarget::Docker { .. } => DockerManager::from_default_socket().ok(),
                _ => None,
            };
            let result =
                match capture_exec(&exec, k8s_client.as_ref(), docker.as_ref(), timeout).await {
                    Ok(raw) => {
                        let cleaned = strip_ansi(&raw);
                        let trimmed = trim_output(&cleaned, max_lines, max_length);
                        InfoBlockResult {
                            output: trimmed,
                            status: InfoBlockStatus::Ok,
                        }
                    }
                    Err(e) => InfoBlockResult {
                        output: String::new(),
                        status: InfoBlockStatus::Error(e.to_string()),
                    },
                };
            let _ = message_tx
                .send(AppMessage::InfoBlockUpdated { index, result })
                .await;
        });
    }

    /// One-shot check at startup: fetch the latest GitHub release and, if it is
    /// newer than the running build, notify the UI. Fails silently on any
    /// network/parse error so an offline machine sees no noise.
    pub(super) fn spawn_version_check(&self) {
        let message_tx = self.message_tx.clone();

        tokio::spawn(async move {
            let Some(latest) = fetch_latest_release_version().await else {
                return;
            };
            if is_newer(&latest, env!("K3DEV_VERSION")) {
                let _ = message_tx.send(AppMessage::UpdateAvailable(latest)).await;
            }
        });
    }
}

/// Query the GitHub Releases API for the latest published `k3dev` release and
/// return its version (the `tag_name` with any leading `v` stripped).
async fn fetch_latest_release_version() -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let release: Release = client
        .get("https://api.github.com/repos/daylioti/k3dev/releases/latest")
        .header(reqwest::header::USER_AGENT, "k3dev")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;

    Some(release.tag_name.trim_start_matches('v').trim().to_string())
}

/// Parse a `major.minor.patch` version, ignoring any leading `v` and any
/// pre-release/build suffix. Missing minor/patch components default to 0.
fn parse_semver(v: &str) -> Option<(u32, u32, u32)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest` is a strictly newer semantic version than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_newer, parse_semver};

    #[test]
    fn parses_versions_with_optional_prefix_and_suffix() {
        assert_eq!(parse_semver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("v2.0"), Some((2, 0, 0)));
        assert_eq!(parse_semver("1.4.0-rc.1"), Some((1, 4, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
    }

    #[test]
    fn detects_newer_releases() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
