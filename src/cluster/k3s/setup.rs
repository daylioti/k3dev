//! Cluster setup utilities
//!
//! This module contains methods for setting up the cluster:
//! - API readiness checking
//! - Socat binary installation
//! - Kubeconfig management
//! - Deployment readiness waiting

use anyhow::{anyhow, Context, Result};
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::K3sManager;
use crate::cluster::kube_ops::KubeOps;
use crate::cluster::platform::PlatformInfo;
use crate::ui::components::OutputLine;

impl K3sManager {
    /// Wait for k3s API to become accessible
    /// Uses async HTTP client with exponential backoff for faster detection
    pub(super) async fn wait_for_api(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Waiting for k3s API..."))
            .await;

        // Create HTTP client with short timeout and TLS disabled (self-signed cert)
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_millis(500))
            .build()
            .context("Failed to create HTTP client")?;

        let start_time = std::time::Instant::now();
        let mut interval = Duration::from_millis(100); // Start fast
        let max_interval = Duration::from_secs(2);
        let max_attempts = 40; // More attempts with faster initial intervals
        let mut last_progress_report = std::time::Instant::now();

        // Use remote host address when Docker is remote, otherwise localhost
        let api_host = PlatformInfo::docker_remote_host()
            .unwrap_or("127.0.0.1")
            .to_string();

        for attempt in 0..max_attempts {
            match client
                .get(format!("https://{}:6443/healthz", api_host))
                .send()
                .await
            {
                Ok(resp) => {
                    // 200 OK or 401 Unauthorized both mean API is up
                    // 401 means auth is required but server is responding
                    if resp.status().is_success() || resp.status() == 401 {
                        let elapsed = start_time.elapsed();
                        tracing::debug!(
                            "API available after {} attempts ({}ms)",
                            attempt + 1,
                            elapsed.as_millis()
                        );
                        return Ok(());
                    }
                    tracing::debug!("API check: status {}", resp.status());
                }
                Err(e) => {
                    tracing::debug!("API check failed: {}", e);
                }
            }

            // Report progress every 5 seconds
            if last_progress_report.elapsed() >= Duration::from_secs(5) {
                let elapsed = start_time.elapsed();
                let _ = output_tx
                    .send(OutputLine::info(format!(
                        "Still waiting for API... ({}s elapsed)",
                        elapsed.as_secs()
                    )))
                    .await;
                last_progress_report = std::time::Instant::now();
            }

            sleep(interval).await;
            // Exponential backoff: 100ms, 200ms, 400ms, 800ms, 1600ms, 2000ms (capped)
            interval = std::cmp::min(interval * 2, max_interval);
        }

        Err(anyhow!("Timeout waiting for k3s API"))
    }

    /// Install socat in the k3s container using embedded static binary
    /// The binary is compiled into k3dev, no network required
    /// Architecture-specific binary is selected at compile time
    pub(super) async fn install_socat(&self) -> Result<()> {
        // Static socat binary embedded at compile time (architecture-specific)
        #[cfg(target_arch = "x86_64")]
        const SOCAT_BINARY: &[u8] = include_bytes!("../../../assets/socat-x86_64");

        #[cfg(target_arch = "aarch64")]
        const SOCAT_BINARY: &[u8] = include_bytes!("../../../assets/socat-aarch64");

        // Check if socat is already installed
        if self
            .docker
            .exec_in_container(&self.config.container_name, &["which", "socat"])
            .await
            .is_ok()
        {
            return Ok(());
        }

        // Base64 encode the binary for injection via exec
        // This eliminates temp file I/O and docker cp overhead
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let encoded = STANDARD.encode(SOCAT_BINARY);

        // Inject binary directly via exec (no temp file, no docker cp)
        // Single atomic command: decode base64, write to file, and make executable
        let install_cmd = format!(
            "mkdir -p /usr/local/bin && \
             echo '{}' | base64 -d > /usr/local/bin/socat && \
             chmod +x /usr/local/bin/socat",
            encoded
        );

        self.docker
            .exec_in_container(&self.config.container_name, &["sh", "-c", &install_cmd])
            .await
            .context("Failed to install socat via binary injection")?;

        // Verify installation
        self.docker
            .exec_in_container(&self.config.container_name, &["socat", "-V"])
            .await
            .context("socat verification failed")?;

        Ok(())
    }

    /// Install k3dev-agent in the k3s container using embedded static binary.
    /// The agent collects cgroup stats and queries Docker for pod mapping.
    pub(super) async fn install_agent(&self) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        const AGENT_BINARY: &[u8] = include_bytes!("../../../assets/k3dev-agent-x86_64");

        #[cfg(target_arch = "aarch64")]
        const AGENT_BINARY: &[u8] = include_bytes!("../../../assets/k3dev-agent-aarch64");

        // Check if agent is already installed
        if self
            .docker
            .exec_in_container(
                &self.config.container_name,
                &["test", "-x", "/usr/local/bin/k3dev-agent"],
            )
            .await
            .is_ok()
        {
            return Ok(());
        }

        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let encoded = STANDARD.encode(AGENT_BINARY);

        let install_cmd = format!(
            "echo '{}' | base64 -d > /usr/local/bin/k3dev-agent && \
             chmod +x /usr/local/bin/k3dev-agent",
            encoded
        );

        self.docker
            .exec_in_container(&self.config.container_name, &["sh", "-c", &install_cmd])
            .await
            .context("Failed to install k3dev-agent")?;

        Ok(())
    }

    /// Setup kubeconfig file
    pub(super) async fn setup_kubeconfig(&self) -> Result<()> {
        let kube_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot find home directory"))?
            .join(".kube");

        // Create .kube directory if it doesn't exist
        fs::create_dir_all(&kube_dir).await?;

        let kubeconfig_path = kube_dir.join("config");
        let temp_config = kube_dir.join("k3s-config.tmp");

        // Wait for k3s to generate kubeconfig
        let max_retries = 30;
        for _ in 0..max_retries {
            let result = self
                .docker
                .exec_in_container(
                    &self.config.container_name,
                    &["cat", "/etc/rancher/k3s/k3s.yaml"],
                )
                .await;

            if let Ok(content) = result {
                if !content.is_empty() && content.contains("clusters:") {
                    // Replace 127.0.0.1 with the appropriate host:
                    // - Remote Docker: use the remote host's address
                    // - Local Docker: use "localhost" for better compatibility
                    let api_host = PlatformInfo::docker_remote_host()
                        .unwrap_or("localhost");
                    let fixed_content = content.replace("127.0.0.1", api_host);

                    fs::write(&temp_config, &fixed_content).await?;
                    fs::copy(&temp_config, &kubeconfig_path).await?;

                    // Set permissions
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mut perms = fs::metadata(&kubeconfig_path).await?.permissions();
                        perms.set_mode(0o600);
                        fs::set_permissions(&kubeconfig_path, perms).await?;
                    }

                    // Cleanup temp file
                    let _ = fs::remove_file(&temp_config).await;

                    return Ok(());
                }
            }

            sleep(Duration::from_secs(1)).await;
        }

        Err(anyhow!("Timeout waiting for kubeconfig"))
    }

    /// Cleanup kubeconfig entries
    pub(super) async fn cleanup_kubeconfig(&self) -> Result<()> {
        // Remove cluster, context, and user entries using kube crate
        // Ignore errors as entries might not exist
        let _ = KubeOps::cleanup_kubeconfig_entries("default", "default", "default").await;
        Ok(())
    }

    /// Wait for cluster to be fully ready
    pub(super) async fn wait_for_cluster_ready(
        &mut self,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Waiting for cluster components..."))
            .await;

        // Wait for core deployments in parallel for faster startup
        // Note: metrics-server and servicelb are disabled
        // We create separate KubeOps instances to avoid borrow checker issues
        let tx1 = output_tx.clone();
        let tx2 = output_tx.clone();

        let coredns_task = tokio::spawn(async move {
            let _ = tx1.send(OutputLine::info("Waiting for coredns...")).await;
            let mut kube_ops = KubeOps::new();
            match kube_ops
                .wait_for_deployment_ready("coredns", "kube-system", 60)
                .await
            {
                Ok(true) => Ok::<(), anyhow::Error>(()),
                Ok(false) => {
                    let _ = tx1
                        .send(OutputLine::warning(
                            "coredns not ready after 60s, continuing...",
                        ))
                        .await;
                    Ok(())
                }
                Err(_) => {
                    let _ = tx1
                        .send(OutputLine::warning(
                            "coredns not ready after 60s, continuing...",
                        ))
                        .await;
                    Ok(())
                }
            }
        });

        let provisioner_task = tokio::spawn(async move {
            let _ = tx2
                .send(OutputLine::info("Waiting for local-path-provisioner..."))
                .await;
            let mut kube_ops = KubeOps::new();
            match kube_ops
                .wait_for_deployment_ready("local-path-provisioner", "kube-system", 60)
                .await
            {
                Ok(true) => Ok::<(), anyhow::Error>(()),
                Ok(false) => {
                    let _ = tx2
                        .send(OutputLine::warning(
                            "local-path-provisioner not ready after 60s, continuing...",
                        ))
                        .await;
                    Ok(())
                }
                Err(_) => {
                    let _ = tx2
                        .send(OutputLine::warning(
                            "local-path-provisioner not ready after 60s, continuing...",
                        ))
                        .await;
                    Ok(())
                }
            }
        });

        // Wait for both tasks to complete
        let (coredns_result, provisioner_result) = tokio::join!(coredns_task, provisioner_task);
        coredns_result??;
        provisioner_result??;

        Ok(())
    }

    /// Wait for a deployment to be ready
    #[allow(dead_code)]
    pub(super) async fn wait_for_deployment(
        &mut self,
        name: &str,
        namespace: &str,
        timeout_secs: u64,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info(format!("Waiting for {}...", name)))
            .await;

        match self
            .kube_ops
            .wait_for_deployment_ready(name, namespace, timeout_secs)
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                // Don't fail if a component isn't ready, just warn
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "{} not ready after {}s, continuing...",
                        name, timeout_secs
                    )))
                    .await;
                Ok(())
            }
            Err(_) => {
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "{} not ready after {}s, continuing...",
                        name, timeout_secs
                    )))
                    .await;
                Ok(())
            }
        }
    }
}
