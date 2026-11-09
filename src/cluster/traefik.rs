use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::config::ClusterConfig;
use super::kube_ops::KubeOps;
use super::local_ca;
use crate::ui::components::OutputLine;

/// Traefik ingress controller manager
/// Configures K3s built-in Traefik with custom TLS certificates
pub struct TraefikManager {
    config: Arc<ClusterConfig>,
    kube_ops: KubeOps,
}

impl TraefikManager {
    pub fn new(config: Arc<ClusterConfig>) -> Self {
        Self {
            kube_ops: KubeOps::for_cluster(&config),
            config,
        }
    }

    /// Configure Traefik ingress controller (K3s built-in)
    pub async fn deploy(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info(
                "Configuring Traefik ingress controller...",
            ))
            .await;

        // Setup TLS certificates
        let ca = self.setup_certificates(&output_tx).await?;

        // Create TLS secret
        self.create_tls_secret(&output_tx).await?;

        // Trust the CA while Traefik comes up: the sudo prompt may wait on the
        // user, and Traefik does not need it.
        let (_, configured) = tokio::join!(local_ca::ensure_trusted(&ca, &output_tx), async {
            // Apply HelmChartConfig to customize K3s built-in Traefik
            self.apply_traefik_config(&output_tx).await?;
            // Wait for Traefik to be ready
            self.wait_for_traefik(&output_tx).await
        });
        configured?;

        let _ = output_tx
            .send(OutputLine::success("Traefik configured successfully"))
            .await;
        Ok(())
    }

    /// Make sure this cluster has a current TLS certificate signed by the
    /// k3dev root CA, regenerating whatever is missing, expiring, or no longer
    /// acceptable. Returns the CA.
    async fn setup_certificates(
        &self,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<local_ca::CertKeyPem> {
        let certs_dir = self.config.certs_dir();
        fs::create_dir_all(&certs_dir).await?;

        let cert_path = certs_dir.join("local-cert.pem");
        let key_path = certs_dir.join("local-key.pem");
        let domain = &self.config.domain;

        let ca = local_ca::ensure_ca(output_tx).await?;

        if key_path.exists() {
            if let Ok(leaf) = fs::read_to_string(&cert_path).await {
                match local_ca::check_leaf(&leaf, &ca.cert, domain, chrono::Utc::now()) {
                    Ok(_) => {
                        let _ = output_tx
                            .send(OutputLine::info("Using existing certificates"))
                            .await;
                        return Ok(ca);
                    }
                    Err(reason) => {
                        let _ = output_tx
                            .send(OutputLine::info(format!(
                                "Regenerating TLS certificate (current one {})",
                                reason
                            )))
                            .await;
                    }
                }
            }
        }

        let _ = output_tx
            .send(OutputLine::info("Generating TLS certificates..."))
            .await;
        let leaf = local_ca::generate_leaf(&ca, domain, chrono::Utc::now())
            .map_err(|e| anyhow!("Failed to generate TLS certificate: {:#}", e))?;
        local_ca::write_private(&key_path, &leaf.key).await?;
        fs::write(&cert_path, &leaf.cert).await?;

        let _ = output_tx
            .send(OutputLine::success("Certificates generated"))
            .await;
        Ok(ca)
    }

    /// Create Kubernetes TLS secret
    async fn create_tls_secret(&mut self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let certs_dir = self.config.certs_dir();
        let cert_path = certs_dir.join("local-cert.pem");
        let key_path = certs_dir.join("local-key.pem");

        if !cert_path.exists() || !key_path.exists() {
            let _ = output_tx
                .send(OutputLine::warning(
                    "Certificates not found, skipping TLS secret",
                ))
                .await;
            return Ok(());
        }

        let _ = output_tx
            .send(OutputLine::info("Creating TLS secret..."))
            .await;

        // Read certificate files
        let cert_data = fs::read(&cert_path).await?;
        let key_data = fs::read(&key_path).await?;

        // Create TLS secret using kube API
        self.kube_ops
            .create_tls_secret("traefik-tls", "kube-system", cert_data, key_data)
            .await?;

        Ok(())
    }

    /// Apply HelmChartConfig to customize K3s built-in Traefik
    async fn apply_traefik_config(&mut self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Applying Traefik configuration..."))
            .await;

        let dashboard_domain = self.config.traefik_dashboard_domain();
        let match_rule = format!(
            "Host(`{}`) && (PathPrefix(`/dashboard`) || PathPrefix(`/api`))",
            dashboard_domain
        );

        // HelmChartConfig to customize K3s built-in Traefik.
        // These nodePorts are *in-cluster* and stay 80/443 for every cluster —
        // `http_port`/`https_port` are host-side only, mapped by the router or a
        // direct publish. Conflating them would give a cluster on host 8080 a
        // NodePort of 8080.
        let helm_chart_config = format!(
            r#"apiVersion: helm.cattle.io/v1
kind: HelmChartConfig
metadata:
  name: traefik
  namespace: kube-system
spec:
  valuesContent: |-
    ports:
      web:
        nodePort: {http_port}
      websecure:
        nodePort: {https_port}
    service:
      type: NodePort
    tlsStore:
      default:
        defaultCertificate:
          secretName: traefik-tls
    providers:
      kubernetesCRD:
        allowExternalNameServices: true
        allowEmptyServices: true
    ingressRoute:
      dashboard:
        enabled: true
        matchRule: "{match_rule}"
        entryPoints:
          - websecure
"#,
            http_port = crate::cluster::config::NODEPORT_HTTP,
            https_port = crate::cluster::config::NODEPORT_HTTPS,
            match_rule = match_rule,
        );

        // Apply via kube API
        self.kube_ops.apply_yaml(&helm_chart_config).await?;

        Ok(())
    }

    /// Wait for Traefik deployment to be ready
    async fn wait_for_traefik(&mut self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Waiting for Traefik to be ready..."))
            .await;

        let timeout_secs = 180;
        let start = std::time::Instant::now();

        while start.elapsed().as_secs() < timeout_secs {
            match self
                .kube_ops
                .get_deployment_ready_replicas("traefik", "kube-system")
                .await
            {
                Ok(replicas) if replicas > 0 => return Ok(()),
                _ => {}
            }

            if start.elapsed().as_secs().is_multiple_of(30) && start.elapsed().as_secs() > 0 {
                let _ = output_tx
                    .send(OutputLine::info(format!(
                        "Still waiting for Traefik... ({}/{}s)",
                        start.elapsed().as_secs(),
                        timeout_secs
                    )))
                    .await;
            }

            sleep(Duration::from_secs(2)).await;
        }

        Err(anyhow!("Timeout waiting for Traefik"))
    }
}
