use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim, Pod};
use kube::{
    api::{Api, DeleteParams, ListParams, LogParams},
    config::{KubeConfigOptions, Kubeconfig},
    Client, Config,
};
use std::collections::HashMap;
use std::path::Path;

use super::jiff_to_chrono;
use crate::config::expand_home;

/// Pod information
#[derive(Debug, Clone)]
pub struct PodInfo {
    pub name: String,
    pub namespace: String,
    pub status: String,
    pub ready: bool,
    pub ip: Option<String>,
}

/// Information about a container waiting for image pull or in error state
#[derive(Debug, Clone)]
pub struct ContainerWaitingInfo {
    pub name: String,
    pub image: String,
    pub reason: String, // "ContainerCreating", "ImagePullBackOff", "ErrImagePull"
}

/// Information about a pending pod
#[derive(Debug, Clone)]
pub struct PendingPodInfo {
    pub name: String,
    pub namespace: String,
    pub containers: Vec<ContainerWaitingInfo>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// PVC information with usage data
#[derive(Debug, Clone)]
pub struct PvcInfo {
    pub name: String,
    pub namespace: String,
    pub capacity_bytes: u64,
    pub used_bytes: Option<u64>,
    pub phase: String,
    pub storage_class: String,
    pub pods: Vec<String>,
}

/// PVC metadata from K8s API (capacity, phase, storage class)
#[derive(Debug, Clone)]
pub struct PvcMetadata {
    pub name: String,
    pub namespace: String,
    pub capacity_bytes: u64,
    pub phase: String,
    pub storage_class: String,
}

/// Kubernetes client wrapper
#[derive(Clone)]
pub struct K8sClient {
    client: Client,
}

impl K8sClient {
    /// Create a new K8s client
    pub async fn new(kubeconfig: Option<&str>, context: Option<&str>) -> Result<Self> {
        let config = if let Some(path) = kubeconfig.filter(|s| !s.is_empty()) {
            let expanded = expand_home(Path::new(path))?;
            let kubeconfig_data = Kubeconfig::read_from(&expanded)?;
            let config_options = KubeConfigOptions {
                context: context.filter(|s| !s.is_empty()).map(String::from),
                ..Default::default()
            };
            Config::from_custom_kubeconfig(kubeconfig_data, &config_options).await?
        } else {
            Config::infer().await?
        };

        let client = Client::try_from(config)?;

        Ok(Self { client })
    }

    /// Check if connected to cluster
    pub async fn is_connected(&self) -> bool {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        namespaces
            .list(&ListParams::default().limit(1))
            .await
            .is_ok()
    }

    /// List namespaces
    pub async fn list_namespaces(&self) -> Result<Vec<String>> {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let list = namespaces.list(&ListParams::default()).await?;

        Ok(list
            .items
            .into_iter()
            .filter_map(|ns| ns.metadata.name)
            .collect())
    }

    /// List pods in a namespace with optional label selector
    pub async fn list_pods(&self, namespace: &str, selector: Option<&str>) -> Result<Vec<PodInfo>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let mut params = ListParams::default();
        if let Some(sel) = selector {
            params = params.labels(sel);
        }

        let list = pods.list(&params).await?;

        Ok(list
            .items
            .into_iter()
            .map(|pod| {
                let name = pod.metadata.name.unwrap_or_default();
                let namespace = pod.metadata.namespace.unwrap_or_default();
                let status = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.clone())
                    .unwrap_or_else(|| "Unknown".to_string());
                let ready = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .and_then(|c| c.iter().find(|c| c.type_ == "Ready"))
                    .map(|c| c.status == "True")
                    .unwrap_or(false);
                let ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());

                PodInfo {
                    name,
                    namespace,
                    status,
                    ready,
                    ip,
                }
            })
            .collect())
    }

    /// Get pod by name
    pub async fn get_pod(&self, namespace: &str, name: &str) -> Result<PodInfo> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod = pods.get(name).await?;

        let status = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        let ready = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|c| c.type_ == "Ready"))
            .map(|c| c.status == "True")
            .unwrap_or(false);
        let ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());

        Ok(PodInfo {
            name: pod.metadata.name.unwrap_or_default(),
            namespace: pod.metadata.namespace.unwrap_or_default(),
            status,
            ready,
            ip,
        })
    }

    /// Get the underlying kube client
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get pod logs
    pub async fn get_pod_logs(
        &self,
        namespace: &str,
        name: &str,
        container: Option<&str>,
        tail_lines: Option<i64>,
    ) -> Result<String> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let mut log_params = LogParams::default();
        if let Some(c) = container {
            log_params.container = Some(c.to_string());
        }
        if let Some(tail) = tail_lines {
            log_params.tail_lines = Some(tail);
        }
        let logs = pods.logs(name, &log_params).await?;
        Ok(logs)
    }

    /// Describe a pod (returns formatted text similar to kubectl describe)
    pub async fn describe_pod(&self, namespace: &str, name: &str) -> Result<String> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod = pods.get(name).await?;

        let mut output = String::new();

        // Basic info
        output.push_str(&format!(
            "Name:         {}\n",
            pod.metadata.name.as_deref().unwrap_or("N/A")
        ));
        output.push_str(&format!(
            "Namespace:    {}\n",
            pod.metadata.namespace.as_deref().unwrap_or("N/A")
        ));

        // Labels
        if let Some(labels) = &pod.metadata.labels {
            output.push_str("Labels:       ");
            let labels_str: Vec<String> =
                labels.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            output.push_str(&labels_str.join("\n              "));
            output.push('\n');
        }

        // Status
        if let Some(status) = &pod.status {
            output.push_str(&format!(
                "Status:       {}\n",
                status.phase.as_deref().unwrap_or("Unknown")
            ));
            output.push_str(&format!(
                "IP:           {}\n",
                status.pod_ip.as_deref().unwrap_or("N/A")
            ));
            output.push_str(&format!(
                "Node:         {}\n",
                status.nominated_node_name.as_deref().unwrap_or("N/A")
            ));
            output.push_str(&format!(
                "Start Time:   {}\n",
                status
                    .start_time
                    .as_ref()
                    .map(|t| jiff_to_chrono(t.0).to_string())
                    .unwrap_or_else(|| "N/A".to_string())
            ));

            // Conditions
            if let Some(conditions) = &status.conditions {
                output.push_str("\nConditions:\n");
                for cond in conditions {
                    output.push_str(&format!(
                        "  Type: {}, Status: {}, Reason: {}\n",
                        cond.type_,
                        cond.status,
                        cond.reason.as_deref().unwrap_or("N/A")
                    ));
                }
            }

            // Container statuses
            if let Some(container_statuses) = &status.container_statuses {
                output.push_str("\nContainers:\n");
                for cs in container_statuses {
                    output.push_str(&format!("  {}:\n", cs.name));
                    output.push_str(&format!("    Ready:         {}\n", cs.ready));
                    output.push_str(&format!("    Restart Count: {}\n", cs.restart_count));
                    output.push_str(&format!("    Image:         {}\n", cs.image));
                    if let Some(state) = &cs.state {
                        if let Some(running) = &state.running {
                            output.push_str(&format!(
                                "    State:         Running since {}\n",
                                running
                                    .started_at
                                    .as_ref()
                                    .map(|t| jiff_to_chrono(t.0).to_string())
                                    .unwrap_or_else(|| "N/A".to_string())
                            ));
                        } else if let Some(waiting) = &state.waiting {
                            output.push_str(&format!(
                                "    State:         Waiting ({})\n",
                                waiting.reason.as_deref().unwrap_or("N/A")
                            ));
                        } else if let Some(terminated) = &state.terminated {
                            output.push_str(&format!(
                                "    State:         Terminated (exit code: {})\n",
                                terminated.exit_code
                            ));
                        }
                    }
                }
            }
        }

        // Spec info
        if let Some(spec) = &pod.spec {
            output.push_str(&format!(
                "\nNode Name:    {}\n",
                spec.node_name.as_deref().unwrap_or("N/A")
            ));

            // Containers
            output.push_str("\nContainer Specs:\n");
            for container in &spec.containers {
                output.push_str(&format!("  {}:\n", container.name));
                output.push_str(&format!(
                    "    Image:   {}\n",
                    container.image.as_deref().unwrap_or("N/A")
                ));
                if let Some(resources) = &container.resources {
                    if let Some(requests) = &resources.requests {
                        let req_str: Vec<String> = requests
                            .iter()
                            .map(|(k, v)| format!("{}={}", k, v.0))
                            .collect();
                        output.push_str(&format!("    Requests: {}\n", req_str.join(", ")));
                    }
                    if let Some(limits) = &resources.limits {
                        let lim_str: Vec<String> = limits
                            .iter()
                            .map(|(k, v)| format!("{}={}", k, v.0))
                            .collect();
                        output.push_str(&format!("    Limits:   {}\n", lim_str.join(", ")));
                    }
                }
            }
        }

        Ok(output)
    }

    /// Delete a pod
    pub async fn delete_pod(&self, namespace: &str, name: &str) -> Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        pods.delete(name, &DeleteParams::default()).await?;
        Ok(())
    }

    /// List PVC metadata (capacity, phase, storage_class) — single K8s API call.
    /// Returns a HashMap keyed by "namespace/name" for easy merging with filesystem data.
    pub async fn list_pvc_metadata(&self) -> Result<HashMap<String, PvcMetadata>> {
        let pvcs: Api<PersistentVolumeClaim> = Api::all(self.client.clone());
        let pvc_list = pvcs
            .list(&ListParams::default())
            .await
            .context("Failed to list PVCs")?;

        let mut map = HashMap::new();
        for pvc in pvc_list.items {
            let name = pvc.metadata.name.clone().unwrap_or_default();
            let namespace = pvc.metadata.namespace.clone().unwrap_or_default();

            let phase = pvc
                .status
                .as_ref()
                .and_then(|s| s.phase.clone())
                .unwrap_or_else(|| "Unknown".to_string());

            let storage_class = pvc
                .spec
                .as_ref()
                .and_then(|s| s.storage_class_name.clone())
                .unwrap_or_default();

            let capacity_bytes = pvc
                .status
                .as_ref()
                .and_then(|s| s.capacity.as_ref())
                .and_then(|c| c.get("storage"))
                .map(|q| parse_k8s_quantity(&q.0))
                .unwrap_or(0);

            let key = format!("{}/{}", namespace, name);
            map.insert(
                key,
                PvcMetadata {
                    name,
                    namespace,
                    capacity_bytes,
                    phase,
                    storage_class,
                },
            );
        }

        Ok(map)
    }

    /// List pending pods with container waiting info
    /// Returns pods that are in Pending phase or have containers in waiting state
    pub async fn list_pending_pods(&self) -> Result<Vec<PendingPodInfo>> {
        let namespaces = self.list_namespaces().await?;
        let mut pending_pods = Vec::new();

        for ns in namespaces {
            let pods: Api<Pod> = Api::namespaced(self.client.clone(), &ns);
            let list = pods.list(&ListParams::default()).await?;

            for pod in list.items {
                let pod_name = pod.metadata.name.clone().unwrap_or_default();
                let namespace = pod.metadata.namespace.clone().unwrap_or_default();

                // Check pod phase
                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_ref())
                    .map(|s| s.as_str())
                    .unwrap_or("Unknown");

                // Skip Running and Succeeded pods
                if phase == "Running" || phase == "Succeeded" {
                    continue;
                }

                // Get start time
                let started_at = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.start_time.as_ref())
                    .map(|t| jiff_to_chrono(t.0));

                // Get container images from spec
                let container_images: HashMap<String, String> = pod
                    .spec
                    .as_ref()
                    .map(|spec| {
                        spec.containers
                            .iter()
                            .map(|c| {
                                (
                                    c.name.clone(),
                                    c.image.clone().unwrap_or_else(|| "unknown".to_string()),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Check container statuses for waiting states
                let mut waiting_containers = Vec::new();

                if let Some(status) = &pod.status {
                    // Check both container_statuses and init_container_statuses
                    let all_statuses = status
                        .container_statuses
                        .iter()
                        .flatten()
                        .chain(status.init_container_statuses.iter().flatten());

                    for cs in all_statuses {
                        if let Some(state) = &cs.state {
                            if let Some(waiting) = &state.waiting {
                                let reason = waiting
                                    .reason
                                    .clone()
                                    .unwrap_or_else(|| "Unknown".to_string());
                                let image = container_images
                                    .get(&cs.name)
                                    .cloned()
                                    .unwrap_or_else(|| cs.image.clone());

                                waiting_containers.push(ContainerWaitingInfo {
                                    name: cs.name.clone(),
                                    image,
                                    reason,
                                });
                            }
                        }
                    }
                }

                // Also add pods in Pending phase even if no container status yet
                // (very early in creation, before containers are even created)
                if !waiting_containers.is_empty() || phase == "Pending" || phase == "Failed" {
                    // If no container status yet, create entries from spec
                    if waiting_containers.is_empty() {
                        if let Some(spec) = &pod.spec {
                            for container in &spec.containers {
                                waiting_containers.push(ContainerWaitingInfo {
                                    name: container.name.clone(),
                                    image: container
                                        .image
                                        .clone()
                                        .unwrap_or_else(|| "unknown".to_string()),
                                    reason: if phase == "Failed" {
                                        "Failed".to_string()
                                    } else {
                                        "ContainerCreating".to_string()
                                    },
                                });
                            }
                        }
                    }

                    pending_pods.push(PendingPodInfo {
                        name: pod_name,
                        namespace,
                        containers: waiting_containers,
                        started_at,
                    });
                }
            }
        }

        Ok(pending_pods)
    }
}

/// Parse K8s resource quantity strings (e.g., "10Gi", "500Mi", "1Ti") to bytes
fn parse_k8s_quantity(quantity: &str) -> u64 {
    let quantity = quantity.trim();
    if quantity.is_empty() {
        return 0;
    }

    // Try binary suffixes (Ki, Mi, Gi, Ti)
    if let Some(num) = quantity.strip_suffix("Ki") {
        return num.parse::<u64>().unwrap_or(0) * 1024;
    }
    if let Some(num) = quantity.strip_suffix("Mi") {
        return num.parse::<u64>().unwrap_or(0) * 1024 * 1024;
    }
    if let Some(num) = quantity.strip_suffix("Gi") {
        return num.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024;
    }
    if let Some(num) = quantity.strip_suffix("Ti") {
        return num.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024 * 1024;
    }
    // Try decimal suffixes (k, M, G, T)
    if let Some(num) = quantity.strip_suffix('k') {
        return num.parse::<u64>().unwrap_or(0) * 1000;
    }
    if let Some(num) = quantity.strip_suffix('M') {
        return num.parse::<u64>().unwrap_or(0) * 1_000_000;
    }
    if let Some(num) = quantity.strip_suffix('G') {
        return num.parse::<u64>().unwrap_or(0) * 1_000_000_000;
    }
    if let Some(num) = quantity.strip_suffix('T') {
        return num.parse::<u64>().unwrap_or(0) * 1_000_000_000_000;
    }
    // Plain number (bytes)
    quantity.parse::<u64>().unwrap_or(0)
}
