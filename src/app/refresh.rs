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
use crate::k8s::K8sClient;

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

    pub(super) fn spawn_resource_stats_check(&self) {
        if !self.cluster_is_running() {
            let message_tx = self.message_tx.clone();
            tokio::spawn(async move {
                let _ = message_tx
                    .send(AppMessage::ResourceStatsUpdated(None))
                    .await;
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
                match docker.get_container_stats_via_agent(&container_name).await {
                    Ok(stats) => Ok(stats),
                    Err(_) => docker.get_container_stats(&container_name).await,
                }
            })
            .await;

            let stats_option = match result {
                Ok(Ok(stats)) if stats.cpu_percent > 0.0 || stats.memory_used_mb > 0.0 => {
                    Some(stats)
                }
                _ => None,
            };
            let _ = message_tx
                .send(AppMessage::ResourceStatsUpdated(stats_option))
                .await;
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

}
