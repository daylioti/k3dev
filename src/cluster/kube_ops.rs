//! Kubernetes operations using the kube crate (replaces kubectl commands)

use anyhow::{anyhow, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Node, Pod, Secret, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use kube::api::{Api, DynamicObject, ListParams, Patch, PatchParams, PostParams};
use kube::config::Kubeconfig;
use kube::discovery::ApiResource;
use kube::{Client, Config};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::sleep;

/// Lazy-compiled regex for extracting Host from Traefik IngressRoute match rules
static HOST_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"Host\(`([^`]+)`\)").expect("Invalid HOST_REGEX pattern"));

/// Lazy-compiled regex for extracting PathPrefix from Traefik IngressRoute match rules
static PATH_PREFIX_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"PathPrefix\(`([^`]+)`\)").expect("Invalid PATH_PREFIX_REGEX pattern")
});

/// Lazy-initialized Kubernetes client
/// Creates connection on first use, handles cases where cluster isn't ready yet
pub struct KubeOps {
    client: Option<Client>,
}

impl KubeOps {
    pub fn new() -> Self {
        Self { client: None }
    }

    /// Get or create the kube client
    async fn client(&mut self) -> Result<&Client> {
        if self.client.is_none() {
            let config = Config::infer().await?;
            let client = Client::try_from(config)?;
            self.client = Some(client);
        }
        // Safety: client is guaranteed to be Some after the above initialization
        Ok(self.client.as_ref().expect("client was just initialized"))
    }

    /// Try to get client, returns None if cluster not accessible
    async fn try_client(&mut self) -> Option<&Client> {
        if self.client.is_none() {
            let config = Config::infer().await.ok()?;
            let client = Client::try_from(config).ok()?;
            self.client = Some(client);
        }
        self.client.as_ref()
    }

    /// Reset the client (useful after kubeconfig changes)
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.client = None;
    }

    // ==================== Deployment Operations ====================

    /// Get deployment ready replicas count
    pub async fn get_deployment_ready_replicas(
        &mut self,
        name: &str,
        namespace: &str,
    ) -> Result<i32> {
        let client = self.client().await?;
        let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);
        let deploy = deployments.get(name).await?;
        Ok(deploy.status.and_then(|s| s.ready_replicas).unwrap_or(0))
    }

    /// Wait for deployment to have at least one ready replica
    pub async fn wait_for_deployment_ready(
        &mut self,
        name: &str,
        namespace: &str,
        timeout_secs: u64,
    ) -> Result<bool> {
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            match self.get_deployment_ready_replicas(name, namespace).await {
                Ok(replicas) if replicas > 0 => return Ok(true),
                _ => {}
            }
            sleep(Duration::from_secs(2)).await;
        }
        Ok(false)
    }

    /// Rollout restart a deployment by updating pod template annotation
    pub async fn rollout_restart_deployment(&mut self, name: &str, namespace: &str) -> Result<()> {
        let client = self.client().await?;
        let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);

        let patch = json!({
            "spec": {
                "template": {
                    "metadata": {
                        "annotations": {
                            "kubectl.kubernetes.io/restartedAt": chrono::Utc::now().to_rfc3339()
                        }
                    }
                }
            }
        });

        deployments
            .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    /// Wait for deployment rollout to complete
    pub async fn wait_for_rollout(
        &mut self,
        name: &str,
        namespace: &str,
        timeout_secs: u64,
    ) -> Result<bool> {
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            let client = self.client().await?;
            let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);

            if let Ok(deploy) = deployments.get(name).await {
                if let Some(status) = deploy.status {
                    let desired = deploy.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
                    let ready = status.ready_replicas.unwrap_or(0);
                    let updated = status.updated_replicas.unwrap_or(0);
                    let available = status.available_replicas.unwrap_or(0);

                    if ready >= desired && updated >= desired && available >= desired {
                        return Ok(true);
                    }
                }
            }
            sleep(Duration::from_secs(2)).await;
        }
        Ok(false)
    }

    // ==================== ConfigMap Operations ====================

    /// Get ConfigMap data field
    pub async fn get_configmap_data(
        &mut self,
        name: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<String>> {
        let client = self.client().await?;
        let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
        let cm = configmaps.get(name).await?;
        Ok(cm.data.and_then(|d| d.get(key).cloned()))
    }

    /// Patch ConfigMap data
    pub async fn patch_configmap_data(
        &mut self,
        name: &str,
        namespace: &str,
        data: BTreeMap<String, String>,
    ) -> Result<()> {
        let client = self.client().await?;
        let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);

        let patch = json!({
            "data": data
        });

        configmaps
            .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    /// Remove annotation from a resource
    pub async fn remove_configmap_annotation(
        &mut self,
        name: &str,
        namespace: &str,
        annotation: &str,
    ) -> Result<()> {
        let client = self.client().await?;
        let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);

        // First get the current configmap to check if annotation exists
        if let Ok(cm) = configmaps.get(name).await {
            if let Some(annotations) = cm.metadata.annotations {
                if annotations.contains_key(annotation) {
                    // Use strategic merge patch with null value to remove annotation
                    let patch = json!({
                        "metadata": {
                            "annotations": {
                                annotation: serde_json::Value::Null
                            }
                        }
                    });
                    let _ = configmaps
                        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await;
                }
            }
        }
        Ok(())
    }

    // ==================== Secret Operations ====================

    /// Create a TLS secret
    pub async fn create_tls_secret(
        &mut self,
        name: &str,
        namespace: &str,
        cert_data: Vec<u8>,
        key_data: Vec<u8>,
    ) -> Result<()> {
        let client = self.client().await?;
        let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);

        let _ = secrets.delete(name, &Default::default()).await;
        sleep(Duration::from_millis(500)).await;

        let mut data = BTreeMap::new();
        data.insert("tls.crt".to_string(), ByteString(cert_data));
        data.insert("tls.key".to_string(), ByteString(key_data));

        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            data: Some(data),
            type_: Some("kubernetes.io/tls".to_string()),
            ..Default::default()
        };

        secrets.create(&PostParams::default(), &secret).await?;
        Ok(())
    }

    /// Delete a secret (ignores if not found)
    pub async fn delete_secret(&mut self, name: &str, namespace: &str) -> Result<()> {
        let client = self.client().await?;
        let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);
        let _ = secrets.delete(name, &Default::default()).await;
        Ok(())
    }

    // ==================== Service Operations ====================

    /// Check if a service exists
    pub async fn service_exists(&mut self, name: &str, namespace: &str) -> bool {
        if let Some(client) = self.try_client().await {
            let services: Api<Service> = Api::namespaced(client.clone(), namespace);
            services.get(name).await.is_ok()
        } else {
            false
        }
    }

    // ==================== Namespace Operations ====================

    /// List all namespaces
    pub async fn list_namespaces(&mut self) -> Result<Vec<String>> {
        let client = self.client().await?;
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let list = namespaces.list(&ListParams::default()).await?;
        Ok(list
            .items
            .into_iter()
            .filter_map(|ns| ns.metadata.name)
            .collect())
    }

    // ==================== Node Operations ====================

    /// List all nodes with details
    pub async fn list_nodes(&mut self) -> Result<Vec<NodeInfo>> {
        let client = self.client().await?;
        let nodes: Api<Node> = Api::all(client.clone());
        let list = nodes.list(&ListParams::default()).await?;

        Ok(list
            .items
            .into_iter()
            .map(|node| {
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

                let internal_ip = node
                    .status
                    .as_ref()
                    .and_then(|s| s.addresses.as_ref())
                    .and_then(|addrs| {
                        addrs
                            .iter()
                            .find(|a| a.type_ == "InternalIP")
                            .map(|a| a.address.clone())
                    });

                let roles = node
                    .metadata
                    .labels
                    .as_ref()
                    .map(|labels| {
                        labels
                            .keys()
                            .filter(|k| k.starts_with("node-role.kubernetes.io/"))
                            .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();

                let version = node
                    .status
                    .as_ref()
                    .and_then(|s| s.node_info.as_ref())
                    .map(|ni| ni.kubelet_version.clone())
                    .unwrap_or_default();

                NodeInfo {
                    name,
                    status,
                    roles,
                    internal_ip,
                    version,
                }
            })
            .collect())
    }

    // ==================== Pod Operations ====================

    /// List pods in a namespace
    pub async fn list_pods(&mut self, namespace: &str) -> Result<Vec<PodInfo>> {
        let client = self.client().await?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let list = pods.list(&ListParams::default()).await?;

        Ok(list
            .items
            .into_iter()
            .map(|pod| {
                let name = pod.metadata.name.unwrap_or_default();
                let status = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.clone())
                    .unwrap_or_else(|| "Unknown".to_string());
                let ready_count = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .map(|cs| cs.iter().filter(|c| c.ready).count())
                    .unwrap_or(0);
                let total_count = pod.spec.as_ref().map(|s| s.containers.len()).unwrap_or(0);

                PodInfo {
                    name,
                    status,
                    ready: format!("{}/{}", ready_count, total_count),
                }
            })
            .collect())
    }

    /// List all pods across all namespaces (for tunnel detection)
    pub async fn list_all_pods(&mut self) -> Result<Vec<PodFullInfo>> {
        let client = self.client().await?;
        let pods: Api<Pod> = Api::all(client.clone());
        let list = pods.list(&ListParams::default()).await?;

        Ok(list
            .items
            .into_iter()
            .map(|pod| {
                let name = pod.metadata.name.clone().unwrap_or_default();
                let namespace = pod.metadata.namespace.clone().unwrap_or_default();

                let containers: Vec<ContainerInfo> = pod
                    .spec
                    .as_ref()
                    .map(|spec| {
                        spec.containers
                            .iter()
                            .map(|c| {
                                let ports: Vec<u16> = c
                                    .ports
                                    .as_ref()
                                    .map(|ps| {
                                        ps.iter()
                                            .filter_map(|p| p.container_port.try_into().ok())
                                            .collect()
                                    })
                                    .unwrap_or_default();

                                ContainerInfo {
                                    image: c.image.clone().unwrap_or_default(),
                                    ports,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                PodFullInfo {
                    name,
                    namespace,
                    containers,
                }
            })
            .collect())
    }

    // ==================== Ingress Operations ====================

    /// List all ingresses across all namespaces
    pub async fn list_ingresses(&mut self) -> Result<Vec<IngressInfo>> {
        let client = self.client().await?;

        // Use k8s_openapi Ingress type
        use k8s_openapi::api::networking::v1::Ingress;
        let ingresses: Api<Ingress> = Api::all(client.clone());

        match ingresses.list(&ListParams::default()).await {
            Ok(list) => {
                let mut result = Vec::new();
                for ingress in list.items {
                    if let Some(spec) = ingress.spec {
                        if let Some(rules) = spec.rules {
                            for rule in rules {
                                let host = rule.host.unwrap_or_default();
                                if host.is_empty() {
                                    continue;
                                }

                                let paths: Vec<String> = rule
                                    .http
                                    .map(|http| {
                                        http.paths
                                            .iter()
                                            .map(|p| {
                                                p.path.clone().unwrap_or_else(|| "/".to_string())
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_else(|| vec!["/".to_string()]);

                                result.push(IngressInfo { host, paths });
                            }
                        }
                    }
                }
                Ok(result)
            }
            Err(_) => Ok(Vec::new()),
        }
    }

    /// List all Traefik IngressRoutes (CRD)
    pub async fn list_ingressroutes(&mut self) -> Result<Vec<IngressRouteInfo>> {
        let client = self.client().await?;

        // Define the IngressRoute API resource
        let ar = ApiResource {
            group: "traefik.io".to_string(),
            version: "v1alpha1".to_string(),
            kind: "IngressRoute".to_string(),
            api_version: "traefik.io/v1alpha1".to_string(),
            plural: "ingressroutes".to_string(),
        };

        let ingressroutes: Api<DynamicObject> = Api::all_with(client.clone(), &ar);

        match ingressroutes.list(&ListParams::default()).await {
            Ok(list) => {
                let mut result = Vec::new();
                for ir in list.items {
                    if let Some(spec) = ir.data.get("spec") {
                        if let Some(routes) = spec.get("routes").and_then(|r| r.as_array()) {
                            for route in routes {
                                if let Some(match_str) = route.get("match").and_then(|m| m.as_str())
                                {
                                    // Extract host using lazy-compiled regex
                                    if let Some(cap) = HOST_REGEX.captures(match_str) {
                                        if let Some(host) = cap.get(1) {
                                            let host = host.as_str().to_string();

                                            // Extract path using lazy-compiled regex
                                            let path = PATH_PREFIX_REGEX
                                                .captures(match_str)
                                                .and_then(|c| c.get(1))
                                                .map(|p| p.as_str().to_string())
                                                .unwrap_or_else(|| "/".to_string());

                                            result.push(IngressRouteInfo { host, path });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(result)
            }
            Err(_) => Ok(Vec::new()), // CRD might not exist
        }
    }

    // ==================== Custom Resource Operations ====================

    /// Apply a YAML manifest (for HelmChartConfig, etc.)
    pub async fn apply_yaml(&mut self, yaml_content: &str) -> Result<()> {
        let client = self.client().await?;

        // Parse the YAML to get resource info
        let value: serde_yaml::Value = serde_yaml::from_str(yaml_content)?;
        let api_version = value
            .get("apiVersion")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing apiVersion"))?;
        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing kind"))?;
        let metadata = value
            .get("metadata")
            .ok_or_else(|| anyhow!("Missing metadata"))?;
        let name = metadata
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing metadata.name"))?;
        let namespace = metadata
            .get("namespace")
            .and_then(|v| v.as_str())
            .unwrap_or("default");

        // Parse apiVersion to get group and version
        let (group, version) = if api_version.contains('/') {
            let parts: Vec<&str> = api_version.split('/').collect();
            (parts[0].to_string(), parts[1].to_string())
        } else {
            (String::new(), api_version.to_string())
        };

        // Create ApiResource
        let ar = ApiResource {
            group: group.clone(),
            version: version.clone(),
            kind: kind.to_string(),
            api_version: api_version.to_string(),
            plural: format!("{}s", kind.to_lowercase()), // Simple pluralization
        };

        // Convert to DynamicObject
        let obj: DynamicObject = serde_yaml::from_str(yaml_content)?;

        // Create namespaced or cluster-scoped API
        let api: Api<DynamicObject> = if namespace == "default" && group.is_empty() {
            Api::all_with(client.clone(), &ar)
        } else {
            Api::namespaced_with(client.clone(), namespace, &ar)
        };

        // Try to patch (update) first, create if it doesn't exist
        match api
            .patch(name, &PatchParams::apply("k3dev"), &Patch::Apply(&obj))
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => {
                api.create(&PostParams::default(), &obj).await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Delete a custom resource
    pub async fn delete_custom_resource(
        &mut self,
        api_version: &str,
        kind: &str,
        name: &str,
        namespace: &str,
    ) -> Result<()> {
        let client = self.client().await?;

        let (group, version) = if api_version.contains('/') {
            let parts: Vec<&str> = api_version.split('/').collect();
            (parts[0].to_string(), parts[1].to_string())
        } else {
            (String::new(), api_version.to_string())
        };

        let ar = ApiResource {
            group,
            version: version.clone(),
            kind: kind.to_string(),
            api_version: api_version.to_string(),
            plural: format!("{}s", kind.to_lowercase()),
        };

        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);
        let _ = api.delete(name, &Default::default()).await;
        Ok(())
    }

    // ==================== Cluster Info ====================

    /// Get Kubernetes version
    pub async fn get_version(&mut self) -> Result<String> {
        let client = self.client().await?;
        let version = client.apiserver_version().await?;
        Ok(format!(
            "Server Version: v{}.{}",
            version.major, version.minor
        ))
    }

    // ==================== Kubeconfig Management ====================

    /// Remove cluster, context, and user entries from kubeconfig
    /// This replaces `kubectl config delete-cluster/context/user`
    pub async fn cleanup_kubeconfig_entries(
        cluster_name: &str,
        context_name: &str,
        user_name: &str,
    ) -> Result<()> {
        let kubeconfig_path = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot find home directory"))?
            .join(".kube")
            .join("config");

        if !kubeconfig_path.exists() {
            return Ok(());
        }

        let mut kubeconfig = Kubeconfig::read_from(&kubeconfig_path)?;

        kubeconfig.clusters.retain(|c| c.name != cluster_name);
        kubeconfig.contexts.retain(|c| c.name != context_name);
        kubeconfig.auth_infos.retain(|a| a.name != user_name);

        if kubeconfig.current_context.as_deref() == Some(context_name) {
            kubeconfig.current_context = kubeconfig.contexts.first().map(|c| c.name.clone());
        }

        let yaml_content = serde_yaml::to_string(&kubeconfig)?;
        tokio::fs::write(&kubeconfig_path, yaml_content).await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&kubeconfig_path, perms)?;
        }

        Ok(())
    }
}

impl Default for KubeOps {
    fn default() -> Self {
        Self::new()
    }
}

// ==================== Info Types ====================

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub name: String,
    pub status: String,
    pub roles: String,
    pub internal_ip: Option<String>,
    pub version: String,
}

impl NodeInfo {
    pub fn to_wide_string(&self) -> String {
        format!(
            "{:<20} {:<10} {:<15} {:<15} {}",
            self.name,
            self.status,
            if self.roles.is_empty() {
                "<none>"
            } else {
                &self.roles
            },
            self.internal_ip.as_deref().unwrap_or("<none>"),
            self.version
        )
    }
}

#[derive(Debug, Clone)]
pub struct PodInfo {
    pub name: String,
    pub status: String,
    pub ready: String,
}

impl PodInfo {
    pub fn to_string_line(&self) -> String {
        format!("{:<50} {:<10} {}", self.name, self.ready, self.status)
    }
}

#[derive(Debug, Clone)]
pub struct PodFullInfo {
    pub name: String,
    pub namespace: String,
    pub containers: Vec<ContainerInfo>,
}

#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub image: String,
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct IngressInfo {
    pub host: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IngressRouteInfo {
    pub host: String,
    pub path: String,
}
