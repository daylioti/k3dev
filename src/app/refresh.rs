//! Background refresh tasks
//!
//! This module contains all spawn_* methods for background data refresh.

use std::collections::HashSet;

use crate::cluster::{
    ClusterManager, ClusterStatus, DockerManager, IngressHealthChecker, IngressManager,
    PortForwardDetector,
};
use crate::k8s::K8sClient;

use super::{App, AppMessage};

impl App {
    pub(super) fn spawn_status_check(&self) {
        let message_tx = self.message_tx.clone();
        let cluster_config = self.cluster_config.clone();
        let timeout = self.refresh_config.status_check_timeout;

        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let manager = match ClusterManager::new(Some(cluster_config)).await {
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

    pub(super) fn spawn_hosts_update(&self) {
        if !self.auto_update_hosts || !matches!(self.cluster_status, ClusterStatus::Running) {
            return;
        }

        let domain = self.cluster_config.domain.clone();
        let timeout = self.refresh_config.hosts_update_timeout;

        // Auto hosts update tries without sudo - silently skips if no permission
        tokio::spawn(async move {
            let _ = tokio::time::timeout(timeout, async {
                let mut ingress_manager = IngressManager::with_domain(domain);
                // Silently ignore errors - user can manually update with 'H' key if needed
                let _ = ingress_manager.update_hosts(None).await;
            })
            .await;
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
        let kubeconfig = self.cluster_config.kubeconfig.clone();
        let context = self.cluster_config.context.clone();
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

            let mut stats = match result {
                Ok(Ok(stats)) => stats,
                _ => vec![],
            };

            // Filter to only show pods with Kubernetes status "Running"
            if !stats.is_empty() {
                if let Ok(k8s_client) =
                    K8sClient::new(kubeconfig.as_deref(), context.as_deref()).await
                {
                    let mut running_pods: HashSet<String> = HashSet::new();

                    // Get all namespaces and collect running pods
                    if let Ok(namespaces) = k8s_client.list_namespaces().await {
                        for ns in namespaces {
                            if let Ok(pods) = k8s_client.list_pods(&ns, None).await {
                                for pod in pods {
                                    if pod.status == "Running" {
                                        running_pods.insert(pod.name.clone());
                                    }
                                }
                            }
                        }
                    }

                    // Filter stats to only include running pods
                    if !running_pods.is_empty() {
                        stats.retain(|s| running_pods.contains(&s.name));
                    }
                }
            }

            let _ = message_tx.send(AppMessage::PodStatsUpdated(stats)).await;
        });
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
