//! Kubernetes operations using the kube crate (replaces kubectl commands)

use anyhow::{anyhow, Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Namespace, Node, Pod, Secret, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use kube::api::{Api, DynamicObject, ListParams, Patch, PatchParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::discovery::ApiResource;
use kube::{Client, Config};
use once_cell::sync::Lazy;
use regex::Regex;
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
    /// Kubeconfig path override (None = default `~/.kube/config` discovery)
    kubeconfig: Option<String>,
    /// Kubeconfig context to bind to. Without this every k3dev process would
    /// drive whichever cluster wrote `current-context` last.
    context: Option<String>,
}

impl KubeOps {
    pub fn new() -> Self {
        Self {
            client: None,
            kubeconfig: None,
            context: None,
        }
    }

    /// Bind this KubeOps to one cluster's kubeconfig context
    pub fn for_cluster(config: &crate::cluster::ClusterConfig) -> Self {
        Self {
            client: None,
            kubeconfig: config.kubeconfig.clone(),
            context: config.context.clone(),
        }
    }

    /// Bind to an explicit (kubeconfig, context) pair — for spawned tasks that
    /// can only carry owned strings across the await boundary.
    pub fn with_context(kubeconfig: Option<String>, context: Option<String>) -> Self {
        Self {
            client: None,
            kubeconfig,
            context,
        }
    }

    /// Build a client config, honoring the bound context when one is set and
    /// falling back to ambient inference otherwise.
    async fn build_config(&self) -> Result<Config> {
        let context = self.context.as_deref().filter(|s| !s.is_empty());
        let kubeconfig_path = self.kubeconfig.as_deref().filter(|s| !s.is_empty());

        if context.is_none() && kubeconfig_path.is_none() {
            return Ok(Config::infer().await?);
        }

        let path = match kubeconfig_path {
            Some(p) => crate::config::expand_home(std::path::Path::new(p))?,
            None => dirs::home_dir()
                .ok_or_else(|| anyhow!("Cannot find home directory"))?
                .join(".kube")
                .join("config"),
        };

        let kubeconfig = Kubeconfig::read_from(&path)?;
        let options = KubeConfigOptions {
            context: context.map(String::from),
            ..Default::default()
        };
        Ok(Config::from_custom_kubeconfig(kubeconfig, &options).await?)
    }

    /// Get or create the kube client
    async fn client(&mut self) -> Result<&Client> {
        if self.client.is_none() {
            let config = self.build_config().await?;
            let client = Client::try_from(config)?;
            self.client = Some(client);
        }
        // Safety: client is guaranteed to be Some after the above initialization
        Ok(self.client.as_ref().expect("client was just initialized"))
    }

    /// Try to get client, returns None if cluster not accessible
    async fn try_client(&mut self) -> Option<&Client> {
        if self.client.is_none() {
            let config = self.build_config().await.ok()?;
            let client = Client::try_from(config).ok()?;
            self.client = Some(client);
        }
        self.client.as_ref()
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
        let value: serde_yml::Value = serde_yml::from_str(yaml_content)?;
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
        let obj: DynamicObject = serde_yml::from_str(yaml_content)?;

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

    /// Rewrite the raw `k3s.yaml` into a standalone, single-cluster kubeconfig.
    ///
    /// k3s names its cluster/user/context `default`; they are renamed to `name`
    /// and the server URL is replaced with `server`. `current-context` points at
    /// this cluster, so anything handed this file as `KUBECONFIG` talks to it
    /// regardless of what the user's global current-context happens to be.
    pub fn rename_kubeconfig_entries(
        generated: Kubeconfig,
        name: &str,
        server: &str,
    ) -> Result<Kubeconfig> {
        let mut cluster = generated
            .clusters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("k3s kubeconfig has no cluster entry"))?;
        let mut auth_info = generated
            .auth_infos
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("k3s kubeconfig has no user entry"))?;
        let mut context = generated
            .contexts
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("k3s kubeconfig has no context entry"))?;

        cluster.name = name.to_string();
        if let Some(c) = cluster.cluster.as_mut() {
            c.server = Some(server.to_string());
        }
        auth_info.name = name.to_string();
        context.name = name.to_string();
        if let Some(c) = context.context.as_mut() {
            c.cluster = name.to_string();
            c.user = Some(name.to_string());
        }

        Ok(Kubeconfig {
            api_version: Some("v1".to_string()),
            kind: Some("Config".to_string()),
            clusters: vec![cluster],
            auth_infos: vec![auth_info],
            contexts: vec![context],
            current_context: Some(name.to_string()),
            ..Default::default()
        })
    }

    /// Merge one cluster's entries into an existing kubeconfig.
    ///
    /// Each entry is merged retain-by-name-then-push so unrelated contexts
    /// survive untouched.
    pub async fn merge_kubeconfig_entries(
        path: &std::path::Path,
        standalone: &Kubeconfig,
        name: &str,
    ) -> Result<()> {
        // Never fall back to an empty config on a parse failure: writing that
        // back would replace every context the user has with just this one.
        // (An empty or missing file legitimately parses as the default.)
        let mut merged = if path.exists() {
            Kubeconfig::read_from(path).with_context(|| {
                format!(
                    "Failed to parse existing kubeconfig {}; refusing to overwrite it",
                    path.display()
                )
            })?
        } else {
            Kubeconfig::default()
        };

        merged.clusters.retain(|c| c.name != name);
        merged.clusters.extend(standalone.clusters.iter().cloned());
        merged.auth_infos.retain(|a| a.name != name);
        merged
            .auth_infos
            .extend(standalone.auth_infos.iter().cloned());
        merged.contexts.retain(|c| c.name != name);
        merged.contexts.extend(standalone.contexts.iter().cloned());

        // Point at the cluster we just started, but only claim the slot if
        // nothing valid is there — switching a user's context out from under
        // them is exactly the destructiveness this merge exists to avoid.
        // Hooks never rely on this; they get the standalone file instead.
        let current_valid = merged
            .current_context
            .as_ref()
            .is_some_and(|cur| merged.contexts.iter().any(|c| &c.name == cur));
        if !current_valid {
            merged.current_context = Some(name.to_string());
        }

        if merged.api_version.is_none() {
            merged.api_version = Some("v1".to_string());
        }
        if merged.kind.is_none() {
            merged.kind = Some("Config".to_string());
        }

        Self::write_kubeconfig(path, &merged).await
    }

    /// Write a kubeconfig with owner-only permissions
    pub async fn write_kubeconfig(path: &std::path::Path, config: &Kubeconfig) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, serde_yml::to_string(config)?).await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }

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

        let yaml_content = serde_yml::to_string(&kubeconfig)?;
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

#[cfg(test)]
mod kubeconfig_tests {
    use super::*;
    use kube::config::Kubeconfig;

    /// k3s emits everything under the name `default`
    fn k3s_yaml() -> Kubeconfig {
        Kubeconfig::from_yaml(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: default
  cluster:
    server: https://127.0.0.1:6443
    certificate-authority-data: Q0E=
users:
- name: default
  user:
    client-certificate-data: Q0VSVA==
contexts:
- name: default
  context:
    cluster: default
    user: default
current-context: default
"#,
        )
        .unwrap()
    }

    #[test]
    fn rename_produces_standalone_config_pinned_to_this_cluster() {
        let standalone =
            KubeOps::rename_kubeconfig_entries(k3s_yaml(), "k3dev", "https://127.0.0.1:7443")
                .unwrap();

        assert_eq!(standalone.clusters[0].name, "k3dev");
        assert_eq!(standalone.auth_infos[0].name, "k3dev");
        assert_eq!(standalone.contexts[0].name, "k3dev");
        assert_eq!(
            standalone.clusters[0].cluster.as_ref().unwrap().server,
            Some("https://127.0.0.1:7443".to_string())
        );
        // The whole point: a hook pointed at this file lands on this cluster
        // no matter what the user's global current-context says.
        assert_eq!(standalone.current_context, Some("k3dev".to_string()));
    }

    #[tokio::test]
    async fn merge_keeps_unrelated_contexts_and_refreshes_stale_own_entry() {
        let dir = std::env::temp_dir().join("k3dev-kubeconfig-merge-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("config");
        tokio::fs::write(
            &path,
            r#"
apiVersion: v1
kind: Config
clusters:
- name: work
  cluster:
    server: https://work.example:6443
users:
- name: work
  user: {}
contexts:
- name: work
  context:
    cluster: work
    user: work
current-context: work
"#,
        )
        .await
        .unwrap();

        let standalone =
            KubeOps::rename_kubeconfig_entries(k3s_yaml(), "k3dev", "https://127.0.0.1:6443")
                .unwrap();
        KubeOps::merge_kubeconfig_entries(&path, &standalone, "k3dev")
            .await
            .unwrap();

        let merged = Kubeconfig::read_from(&path).unwrap();
        let names: Vec<_> = merged.contexts.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"work"));
        assert!(names.contains(&"k3dev"));
        // A valid user context is never stolen
        assert_eq!(merged.current_context, Some("work".to_string()));
    }

    /// A kubeconfig that does not parse must abort the merge: treating it as
    /// empty would write this cluster back over every context the user has.
    #[tokio::test]
    async fn merge_refuses_to_clobber_a_kubeconfig_it_cannot_parse() {
        let dir = std::env::temp_dir().join("k3dev-kubeconfig-broken-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("config");
        let broken = "clusters: [oops\n";
        tokio::fs::write(&path, broken).await.unwrap();

        let standalone =
            KubeOps::rename_kubeconfig_entries(k3s_yaml(), "k3dev", "https://127.0.0.1:6443")
                .unwrap();

        assert!(
            KubeOps::merge_kubeconfig_entries(&path, &standalone, "k3dev")
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), broken);
    }

    /// An empty `~/.kube/config` is not a parse failure — the merge still has to
    /// produce this cluster's entries.
    #[tokio::test]
    async fn merge_into_an_empty_file_writes_this_cluster() {
        let dir = std::env::temp_dir().join("k3dev-kubeconfig-empty-test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("config");
        tokio::fs::write(&path, "").await.unwrap();

        let standalone =
            KubeOps::rename_kubeconfig_entries(k3s_yaml(), "k3dev", "https://127.0.0.1:6443")
                .unwrap();
        KubeOps::merge_kubeconfig_entries(&path, &standalone, "k3dev")
            .await
            .unwrap();

        let merged = Kubeconfig::read_from(&path).unwrap();
        assert_eq!(merged.contexts.len(), 1);
        assert_eq!(merged.contexts[0].name, "k3dev");
        assert_eq!(merged.current_context, Some("k3dev".to_string()));
    }
}
