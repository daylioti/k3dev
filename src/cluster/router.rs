use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs;
use tokio::sync::mpsc;

use super::config::{ClusterConfig, NODEPORT_HTTP, NODEPORT_HTTPS};
use super::docker::{ContainerRunConfig, DockerManager};
use crate::ui::components::OutputLine;

/// Name of the shared front-router container
pub const ROUTER_CONTAINER: &str = "k3dev-router";

/// Where the router reads its file-provider config from, inside the container
const DYNAMIC_DIR_IN_CONTAINER: &str = "/etc/traefik/dynamic";

/// Router entrypoint ports inside the router container. The host-side ports are
/// per-cluster config (`http_port` / `https_port`); these are fixed.
const ENTRYPOINT_HTTP: u16 = 80;
const ENTRYPOINT_HTTPS: u16 = 443;

/// Shared front-router: one Traefik container owning host :80/:443 that fans out
/// to every running cluster by Host header (HTTP) and SNI (HTTPS).
///
/// Each cluster contributes one file in a watched dynamic-config directory, so
/// clusters can start and stop independently without restarting the router.
pub struct RouterManager {
    image: String,
}

impl RouterManager {
    pub fn new(image: String) -> Self {
        Self { image }
    }

    /// Host directory bind-mounted into the router as its dynamic config dir
    fn dynamic_dir() -> PathBuf {
        ClusterConfig::state_dir().join("router").join("dynamic")
    }

    fn config_path(cluster_name: &str) -> PathBuf {
        Self::dynamic_dir().join(format!("{}.yml", cluster_name))
    }

    /// Cluster names that currently have a dynamic config file
    async fn configured_clusters() -> Vec<String> {
        let mut clusters = Vec::new();
        let Ok(mut entries) = fs::read_dir(Self::dynamic_dir()).await else {
            return clusters;
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                clusters.push(stem.to_string());
            }
        }

        clusters
    }

    /// Register a cluster with the router and make sure the router is up
    pub async fn ensure(
        &self,
        docker: &DockerManager,
        config: &ClusterConfig,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let dir = Self::dynamic_dir();
        fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("Failed to create router config dir {}", dir.display()))?;

        let path = Self::config_path(&config.cluster_name);
        fs::write(&path, dynamic_config_yaml(config))
            .await
            .with_context(|| format!("Failed to write router config {}", path.display()))?;

        self.ensure_container(docker, config, output_tx).await?;

        docker
            .connect_network(&config.network_name, ROUTER_CONTAINER)
            .await
            .with_context(|| {
                format!("Failed to attach router to network {}", config.network_name)
            })?;

        let _ = output_tx
            .send(OutputLine::success(format!(
                "Router serving {} on ports {}/{}",
                config.domain, config.http_port, config.https_port
            )))
            .await;

        Ok(())
    }

    /// Create the router container, or start it if it exists but is stopped
    async fn ensure_container(
        &self,
        docker: &DockerManager,
        config: &ClusterConfig,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        if docker.container_exists(ROUTER_CONTAINER).await {
            if !docker.container_running(ROUTER_CONTAINER).await {
                let _ = output_tx
                    .send(OutputLine::info("Starting shared router..."))
                    .await;
                docker
                    .start_container(ROUTER_CONTAINER)
                    .await
                    .context("Failed to start router container")?;
            }
            // The router is shared, so its host ports are whichever cluster
            // created it. A later cluster asking for different ones is not
            // served on them — say so instead of failing silently.
            Self::warn_on_port_mismatch(docker, config, output_tx).await;
            return Ok(());
        }

        if !docker.image_exists(&self.image).await {
            let _ = output_tx
                .send(OutputLine::info(format!(
                    "Pulling router image {}...",
                    self.image
                )))
                .await;
            docker
                .pull_image(&self.image)
                .await
                .with_context(|| format!("Failed to pull router image {}", self.image))?;
        }

        let _ = output_tx
            .send(OutputLine::info("Creating shared router..."))
            .await;

        let mut labels = HashMap::new();
        labels.insert("k3dev.role".to_string(), "router".to_string());

        let run_config = ContainerRunConfig {
            name: ROUTER_CONTAINER.to_string(),
            hostname: Some(ROUTER_CONTAINER.to_string()),
            image: self.image.clone(),
            detach: true,
            privileged: false,
            ports: vec![
                (config.http_port, ENTRYPOINT_HTTP),
                (config.https_port, ENTRYPOINT_HTTPS),
            ],
            volumes: vec![(
                Self::dynamic_dir().to_string_lossy().to_string(),
                DYNAMIC_DIR_IN_CONTAINER.to_string(),
                String::new(),
            )],
            env: Vec::new(),
            network: Some(config.network_name.clone()),
            cgroupns_host: false,
            pid_host: false,
            entrypoint: None,
            command: Some(vec![
                format!("--providers.file.directory={}", DYNAMIC_DIR_IN_CONTAINER),
                "--providers.file.watch=true".to_string(),
                format!("--entrypoints.web.address=:{}", ENTRYPOINT_HTTP),
                format!("--entrypoints.websecure.address=:{}", ENTRYPOINT_HTTPS),
            ]),
            security_opt: Vec::new(),
            labels,
            auto_remove: false,
        };

        if let Err(e) = docker.run_container(&run_config).await {
            // Two clusters starting at once both find no router and both try to
            // create it; the loser gets a name conflict. The router is shared,
            // so the winner's container is exactly what this call wanted.
            if !docker.container_exists(ROUTER_CONTAINER).await {
                return Err(e).context("Failed to create router container");
            }
            tracing::debug!("Router already created concurrently: {:#}", e);
            if !docker.container_running(ROUTER_CONTAINER).await {
                docker
                    .start_container(ROUTER_CONTAINER)
                    .await
                    .context("Failed to start router container")?;
            }
        }

        Ok(())
    }

    /// Report ports this cluster asked for that the existing router does not own
    async fn warn_on_port_mismatch(
        docker: &DockerManager,
        config: &ClusterConfig,
        output_tx: &mpsc::Sender<OutputLine>,
    ) {
        for (wanted, entrypoint, scheme) in [
            (config.http_port, ENTRYPOINT_HTTP, "HTTP"),
            (config.https_port, ENTRYPOINT_HTTPS, "HTTPS"),
        ] {
            let actual = docker
                .published_host_port(ROUTER_CONTAINER, entrypoint)
                .await;
            if let Some(actual) = actual {
                if actual != wanted {
                    let _ = output_tx
                        .send(OutputLine::warning(format!(
                            "Router already listens on {} port {}, not {} —                              recreate it (stop every cluster) to change {} ports",
                            scheme, actual, wanted, scheme
                        )))
                        .await;
                }
            }
        }
    }

    /// Unregister a cluster from the router.
    ///
    /// Must run *before* the caller removes the cluster network: while the
    /// router still holds an endpoint on it, `remove_network` fails with
    /// "has active endpoints".
    pub async fn release(
        &self,
        docker: &DockerManager,
        config: &ClusterConfig,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        self.forget_cluster(docker, &config.cluster_name, &config.network_name)
            .await;
        self.remove_if_idle(docker, output_tx).await;
        Ok(())
    }

    /// Drop router state for clusters whose server container is gone.
    ///
    /// Runs on startup so a crash mid-teardown does not leave the router
    /// advertising dead clusters. Never creates anything.
    pub async fn reconcile(
        &self,
        docker: &DockerManager,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let running = docker.list_cluster_servers().await;

        for cluster in Self::configured_clusters().await {
            if running.contains(&cluster) {
                continue;
            }

            let network = format!("{}-net", cluster);
            self.forget_cluster(docker, &cluster, &network).await;
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Router: dropped stale route for cluster '{}'",
                    cluster
                )))
                .await;
        }

        self.remove_if_idle(docker, output_tx).await;
        Ok(())
    }

    /// Remove a cluster's dynamic config and detach the router from its network
    async fn forget_cluster(&self, docker: &DockerManager, cluster: &str, network: &str) {
        let path = Self::config_path(cluster);
        if let Err(e) = fs::remove_file(&path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("Failed to remove {}: {}", path.display(), e);
            }
        }

        // Best effort: the network may already be gone, which is not an error here
        if let Err(e) = docker.disconnect_network(network, ROUTER_CONTAINER).await {
            tracing::debug!("Router detach from {} failed: {}", network, e);
        }
    }

    /// Tear the router down once no cluster routes remain
    async fn remove_if_idle(&self, docker: &DockerManager, output_tx: &mpsc::Sender<OutputLine>) {
        if !Self::configured_clusters().await.is_empty() {
            return;
        }
        if !docker.container_exists(ROUTER_CONTAINER).await {
            return;
        }

        match docker.remove_container(ROUTER_CONTAINER, true).await {
            Ok(()) => {
                let _ = output_tx
                    .send(OutputLine::info("Removed shared router (no clusters left)"))
                    .await;
            }
            Err(e) => {
                let _ = output_tx
                    .send(OutputLine::error(format!(
                        "Failed to remove router container: {}",
                        e
                    )))
                    .await;
            }
        }
    }

    /// Whether the router container is currently running
    pub async fn is_healthy(&self, docker: &DockerManager) -> bool {
        docker.container_running(ROUTER_CONTAINER).await
    }

    /// Whether the shared router is what publishes `host_port`.
    ///
    /// In router mode the cluster container deliberately does not publish
    /// HTTP/HTTPS, so "port already bound" on those is the expected state
    /// rather than a conflict — as long as it is the router holding them.
    pub async fn owns_host_port(&self, docker: &DockerManager, host_port: u16) -> bool {
        for entrypoint in [ENTRYPOINT_HTTP, ENTRYPOINT_HTTPS] {
            if docker
                .published_host_port(ROUTER_CONTAINER, entrypoint)
                .await
                == Some(host_port)
            {
                return true;
            }
        }
        false
    }
}

/// Escape a domain for use inside a Go regexp — only dots matter for hostnames
fn escape_domain(domain: &str) -> String {
    domain.replace('.', "\\.")
}

/// Build the Traefik file-provider dynamic config for one cluster.
///
/// HTTP is routed normally, HTTPS is a TCP router with TLS passthrough so each
/// cluster's own Traefik keeps terminating TLS with its own certificate.
/// Traefik v3 cannot express wildcards in `Host()`, hence the regexp form for
/// subdomains plus a literal match for the apex. Rules are single-quoted YAML
/// scalars so the regexp backslashes survive verbatim.
pub(crate) fn dynamic_config_yaml(config: &ClusterConfig) -> String {
    let name = &config.cluster_name;
    let domain = &config.domain;
    let pattern = format!("^.+\\.{}$", escape_domain(domain));
    let backend = &config.container_name;

    format!(
        r#"# Managed by k3dev - cluster '{name}'
http:
  routers:
    {name}:
      rule: 'HostRegexp(`{pattern}`) || Host(`{domain}`)'
      entryPoints:
        - web
      service: {name}
  services:
    {name}:
      loadBalancer:
        servers:
          - url: 'http://{backend}:{http_port}'
tcp:
  routers:
    {name}:
      rule: 'HostSNIRegexp(`{pattern}`) || HostSNI(`{domain}`)'
      entryPoints:
        - websecure
      service: {name}
      tls:
        passthrough: true
  services:
    {name}:
      loadBalancer:
        servers:
          - address: '{backend}:{https_port}'
"#,
        name = name,
        domain = domain,
        pattern = pattern,
        backend = backend,
        http_port = NODEPORT_HTTP,
        https_port = NODEPORT_HTTPS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha() -> ClusterConfig {
        ClusterConfig {
            cluster_name: "alpha".into(),
            domain: "a.dev".into(),
            container_name: "alpha-server".into(),
            network_name: "alpha-net".into(),
            ..Default::default()
        }
    }

    #[test]
    fn dynamic_config_escapes_domain_dots_in_regexes() {
        let yaml = dynamic_config_yaml(&alpha());

        assert!(yaml.contains("HostRegexp(`^.+\\.a\\.dev$`) || Host(`a.dev`)"));
        assert!(yaml.contains("HostSNIRegexp(`^.+\\.a\\.dev$`) || HostSNI(`a.dev`)"));
    }

    #[test]
    fn dynamic_config_points_at_cluster_nodeports() {
        let yaml = dynamic_config_yaml(&alpha());

        assert!(yaml.contains("url: 'http://alpha-server:80'"));
        assert!(yaml.contains("address: 'alpha-server:443'"));
        // HTTPS must stay end-to-end so the cluster's own Traefik terminates TLS
        assert!(yaml.contains("passthrough: true"));
    }
}
