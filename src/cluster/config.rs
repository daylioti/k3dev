use std::path::PathBuf;

use super::docker::{DockerManager, PortMapping};
use crate::config::{HooksConfig, InfrastructureConfig, SpeedupConfig};

/// In-cluster Traefik NodePort for HTTP. Distinct from `http_port`, which is
/// only the host-side port; the router (or a direct publish) maps one to the other.
pub const NODEPORT_HTTP: u16 = 80;

/// In-cluster Traefik NodePort for HTTPS
pub const NODEPORT_HTTPS: u16 = 443;

/// Port the k3s API server listens on inside the cluster container. Also the
/// base of the automatic host port: an unset `api_port` resolves to this plus
/// the cluster index, so index 0 publishes it unchanged.
pub const K3S_API_PORT: u16 = 6443;

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
    /// Host port for the API: the configured one, or derived from
    /// `cluster_index` when the config leaves it unset
    pub api_port: u16,
    /// `api_port` was derived rather than set in the config
    pub api_port_auto: bool,
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

/// A port key as written in the config file
fn setting(key: &str) -> String {
    format!("`infrastructure.{}`", key)
}

/// The API host port: a pinned value as is, otherwise offset by the cluster
/// index. Indices are unique and capped, so derived ports never collide with
/// each other.
fn resolve_api_port(pinned: Option<u16>, cluster_index: u16) -> u16 {
    pinned.unwrap_or(K3S_API_PORT + cluster_index)
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
            api_port: resolve_api_port(infra.api_port, cluster_index),
            api_port_auto: infra.api_port.is_none(),
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

            api_port: resolve_api_port(infra.api_port, 0),
            api_port_auto: infra.api_port.is_none(),
            http_port: infra.http_port,
            https_port: infra.https_port,
            additional_ports: Vec::new(),

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

    /// The ports the cluster container publishes.
    ///
    /// The API port is always published directly (it cannot be Host- or
    /// SNI-routed). HTTP/HTTPS are published only when the shared router is
    /// disabled — otherwise the router owns host :80/:443 and reaches each
    /// cluster over its Docker network.
    ///
    /// The kubeconfig points at the Docker host: 127.0.0.1 when Docker is
    /// local, so the API port binds loopback only and stays off the network.
    /// With a remote Docker host the kubeconfig names that host, so the port
    /// has to be reachable on its interfaces.
    pub fn port_mappings(&self, docker_remote: bool) -> Vec<PortMapping> {
        let mut ports = vec![if docker_remote {
            PortMapping::new(self.api_port, K3S_API_PORT)
        } else {
            PortMapping::loopback(self.api_port, K3S_API_PORT)
        }];

        if !self.use_router {
            ports.push(PortMapping::new(self.http_port, NODEPORT_HTTP));
            ports.push(PortMapping::new(self.https_port, NODEPORT_HTTPS));
        }

        for (host, container) in &self.additional_ports {
            ports.push(PortMapping::new(*host, *container));
        }

        ports
    }

    /// The settings that publish `host_port` from the cluster container (see
    /// [`Self::port_mappings`]), named for an error that says what to change.
    /// More than one means the container collides with itself.
    pub fn port_settings(&self, host_port: u16) -> Vec<String> {
        let mut settings = Vec::new();
        if host_port == self.api_port {
            if self.api_port_auto {
                settings.push(format!(
                    "{} (unset, so derived from the cluster index)",
                    setting("api_port")
                ));
            } else {
                settings.push(setting("api_port"));
            }
        }
        if !self.use_router {
            settings.extend(self.router_port_settings(host_port));
        }
        for (host, container) in &self.additional_ports {
            if *host == host_port {
                settings.push(format!(
                    "the `{}:{}` entry in {}",
                    host,
                    container,
                    setting("additional_ports")
                ));
            }
        }
        settings
    }

    /// Like [`Self::port_settings`], for `http_port` and `https_port` alone:
    /// the shared router publishes those, or the cluster container without it
    pub fn router_port_settings(&self, host_port: u16) -> Vec<String> {
        let mut settings = Vec::new();
        if host_port == self.http_port {
            settings.push(setting("http_port"));
        }
        if host_port == self.https_port {
            settings.push(setting("https_port"));
        }
        settings
    }

    /// Host port the API is reached on. A container keeps the port it was
    /// created with, which the config may since resolve differently (`api_port`
    /// changed or removed), so an existing container's binding wins over
    /// [`Self::api_port`].
    pub async fn live_api_port(&self, docker: &DockerManager) -> u16 {
        docker
            .published_host_port(&self.container_name, K3S_API_PORT)
            .await
            .unwrap_or(self.api_port)
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
    fn an_unset_api_port_is_derived_from_the_cluster_index() {
        assert_eq!(resolve_api_port(None, 0), 6443);
        assert_eq!(resolve_api_port(None, 3), 6446);
        assert_eq!(
            resolve_api_port(None, super::super::registry::MAX_INDEX),
            6543
        );
    }

    #[test]
    fn a_pinned_api_port_wins_over_the_index() {
        assert_eq!(resolve_api_port(Some(7000), 3), 7000);
        assert_eq!(resolve_api_port(Some(6443), 3), 6443);
    }

    #[test]
    fn the_default_config_keeps_the_k3s_api_port() {
        let config = ClusterConfig::default();
        assert_eq!(config.cluster_index, 0);
        assert_eq!(config.api_port, 6443);
        assert!(config.api_port_auto);
    }

    #[test]
    fn router_mode_leaves_http_ports_to_the_router() {
        let with_router = ClusterConfig {
            use_router: true,
            additional_ports: vec![],
            ..Default::default()
        };
        assert_eq!(
            with_router.port_mappings(false),
            vec![PortMapping::loopback(6443, 6443)]
        );

        let direct = ClusterConfig {
            use_router: false,
            additional_ports: vec![],
            http_port: 8080,
            ..Default::default()
        };
        // Host port maps onto the in-cluster NodePort, which never changes
        let ports = direct.port_mappings(false);
        assert!(ports.contains(&PortMapping::new(8080, 80)));
        assert!(ports.contains(&PortMapping::new(443, 443)));
    }

    #[test]
    fn api_port_binds_loopback_unless_docker_is_remote() {
        let config = ClusterConfig {
            use_router: false,
            api_port: 6451,
            additional_ports: vec![(2345, 2345)],
            ..Default::default()
        };
        let local = config.port_mappings(false);
        assert_eq!(local[0], PortMapping::loopback(6451, 6443));
        // Only the API port: ingress and extra ports stay reachable as before
        assert!(local[1..].iter().all(|port| !port.loopback));

        let remote = config.port_mappings(true);
        assert!(remote.iter().all(|port| !port.loopback));
        assert_eq!(remote[0], PortMapping::new(6451, 6443));
    }

    #[test]
    fn every_published_port_has_one_setting() {
        for use_router in [true, false] {
            let config = ClusterConfig {
                use_router,
                api_port: 6451,
                http_port: 8080,
                https_port: 8443,
                additional_ports: vec![(2345, 2345)],
                ..Default::default()
            };
            for mapping in config.port_mappings(false) {
                assert_eq!(config.port_settings(mapping.host).len(), 1, "{:?}", mapping);
            }
        }
    }

    #[test]
    fn port_settings_name_the_config_key() {
        let config = ClusterConfig {
            use_router: false,
            api_port: 6451,
            api_port_auto: false,
            http_port: 8080,
            https_port: 8443,
            additional_ports: vec![(2345, 2346)],
            ..Default::default()
        };
        assert_eq!(config.port_settings(6451), ["`infrastructure.api_port`"]);
        assert_eq!(config.port_settings(8080), ["`infrastructure.http_port`"]);
        assert_eq!(config.port_settings(8443), ["`infrastructure.https_port`"]);
        assert_eq!(
            config.port_settings(2345),
            ["the `2345:2346` entry in `infrastructure.additional_ports`"]
        );
        assert!(config.port_settings(9999).is_empty());

        // With the router, the cluster container does not publish HTTP(S);
        // the router does
        let routed = ClusterConfig {
            use_router: true,
            ..config
        };
        assert!(routed.port_settings(8080).is_empty());
        assert_eq!(
            routed.router_port_settings(8080),
            ["`infrastructure.http_port`"]
        );
    }

    /// Pinning a free port is still the fix, but the key is absent from the
    /// config file, so the message says where the port came from
    #[test]
    fn an_auto_api_port_says_it_was_derived() {
        let config = ClusterConfig {
            cluster_index: 2,
            api_port: 6445,
            api_port_auto: true,
            ..Default::default()
        };
        assert_eq!(
            config.port_settings(6445),
            ["`infrastructure.api_port` (unset, so derived from the cluster index)"]
        );
    }

    #[test]
    fn a_host_port_published_twice_names_both_settings() {
        let config = ClusterConfig {
            use_router: false,
            http_port: 8080,
            additional_ports: vec![(8080, 8080)],
            ..Default::default()
        };
        assert_eq!(
            config.port_settings(8080),
            [
                "`infrastructure.http_port`",
                "the `8080:8080` entry in `infrastructure.additional_ports`"
            ]
        );
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
