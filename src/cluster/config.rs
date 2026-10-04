use std::path::PathBuf;

use crate::config::{HooksConfig, InfrastructureConfig, SpeedupConfig};

/// In-cluster Traefik NodePort for HTTP. Distinct from `http_port`, which is
/// only the host-side port; the router (or a direct publish) maps one to the other.
pub const NODEPORT_HTTP: u16 = 80;

/// In-cluster Traefik NodePort for HTTPS
pub const NODEPORT_HTTPS: u16 = 443;

/// Unified cluster configuration settings
///
/// This struct combines all cluster-related configuration:
/// - K8s client settings (kubeconfig, context)
/// - K3s cluster settings (version, ports, domain)
/// - Container settings (name, network)
/// - Feature flags and hooks
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    // K8s client settings
    pub kubeconfig: Option<String>,
    pub context: Option<String>,

    // Identity
    pub cluster_name: String,
    /// Stable per-cluster index used to derive non-overlapping CIDRs
    pub cluster_index: u16,

    // Kubernetes
    pub k3s_version: String,
    pub k3s_image_repo: String,
    pub domain: String,

    // Container settings
    pub container_name: String,
    pub network_name: String,

    // Ports
    pub api_port: u16,
    pub http_port: u16,
    pub https_port: u16,
    pub additional_ports: Vec<(u16, u16)>,

    /// Route host :80/:443 through the shared k3dev-router instead of
    /// publishing them directly from the cluster container
    pub use_router: bool,

    /// Image used for the shared front router
    pub router_image: String,

    // Speedup optimizations
    pub speedup: SpeedupConfig,

    // Hooks
    pub hooks: HooksConfig,
}

/// Parse additional ports from string format "host:container" to tuple
fn parse_port_mapping(s: &str) -> Option<(u16, u16)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 2 {
        let host = parts[0].parse().ok()?;
        let container = parts[1].parse().ok()?;
        Some((host, container))
    } else {
        None
    }
}

impl From<InfrastructureConfig> for ClusterConfig {
    fn from(infra: InfrastructureConfig) -> Self {
        let additional_ports: Vec<(u16, u16)> = infra
            .additional_ports
            .iter()
            .filter_map(|s| parse_port_mapping(s))
            .collect();

        let container_name = infra.container_name();
        let network_name = infra.network_name();
        // A pinned index is recorded in the registry as well, so the allocator
        // cannot later hand the same one to another cluster — two clusters on
        // one index share their pod, service and DNS CIDRs.
        let cluster_index = match infra.cluster_index {
            Some(index) => super::registry::reserve_index(&infra.cluster_name, index),
            None => super::registry::resolve_index(&infra.cluster_name),
        };

        Self {
            kubeconfig: None,
            context: Some(infra.cluster_name.clone()),
            cluster_name: infra.cluster_name,
            cluster_index,
            k3s_version: infra.k3s_version,
            k3s_image_repo: infra.k3s_image_repo,
            domain: infra.domain,
            container_name,
            network_name,
            api_port: infra.api_port,
            http_port: infra.http_port,
            https_port: infra.https_port,
            additional_ports,
            use_router: infra.router.enabled,
            router_image: infra.router.image,
            speedup: infra.speedup,
            hooks: HooksConfig::default(),
        }
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        let infra = InfrastructureConfig::default();
        let container_name = infra.container_name();
        let network_name = infra.network_name();

        Self {
            kubeconfig: None,
            context: Some(infra.cluster_name.clone()),

            cluster_name: infra.cluster_name,
            cluster_index: 0,

            k3s_version: infra.k3s_version,
            k3s_image_repo: infra.k3s_image_repo,
            domain: infra.domain,

            container_name,
            network_name,

            api_port: infra.api_port,
            http_port: infra.http_port,
            https_port: infra.https_port,
            additional_ports: vec![(2345, 2345), (8309, 8309)],

            use_router: infra.router.enabled,
            router_image: infra.router.image,

            speedup: SpeedupConfig::default(),

            hooks: HooksConfig::default(),
        }
    }
}

impl ClusterConfig {
    /// Get the k3s image name
    pub fn k3s_image(&self) -> String {
        format!("{}:{}", self.k3s_image_repo, self.k3s_version)
    }

    /// Get kubeconfig path
    pub fn kubeconfig_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".kube")
            .join("config")
    }

    /// Root of the k3dev state directory
    pub fn state_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".k3dev")
    }

    /// Per-cluster certificates directory (the CA stays shared, one level up)
    pub fn certs_dir(&self) -> PathBuf {
        Self::state_dir().join("certs").join(&self.cluster_name)
    }

    /// Standalone kubeconfig pinned to this cluster.
    ///
    /// Hooks and custom commands are pointed here so they reach this cluster
    /// without depending on the user's global current-context, which k3dev
    /// deliberately leaves alone.
    pub fn pinned_kubeconfig_path(&self) -> PathBuf {
        Self::state_dir()
            .join("kubeconfig")
            .join(format!("{}.yaml", self.cluster_name))
    }

    /// The pinned kubeconfig, if it has been written for this cluster yet
    pub fn pinned_kubeconfig(&self) -> Option<PathBuf> {
        let path = self.pinned_kubeconfig_path();
        path.exists().then_some(path)
    }

    /// Environment for host-side commands, info blocks and visibility checks.
    ///
    /// Mirrors what hooks get: a bare `kubectl` then reaches this cluster
    /// rather than whatever the user's current-context points at — which may
    /// be a context left over from a deleted cluster whose CA no longer
    /// matches the one listening on the API port.
    pub fn host_command_env(&self) -> Vec<(String, String)> {
        let mut env = vec![("K3DEV_CONTEXT".to_string(), self.context_name())];
        if let Some(path) = self.pinned_kubeconfig() {
            env.push((
                "KUBECONFIG".to_string(),
                path.to_string_lossy().into_owned(),
            ));
        }
        env
    }

    // === Per-cluster resource names ===

    /// Docker volume holding /var/lib/rancher/k3s for this cluster
    pub fn rancher_volume_name(&self) -> String {
        format!("{}-rancher-data", self.cluster_name)
    }

    /// Docker volume holding local-path PV data for this cluster
    pub fn local_pv_volume_name(&self) -> String {
        format!("{}-local-pv-data", self.cluster_name)
    }

    /// Host path where the local PV volume is mounted (also used inside the container)
    pub fn local_pv_storage_path(&self, docker_root: &str) -> String {
        format!(
            "{}/volumes/{}/_data",
            docker_root.trim_end_matches('/'),
            self.local_pv_volume_name()
        )
    }

    /// Kubelet root-dir. Kubelet keeps flat, unkeyed state files here, so two
    /// clusters sharing one root corrupt each other's manager state.
    pub fn kubelet_root_dir(&self, docker_root: &str) -> String {
        format!(
            "{}/kubelet-{}",
            docker_root.trim_end_matches('/'),
            self.cluster_name
        )
    }

    /// Kubelet cgroup root. Kubelet reconciles away pod cgroups it does not
    /// recognize, so each cluster needs its own subtree of the host hierarchy.
    pub fn cgroup_root(&self) -> String {
        format!("/kubepods-{}", self.cluster_name)
    }

    /// Absolute path of the cgroup root in the unified hierarchy
    pub fn cgroup_root_path(&self) -> String {
        format!("/sys/fs/cgroup{}", self.cgroup_root())
    }

    /// K8s node name for this cluster's single server node
    pub fn node_name(&self) -> String {
        format!("{}-server", self.cluster_name)
    }

    /// Marker appended to this cluster's /etc/hosts lines. The bracket form
    /// keeps a `contains` match safe against an `a` / `ab` prefix trap.
    pub fn hosts_marker(&self) -> String {
        format!("# k3dev-ingress[{}]", self.cluster_name)
    }

    /// Prefix shared by all of this cluster's snapshot images
    pub fn snapshot_prefix(&self) -> String {
        format!("k3dev-snapshot-{}-", self.cluster_name)
    }

    /// Kubeconfig cluster/user/context name for this cluster
    pub fn context_name(&self) -> String {
        self.cluster_name.clone()
    }

    // === Per-cluster networking ===

    /// Pod CIDR: 10.{42 + 2i}.0.0/16 (index 0 == the k3s default)
    pub fn cluster_cidr(&self) -> String {
        format!("10.{}.0.0/16", 42 + 2 * self.cluster_index)
    }

    /// Service CIDR: 10.{43 + 2i}.0.0/16
    pub fn service_cidr(&self) -> String {
        format!("10.{}.0.0/16", 43 + 2 * self.cluster_index)
    }

    /// Cluster DNS address — must live inside `service_cidr`, since CoreDNS's
    /// manifest pins a fixed ClusterIP the apiserver validates against the range.
    pub fn cluster_dns(&self) -> String {
        format!("10.{}.0.10", 43 + 2 * self.cluster_index)
    }

    /// Get all port mappings as docker format strings.
    ///
    /// The API port is always published directly (it cannot be Host- or
    /// SNI-routed). HTTP/HTTPS are published only when the shared router is
    /// disabled — otherwise the router owns host :80/:443 and reaches each
    /// cluster over its Docker network.
    pub fn port_mappings(&self) -> Vec<String> {
        let mut ports = vec![format!("{}:6443", self.api_port)];

        if !self.use_router {
            ports.push(format!("{}:{}", self.http_port, NODEPORT_HTTP));
            ports.push(format!("{}:{}", self.https_port, NODEPORT_HTTPS));
        }

        for (host, container) in &self.additional_ports {
            ports.push(format!("{}:{}", host, container));
        }

        ports
    }

    /// Get traefik dashboard domain
    pub fn traefik_dashboard_domain(&self) -> String {
        format!("traefik.{}", self.domain)
    }

    /// Get wildcard domain for certificates
    pub fn wildcard_domain(&self) -> String {
        format!("*.{}", self.domain)
    }

    /// Builder method to set hooks configuration
    pub fn with_hooks(mut self, hooks: HooksConfig) -> Self {
        self.hooks = hooks;
        self
    }

    /// Builder method to set K8s client configuration.
    ///
    /// An explicitly configured context wins; otherwise the per-cluster context
    /// derived from `cluster_name` is kept, so k3dev never silently follows
    /// whatever `current-context` happens to be.
    pub fn with_k8s_config(mut self, kubeconfig: Option<String>, context: Option<String>) -> Self {
        if kubeconfig.is_some() {
            self.kubeconfig = kubeconfig;
        }
        if context.is_some() {
            self.context = context;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k3s_image_uses_default_ghcr_repo() {
        let config = ClusterConfig::default();
        assert_eq!(
            config.k3s_image(),
            format!("ghcr.io/daylioti/k3dev-k3s:{}", config.k3s_version)
        );
    }

    fn named(cluster: &str, index: u16) -> ClusterConfig {
        ClusterConfig {
            cluster_name: cluster.to_string(),
            cluster_index: index,
            ..Default::default()
        }
    }

    #[test]
    fn per_cluster_resources_never_collide() {
        let a = named("alpha", 0);
        let b = named("beta", 1);

        assert_ne!(a.rancher_volume_name(), b.rancher_volume_name());
        assert_ne!(a.local_pv_volume_name(), b.local_pv_volume_name());
        assert_ne!(
            a.kubelet_root_dir("/var/lib/docker"),
            b.kubelet_root_dir("/var/lib/docker")
        );
        assert_ne!(a.cgroup_root(), b.cgroup_root());
        assert_ne!(a.hosts_marker(), b.hosts_marker());
        assert_ne!(a.snapshot_prefix(), b.snapshot_prefix());
        assert_ne!(a.certs_dir(), b.certs_dir());
        assert_ne!(a.cluster_cidr(), b.cluster_cidr());
        assert_ne!(a.service_cidr(), b.service_cidr());
    }

    #[test]
    fn index_zero_keeps_the_k3s_default_cidrs() {
        let a = named("alpha", 0);
        assert_eq!(a.cluster_cidr(), "10.42.0.0/16");
        assert_eq!(a.service_cidr(), "10.43.0.0/16");
        assert_eq!(a.cluster_dns(), "10.43.0.10");
    }

    #[test]
    fn cluster_dns_stays_inside_the_service_cidr() {
        for index in 0..8 {
            let c = named("c", index);
            let service_prefix = c.service_cidr().trim_end_matches("0.0/16").to_string();
            assert!(
                c.cluster_dns().starts_with(&service_prefix),
                "dns {} outside service cidr {}",
                c.cluster_dns(),
                c.service_cidr()
            );
            // Pod and service ranges must not overlap either
            assert_ne!(c.cluster_cidr(), c.service_cidr());
        }
    }

    /// The registry caps the index; that cap has to keep both CIDRs inside a
    /// valid octet, or k3s is handed a nonsense `--cluster-cidr`.
    #[test]
    fn the_highest_allowed_index_still_yields_valid_octets() {
        let c = named("c", super::super::registry::MAX_INDEX);
        for value in [c.cluster_cidr(), c.service_cidr(), c.cluster_dns()] {
            let octet: u32 = value
                .split('.')
                .nth(1)
                .expect("dotted quad")
                .parse()
                .expect("numeric octet");
            assert!(octet <= 255, "{} has an invalid octet", value);
        }
    }

    #[test]
    fn router_mode_leaves_http_ports_to_the_router() {
        let with_router = ClusterConfig {
            use_router: true,
            additional_ports: vec![],
            ..Default::default()
        };
        assert_eq!(with_router.port_mappings(), vec!["6443:6443".to_string()]);

        let direct = ClusterConfig {
            use_router: false,
            additional_ports: vec![],
            http_port: 8080,
            ..Default::default()
        };
        // Host port maps onto the in-cluster NodePort, which never changes
        assert!(direct.port_mappings().contains(&"8080:80".to_string()));
        assert!(direct.port_mappings().contains(&"443:443".to_string()));
    }

    #[test]
    fn k3s_image_honors_overridden_repo() {
        let config = ClusterConfig {
            k3s_image_repo: "rancher/k3s".to_string(),
            k3s_version: "v1.30.0-k3s1".to_string(),
            ..Default::default()
        };
        assert_eq!(config.k3s_image(), "rancher/k3s:v1.30.0-k3s1");
    }

    #[test]
    fn host_command_env_without_a_pinned_kubeconfig_only_names_the_context() {
        let env = named("k3dev-test-never-started", 0).host_command_env();

        assert_eq!(
            env,
            vec![(
                "K3DEV_CONTEXT".to_string(),
                "k3dev-test-never-started".to_string()
            )]
        );
    }
}
