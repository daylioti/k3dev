use std::path::PathBuf;

use crate::config::{HooksConfig, InfrastructureConfig, SpeedupConfig};

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

    // Kubernetes
    pub k3s_version: String,
    pub domain: String,

    // Container settings
    pub container_name: String,
    pub network_name: String,

    // Ports
    pub api_port: u16,
    pub http_port: u16,
    pub https_port: u16,
    pub additional_ports: Vec<(u16, u16)>,

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

        Self {
            kubeconfig: None,
            context: None,
            k3s_version: infra.k3s_version,
            domain: infra.domain,
            container_name,
            network_name,
            api_port: infra.api_port,
            http_port: infra.http_port,
            https_port: infra.https_port,
            additional_ports,
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
            context: None,

            k3s_version: infra.k3s_version,
            domain: infra.domain,

            container_name,
            network_name,

            api_port: infra.api_port,
            http_port: infra.http_port,
            https_port: infra.https_port,
            additional_ports: vec![(2345, 2345), (8309, 8309)],

            speedup: SpeedupConfig::default(),

            hooks: HooksConfig::default(),
        }
    }
}

impl ClusterConfig {
    /// Get the k3s image name
    pub fn k3s_image(&self) -> String {
        format!("rancher/k3s:{}", self.k3s_version)
    }

    /// Get kubeconfig path
    #[allow(dead_code)]
    pub fn kubeconfig_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".kube")
            .join("config")
    }

    /// Get certificates directory
    pub fn certs_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".k3dev")
            .join("certs")
    }

    /// Get all port mappings as docker format strings
    pub fn port_mappings(&self) -> Vec<String> {
        let mut ports = vec![
            format!("{}:{}", self.api_port, self.api_port),
            format!("{}:{}", self.http_port, self.http_port),
            format!("{}:{}", self.https_port, self.https_port),
        ];

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

    /// Builder method to set K8s client configuration
    pub fn with_k8s_config(mut self, kubeconfig: Option<String>, context: Option<String>) -> Self {
        self.kubeconfig = kubeconfig;
        self.context = context;
        self
    }
}
