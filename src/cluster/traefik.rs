use anyhow::{anyhow, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
};
use std::path::PathBuf;
use std::sync::Arc;
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
    config: Arc<ClusterConfig>,
    kube_ops: KubeOps,
}

impl TraefikManager {
    pub fn new(config: Arc<ClusterConfig>) -> Self {
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

    /// Get the CA directory path
    fn ca_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".k3dev")
            .join("ca")
    }

    /// Build CA certificate parameters (shared between generation and reconstruction)
    fn ca_params() -> Result<CertificateParams> {
        let mut params = CertificateParams::new(vec![])
            .map_err(|e| anyhow!("Failed to create CA params: {}", e))?;
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "k3dev Local CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "k3dev");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        Ok(params)
    }

    /// Setup TLS certificates using built-in rcgen CA
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

        let _ = output_tx
            .send(OutputLine::info("Generating TLS certificates..."))
            .await;

        // Ensure CA exists (generate if needed)
        let (_, ca_key_pem) = self.ensure_ca(output_tx).await?;

        // Parse CA key and reconstruct issuer for signing
        let ca_key =
            KeyPair::from_pem(&ca_key_pem).map_err(|e| anyhow!("Failed to parse CA key: {}", e))?;
        let ca_issuer = Issuer::new(Self::ca_params()?, ca_key);

        // Generate leaf certificate for the configured domain
        let domain = &self.config.domain;
        let wildcard = self.config.wildcard_domain();

        let subject_alt_names = vec![
            rcgen::SanType::DnsName(
                domain
                    .clone()
                    .try_into()
                    .map_err(|e| anyhow!("Invalid domain '{}': {}", domain, e))?,
            ),
            rcgen::SanType::DnsName(
                wildcard
                    .clone()
                    .try_into()
                    .map_err(|e| anyhow!("Invalid wildcard domain '{}': {}", wildcard, e))?,
            ),
            rcgen::SanType::DnsName(
                "localhost"
                    .try_into()
                    .map_err(|e| anyhow!("Invalid domain 'localhost': {}", e))?,
            ),
            rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()),
        ];

        let mut leaf_params = CertificateParams::new(vec![])
            .map_err(|e| anyhow!("Failed to create cert params: {}", e))?;
        leaf_params.subject_alt_names = subject_alt_names;
        leaf_params.distinguished_name = DistinguishedName::new();
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, domain.as_str());
        leaf_params
            .distinguished_name
            .push(DnType::OrganizationName, "k3dev");

        let leaf_key =
            KeyPair::generate().map_err(|e| anyhow!("Failed to generate leaf key: {}", e))?;
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &ca_issuer)
            .map_err(|e| anyhow!("Failed to sign leaf cert: {}", e))?;

        // Write leaf cert and key
        fs::write(&cert_path, leaf_cert.pem()).await?;
        fs::write(&key_path, leaf_key.serialize_pem()).await?;

        let _ = output_tx
            .send(OutputLine::success("Certificates generated"))
            .await;
        Ok(())
    }

    /// Ensure a CA certificate exists, generating one if needed.
    /// Returns (ca_cert_pem, ca_key_pem).
    async fn ensure_ca(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<(String, String)> {
        let ca_dir = Self::ca_dir();
        fs::create_dir_all(&ca_dir).await?;

        let ca_cert_path = ca_dir.join("rootCA.pem");
        let ca_key_path = ca_dir.join("rootCA-key.pem");

        // If CA already exists, return it
        if ca_cert_path.exists() && ca_key_path.exists() {
            let cert_pem = fs::read_to_string(&ca_cert_path).await?;
            let key_pem = fs::read_to_string(&ca_key_path).await?;
            return Ok((cert_pem, key_pem));
        }

        let _ = output_tx
            .send(OutputLine::info("Generating k3dev root CA..."))
            .await;

        // Generate new CA
        let ca_key =
            KeyPair::generate().map_err(|e| anyhow!("Failed to generate CA key: {}", e))?;
        let ca_cert = Self::ca_params()?
            .self_signed(&ca_key)
            .map_err(|e| anyhow!("Failed to self-sign CA: {}", e))?;

        let cert_pem = ca_cert.pem();
        let key_pem = ca_key.serialize_pem();

        fs::write(&ca_cert_path, &cert_pem).await?;
        fs::write(&ca_key_path, &key_pem).await?;

        // Install CA into trust stores
        self.install_ca(&ca_cert_path, output_tx).await;

        let _ = output_tx
            .send(OutputLine::success("Root CA generated and installed"))
            .await;

        Ok((cert_pem, key_pem))
    }

    /// Install CA certificate into system and browser trust stores
    async fn install_ca(
        &self,
        ca_cert_path: &std::path::Path,
        output_tx: &mpsc::Sender<OutputLine>,
    ) {
        let ca_path_str = ca_cert_path.to_string_lossy().to_string();

        // Install into system trust store (platform-specific)
        #[cfg(target_os = "linux")]
        {
            let _ = output_tx
                .send(OutputLine::info(
                    "Installing CA into system trust store (may require sudo)...",
                ))
                .await;

            // Detect distro trust store: Arch, Debian/Ubuntu, Fedora/RHEL
            let dest_dir_arch = "/etc/ca-certificates/trust-source/anchors";
            let dest_dir_debian = "/usr/local/share/ca-certificates";
            let dest_dir_rhel = "/etc/pki/ca-trust/source/anchors";

            let (dest_dir, cert_ext, update_cmd) = if std::path::Path::new(dest_dir_arch).exists() {
                (dest_dir_arch, "crt", "update-ca-trust")
            } else if std::path::Path::new(dest_dir_debian).exists() {
                (dest_dir_debian, "crt", "update-ca-certificates")
            } else if std::path::Path::new(dest_dir_rhel).exists() {
                (dest_dir_rhel, "pem", "update-ca-trust")
            } else {
                let _ = output_tx
                    .send(OutputLine::warning(
                        "Could not detect system CA trust store location",
                    ))
                    .await;
                ("", "", "")
            };

            if !dest_dir.is_empty() {
                let dest = format!("{}/k3dev-local-ca.{}", dest_dir, cert_ext);
                let cp = Command::new("sudo")
                    .args(["cp", &ca_path_str, &dest])
                    .output()
                    .await;
                if let Ok(out) = cp {
                    if out.status.success() {
                        let _ = Command::new("sudo").arg(update_cmd).output().await;
                    }
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            let _ = output_tx
                .send(OutputLine::info(
                    "Installing CA into system keychain (may require sudo)...",
                ))
                .await;

            let result = Command::new("sudo")
                .args([
                    "security",
                    "add-trusted-cert",
                    "-d",
                    "-r",
                    "trustRoot",
                    "-k",
                    "/Library/Keychains/System.keychain",
                    &ca_path_str,
                ])
                .output()
                .await;

            if let Ok(out) = &result {
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let _ = output_tx
                        .send(OutputLine::warning(format!(
                            "CA system install warning: {}",
                            stderr.trim()
                        )))
                        .await;
                }
            }
        }

        // Install into NSS databases (Firefox, Chrome/Chromium)
        self.install_ca_in_nss_databases(&ca_path_str, output_tx)
            .await;
    }

    /// Install CA in NSS databases used by Firefox and Chrome/Chromium
    async fn install_ca_in_nss_databases(
        &self,
        ca_cert_path: &str,
        output_tx: &mpsc::Sender<OutputLine>,
    ) {
        use super::platform::PlatformInfo;

        // Check if certutil is available
        if PlatformInfo::find_binary("certutil").is_none() {
            return;
        }

        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return,
        };

        // Collect all NSS database paths to install into
        let mut nss_dbs: Vec<(String, &str)> = Vec::new();

        // Chrome/Chromium on Linux uses ~/.pki/nssdb
        let chrome_nss = home.join(".pki").join("nssdb");
        if chrome_nss.exists() {
            nss_dbs.push((format!("sql:{}", chrome_nss.display()), "Chrome/Chromium"));
        }

        // Firefox profiles
        #[cfg(target_os = "macos")]
        let firefox_dir = home
            .join("Library")
            .join("Application Support")
            .join("Firefox")
            .join("Profiles");
        #[cfg(not(target_os = "macos"))]
        let firefox_dir = home.join(".mozilla").join("firefox");

        if let Ok(entries) = std::fs::read_dir(&firefox_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if name.contains("default") {
                        nss_dbs.push((format!("sql:{}", path.display()), "Firefox"));
                    }
                }
            }
        }

        // Install CA into each NSS database
        for (nss_db, browser) in &nss_dbs {
            // Check if CA already exists
            let check = Command::new("certutil")
                .args(["-d", nss_db, "-L"])
                .output()
                .await;

            if let Ok(out) = check {
                let list = String::from_utf8_lossy(&out.stdout);
                if list.contains("k3dev") {
                    continue; // Already installed
                }
            }

            // Install CA
            let result = Command::new("certutil")
                .args([
                    "-d",
                    nss_db,
                    "-A",
                    "-t",
                    "C,,",
                    "-n",
                    "k3dev Local CA",
                    "-i",
                    ca_cert_path,
                ])
                .output()
                .await;

            if let Ok(out) = result {
                if out.status.success() {
                    let _ = output_tx
                        .send(OutputLine::info(format!(
                            "Installed k3dev CA in {}",
                            browser
                        )))
                        .await;
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
    #[allow(dead_code)] // May be used for selective cleanup in the future
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
