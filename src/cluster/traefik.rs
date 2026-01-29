use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;
use x509_parser::prelude::*;

use super::config::ClusterConfig;
use super::kube_ops::KubeOps;
use crate::ui::components::OutputLine;

/// Traefik ingress controller manager
/// Configures K3s built-in Traefik with custom TLS certificates
pub struct TraefikManager {
    config: ClusterConfig,
    kube_ops: KubeOps,
}

impl TraefikManager {
    pub fn new(config: ClusterConfig) -> Self {
        Self {
            config,
            kube_ops: KubeOps::new(),
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
        self.setup_certificates(&output_tx).await?;

        // Create TLS secret
        self.create_tls_secret(&output_tx).await?;

        // Apply HelmChartConfig to customize K3s built-in Traefik
        self.apply_traefik_config(&output_tx).await?;

        // Wait for Traefik to be ready
        self.wait_for_traefik(&output_tx).await?;

        let _ = output_tx
            .send(OutputLine::success("Traefik configured successfully"))
            .await;
        Ok(())
    }

    /// Setup TLS certificates using mkcert
    async fn setup_certificates(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let certs_dir = ClusterConfig::certs_dir();
        fs::create_dir_all(&certs_dir).await?;

        let cert_path = certs_dir.join("local-cert.pem");
        let key_path = certs_dir.join("local-key.pem");

        // Check if certificates exist, are valid, and match the configured domain
        if cert_path.exists() && key_path.exists() {
            let (is_valid, domain_matches) = self.check_certificate(&cert_path);
            if is_valid && domain_matches {
                let _ = output_tx
                    .send(OutputLine::info("Using existing certificates"))
                    .await;
                return Ok(());
            } else {
                let _ = output_tx
                    .send(OutputLine::info(
                        "Certificates need regeneration (expired or domain mismatch)",
                    ))
                    .await;
            }
        }

        // Check if mkcert is installed
        if which::which("mkcert").is_err() {
            let _ = output_tx
                .send(OutputLine::warning(
                    "mkcert not installed, skipping certificate generation",
                ))
                .await;
            let _ = output_tx.send(OutputLine::warning("Install with: brew install mkcert (macOS) or see https://github.com/FiloSottile/mkcert")).await;
            return Ok(());
        }

        let _ = output_tx
            .send(OutputLine::info("Generating TLS certificates..."))
            .await;

        // Install mkcert CA in system and browser trust stores
        let _ = output_tx
            .send(OutputLine::info(
                "Installing mkcert CA (may require sudo)...",
            ))
            .await;
        let install_output = Command::new("mkcert").arg("-install").output().await;

        if let Ok(out) = &install_output {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "mkcert CA install warning: {}",
                        stderr.trim()
                    )))
                    .await;
            }
        }

        // Try to install CA in Firefox NSS database if certutil is available
        self.install_ca_in_firefox(output_tx).await;

        // Generate certificates
        let domain = &self.config.domain;
        let wildcard = self.config.wildcard_domain();

        let output = Command::new("mkcert")
            .current_dir(&certs_dir)
            .args([
                "-cert-file",
                "local-cert.pem",
                "-key-file",
                "local-key.pem",
                domain,
                &wildcard,
                "localhost",
                "127.0.0.1",
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!("Failed to generate certificates"));
        }

        let _ = output_tx
            .send(OutputLine::success("Certificates generated"))
            .await;
        Ok(())
    }

    /// Install mkcert CA in Firefox NSS database (Firefox uses its own trust store)
    async fn install_ca_in_firefox(&self, output_tx: &mpsc::Sender<OutputLine>) {
        // Check if certutil is available
        if which::which("certutil").is_err() {
            return;
        }

        // Get mkcert CA root path
        let ca_root = Command::new("mkcert").arg("-CAROOT").output().await;

        let ca_root_path = match ca_root {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => return,
        };

        let ca_cert_path = format!("{}/rootCA.pem", ca_root_path);
        if !std::path::Path::new(&ca_cert_path).exists() {
            return;
        }

        // Find Firefox profiles
        let home = std::env::var("HOME").unwrap_or_default();
        let firefox_dir = format!("{}/.mozilla/firefox", home);

        if let Ok(entries) = std::fs::read_dir(&firefox_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if name.contains("default") {
                        let nss_db = format!("sql:{}", path.display());

                        // Check if CA already exists
                        let check = Command::new("certutil")
                            .args(["-d", &nss_db, "-L"])
                            .output()
                            .await;

                        if let Ok(out) = check {
                            let list = String::from_utf8_lossy(&out.stdout);
                            if list.contains("mkcert") {
                                continue; // Already installed
                            }
                        }

                        // Install CA
                        let result = Command::new("certutil")
                            .args([
                                "-d",
                                &nss_db,
                                "-A",
                                "-t",
                                "C,,",
                                "-n",
                                "mkcert CA",
                                "-i",
                                &ca_cert_path,
                            ])
                            .output()
                            .await;

                        if let Ok(out) = result {
                            if out.status.success() {
                                let _ = output_tx
                                    .send(OutputLine::info("Installed mkcert CA in Firefox"))
                                    .await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Check certificate validity and domain match
    fn check_certificate(&self, cert_path: &PathBuf) -> (bool, bool) {
        let pem_data = match std::fs::read(cert_path) {
            Ok(data) => data,
            Err(_) => return (false, false),
        };

        let pem_parsed = match ::pem::parse(&pem_data) {
            Ok(p) => p,
            Err(_) => return (false, false),
        };

        let (_, cert) = match X509Certificate::from_der(pem_parsed.contents()) {
            Ok(c) => c,
            Err(_) => return (false, false),
        };

        // Check validity
        let now = chrono::Utc::now().timestamp();
        let not_before = cert.validity().not_before.timestamp();
        let not_after = cert.validity().not_after.timestamp();
        let is_valid = now >= not_before && now <= not_after;

        // Check domain
        let mut domain_matches = false;
        if let Ok(Some(san_ext)) = cert.subject_alternative_name() {
            for name in &san_ext.value.general_names {
                if let GeneralName::DNSName(dns) = name {
                    if *dns == self.config.domain {
                        domain_matches = true;
                        break;
                    }
                    // Check wildcard
                    if let Some(wildcard_base) = dns.strip_prefix("*.") {
                        if wildcard_base == self.config.domain
                            || self.config.domain.ends_with(&format!(".{}", wildcard_base))
                        {
                            domain_matches = true;
                            break;
                        }
                    }
                }
            }
        }

        (is_valid, domain_matches)
    }

    /// Create Kubernetes TLS secret
    async fn create_tls_secret(&mut self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let certs_dir = ClusterConfig::certs_dir();
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

        // HelmChartConfig to customize K3s built-in Traefik
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
            http_port = self.config.http_port,
            https_port = self.config.https_port,
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

    /// Uninstall Traefik configuration (removes HelmChartConfig, K3s will reset to defaults)
    pub async fn uninstall(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Removing Traefik configuration..."))
            .await;

        // Delete the HelmChartConfig (K3s will redeploy Traefik with defaults)
        let _ = self
            .kube_ops
            .delete_custom_resource(
                "helm.cattle.io/v1",
                "HelmChartConfig",
                "traefik",
                "kube-system",
            )
            .await;

        // Delete TLS secret
        let _ = self
            .kube_ops
            .delete_secret("traefik-tls", "kube-system")
            .await;

        Ok(())
    }
}
