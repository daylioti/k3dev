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

    pub(super) fn spawn_pod_stats_check(&self) {
        if !self.cluster_is_running() {
            let message_tx = self.message_tx.clone();
            tokio::spawn(async move {
                let _ = message_tx.send(AppMessage::PodStatsUpdated(vec![])).await;
            });
            return;
        }

        let message_tx = self.message_tx.clone();
        let container_name = self.cluster_config.container_name.clone();
        let timeout = self.refresh_config.docker_stats_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let docker = DockerManager::from_default_socket()
                    .map_err(|_| anyhow::anyhow!("Failed to create DockerManager"))?;
                // Try agent first, fall back to direct cgroup reads
                match docker.get_pod_stats_via_agent(&container_name).await {
                    Ok(stats) => Ok(stats),
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

        let message_tx = self.message_tx.clone();
        let kubeconfig = self.cluster_config.kubeconfig.clone();
        let context = self.cluster_config.context.clone();
        let timeout = self.refresh_config.docker_stats_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let k8s_client = K8sClient::new(kubeconfig.as_deref(), context.as_deref()).await?;
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

    pub(super) fn spawn_volume_stats_check(&self) {
        if !self.cluster_is_running() {
            return;
        }

        let message_tx = self.message_tx.clone();
        let kubeconfig = self.cluster_config.kubeconfig.clone();
        let context = self.cluster_config.context.clone();
        let timeout = self.refresh_config.volume_timeout;
        let storage_path = crate::cluster::K3sManager::LOCAL_PV_STORAGE_PATH.to_string();
        let container_name = self.cluster_config.container_name.clone();

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let docker = DockerManager::from_default_socket()
                    .map_err(|_| anyhow::anyhow!("Failed to create DockerManager"))?;

                // 1. Get volume stats via docker exec + container mounts (PVC dirs, sizes, pod mapping)
                let volume_stats = docker
                    .get_volume_stats(&container_name, &storage_path)
                    .await;

                // 2. Get PVC metadata from K8s API (single call: capacity, phase, storage_class)
                let pvc_metadata =
                    match K8sClient::new(kubeconfig.as_deref(), context.as_deref()).await {
                        Ok(k8s) => k8s.list_pvc_metadata().await.unwrap_or_default(),
                        Err(_) => std::collections::HashMap::new(),
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
    pub(super) fn spawn_image_arch_check(&self) {
        if !self.cluster_is_running() {
            return;
        }

        let message_tx = self.message_tx.clone();
        let timeout = self.refresh_config.docker_stats_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let docker = DockerManager::from_default_socket()
                    .map_err(|_| anyhow::anyhow!("Failed to create DockerManager"))?;
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
}
