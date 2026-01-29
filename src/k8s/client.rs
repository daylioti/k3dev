use anyhow::Result;
use k8s_openapi::api::core::v1::{Namespace, Node, Pod};
use kube::{
    api::{Api, DeleteParams, ListParams, LogParams},
    config::{KubeConfigOptions, Kubeconfig},
    Client, Config,
};
use std::collections::HashMap;
use std::path::Path;

use crate::config::expand_home;

/// Node information
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub name: String,
    pub status: String,
    pub cpu: String,
    pub memory: String,
}

/// Cluster information
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ClusterInfo {
    pub connected: bool,
    pub nodes: Vec<NodeInfo>,
    pub namespaces: Vec<String>,
    pub pod_counts: HashMap<String, usize>,
}

/// Pod information
#[derive(Debug, Clone)]
pub struct PodInfo {
    pub name: String,
    #[allow(dead_code)]
    pub namespace: String,
    #[allow(dead_code)]
    pub status: String,
    #[allow(dead_code)]
    pub containers: Vec<String>,
    #[allow(dead_code)]
    pub ready: bool,
    #[allow(dead_code)]
    pub ip: Option<String>,
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
    #[allow(dead_code)]
    pub async fn is_connected(&self) -> bool {
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        namespaces
            .list(&ListParams::default().limit(1))
            .await
            .is_ok()
    }

    /// Get cluster information
    #[allow(dead_code)]
    pub async fn get_cluster_info(&self) -> Result<ClusterInfo> {
        let mut info = ClusterInfo::default();

        if !self.is_connected().await {
            return Ok(info);
        }
        info.connected = true;

        // Get nodes
        let nodes: Api<Node> = Api::all(self.client.clone());
        if let Ok(node_list) = nodes.list(&ListParams::default()).await {
            for node in node_list.items {
                let name = node.metadata.name.unwrap_or_default();
                let status = node
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .and_then(|c| c.iter().find(|c| c.type_ == "Ready"))
                    .map(|c| {
                        if c.status == "True" {
                            "Ready"
                        } else {
                            "NotReady"
                        }
                    })
                    .unwrap_or("Unknown")
                    .to_string();

                let cpu = node
                    .status
                    .as_ref()
                    .and_then(|s| s.allocatable.as_ref())
                    .and_then(|a| a.get("cpu"))
                    .map(|q| q.0.clone())
                    .unwrap_or_default();

                let memory = node
                    .status
                    .as_ref()
                    .and_then(|s| s.allocatable.as_ref())
                    .and_then(|a| a.get("memory"))
                    .map(|q| q.0.clone())
                    .unwrap_or_default();

                info.nodes.push(NodeInfo {
                    name,
                    status,
                    cpu,
                    memory,
                });
            }
        }

        // Get namespaces and pod counts
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        if let Ok(ns_list) = namespaces.list(&ListParams::default()).await {
            for ns in ns_list.items {
                if let Some(name) = ns.metadata.name {
                    let pods: Api<Pod> = Api::namespaced(self.client.clone(), &name);
                    let count = pods
                        .list(&ListParams::default())
                        .await
                        .map(|p| p.items.len())
                        .unwrap_or(0);
                    info.pod_counts.insert(name.clone(), count);
                    info.namespaces.push(name);
                }
            }
        }

        Ok(info)
    }

    /// List namespaces
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
                let containers = pod
                    .spec
                    .as_ref()
                    .map(|s| s.containers.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default();
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
                    containers,
                    ready,
                    ip,
                }
            })
            .collect())
    }

    /// Find a running pod by selector
    #[allow(dead_code)]
    pub async fn find_running_pod(
        &self,
        namespace: &str,
        selector: &str,
    ) -> Result<Option<PodInfo>> {
        let pods = self.list_pods(namespace, Some(selector)).await?;

        Ok(pods.into_iter().find(|p| p.status == "Running"))
    }

    /// Get pod by name
    #[allow(dead_code)]
    pub async fn get_pod(&self, namespace: &str, name: &str) -> Result<PodInfo> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod = pods.get(name).await?;

        let status = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        let containers = pod
            .spec
            .as_ref()
            .map(|s| s.containers.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
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
            containers,
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
                    .map(|t| t.0.to_string())
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
                                    .map(|t| t.0.to_string())
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
}
