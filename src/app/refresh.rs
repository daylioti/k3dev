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
        if !matches!(self.cluster_status, ClusterStatus::Running) {
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
        if !matches!(self.cluster_status, ClusterStatus::Running) {
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
        if !matches!(self.cluster_status, ClusterStatus::Running) {
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
        if !matches!(self.cluster_status, ClusterStatus::Running) {
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
                let socket_path = std::path::PathBuf::from("/var/run/docker.sock");

                let docker = match DockerManager::new(socket_path) {
                    Ok(d) => d,
                    Err(_) => return Err(anyhow::anyhow!("Failed to create DockerManager")),
                };
                docker.get_container_stats(&container_name).await
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
        if !matches!(self.cluster_status, ClusterStatus::Running) {
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
                let socket_path = std::path::PathBuf::from("/var/run/docker.sock");

                let docker = match DockerManager::new(socket_path) {
                    Ok(d) => d,
                    Err(_) => return Err(anyhow::anyhow!("Failed to create DockerManager")),
                };
                docker.get_pod_stats(&container_name).await
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
        if !matches!(self.cluster_status, ClusterStatus::Running) {
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
        if !matches!(self.cluster_status, ClusterStatus::Running) {
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

    pub(super) fn spawn_port_forwards_check(&self) {
        if !matches!(self.cluster_status, ClusterStatus::Running) {
            let message_tx = self.message_tx.clone();
            tokio::spawn(async move {
                let _ = message_tx
                    .send(AppMessage::ActivePortForwardsUpdated(vec![]))
                    .await;
            });
            return;
        }

        let message_tx = self.message_tx.clone();
        let container_name = self.cluster_config.container_name.clone();
        let static_ports = self.get_static_ports();
        let timeout = self.refresh_config.port_forward_timeout;

        tokio::spawn(async move {
            let mut detector = PortForwardDetector::new(container_name, static_ports);
            let result = tokio::time::timeout(timeout, detector.detect()).await;

            let forwards = result.unwrap_or_default();
            let _ = message_tx
                .send(AppMessage::ActivePortForwardsUpdated(forwards))
                .await;
        });
    }

    /// Get static port mappings from config
    pub(super) fn get_static_ports(&self) -> Vec<(u16, u16)> {
        let mut ports = vec![
            (self.cluster_config.http_port, self.cluster_config.http_port),
            (
                self.cluster_config.https_port,
                self.cluster_config.https_port,
            ),
            (self.cluster_config.api_port, self.cluster_config.api_port),
        ];
        ports.extend(self.cluster_config.additional_ports.clone());
        ports
    }
}
