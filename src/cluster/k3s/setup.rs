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
use kube::config::Kubeconfig;

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
                .get(format!(
                    "https://{}:{}/healthz",
                    api_host, self.config.api_port
                ))
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

    /// Install socat in the k3s container using embedded static binary.
    /// Uses docker cp for reliable transfer of large binaries.
    pub(super) async fn install_socat(&self) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        const SOCAT_BINARY: &[u8] = include_bytes!("../../../assets/socat-x86_64");

        #[cfg(target_arch = "aarch64")]
        const SOCAT_BINARY: &[u8] = include_bytes!("../../../assets/socat-aarch64");

        if self
            .docker
            .exec_in_container(&self.config.container_name, &["which", "socat"])
            .await
            .is_ok()
        {
            return Ok(());
        }

        self.install_binary_via_docker_cp(SOCAT_BINARY, "socat", "/usr/local/bin/socat")
            .await
            .context("Failed to install socat")?;

        self.docker
            .exec_in_container(&self.config.container_name, &["socat", "-V"])
            .await
            .context("socat verification failed")?;

        Ok(())
    }

    /// Install k3dev-agent in the k3s container using embedded static binary.
    pub(super) async fn install_agent(&self) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        const AGENT_BINARY: &[u8] = include_bytes!("../../../assets/k3dev-agent-x86_64");

        #[cfg(target_arch = "aarch64")]
        const AGENT_BINARY: &[u8] = include_bytes!("../../../assets/k3dev-agent-aarch64");

        // The prebuilt k3s image bakes in an agent, which may predate the CLI
        // contract this build expects (it now takes a per-cluster cgroup root).
        // Reinstall unless the version string matches exactly.
        let expected = format!("k3dev-agent {}", Self::AGENT_VERSION);
        if let Ok(out) = self
            .docker
            .exec_in_container(
                &self.config.container_name,
                &["/usr/local/bin/k3dev-agent", "--version"],
            )
            .await
        {
            if out.trim() == expected {
                return Ok(());
            }
        }

        self.install_binary_via_docker_cp(
            AGENT_BINARY,
            "k3dev-agent",
            "/usr/local/bin/k3dev-agent",
        )
        .await
        .context("Failed to install k3dev-agent")?;

        Ok(())
    }

    /// Install k3dev-criproxy in the k3s container using embedded static binary.
    ///
    /// Unlike socat and the stats agent this runs *before* k3s does — the
    /// container entrypoint waits for it — so it is uploaded straight after the
    /// container starts rather than after the API comes up.
    pub(super) async fn install_criproxy(&self) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        const CRIPROXY_BINARY: &[u8] = include_bytes!("../../../assets/k3dev-criproxy-x86_64");

        #[cfg(target_arch = "aarch64")]
        const CRIPROXY_BINARY: &[u8] = include_bytes!("../../../assets/k3dev-criproxy-aarch64");

        let expected = format!("k3dev-criproxy {}", Self::CRIPROXY_VERSION);
        if let Ok(out) = self
            .docker
            .exec_in_container(
                &self.config.container_name,
                &["/usr/local/bin/k3dev-criproxy", "--version"],
            )
            .await
        {
            if out.trim() == expected {
                return Ok(());
            }
        }

        self.install_binary_via_docker_cp(
            CRIPROXY_BINARY,
            "k3dev-criproxy",
            "/usr/local/bin/k3dev-criproxy",
        )
        .await
        .context("Failed to install k3dev-criproxy")?;

        // The container entrypoint blocks on this binary's socket, so a broken
        // upload (a build placeholder, a mismatched arch) would stall startup
        // until the API wait times out with an unrelated message. Fail here
        // instead, while the cause is still obvious.
        let installed = self
            .docker
            .exec_in_container(
                &self.config.container_name,
                &["/usr/local/bin/k3dev-criproxy", "--version"],
            )
            .await
            .context("k3dev-criproxy verification failed")?;
        if installed.trim() != expected {
            return Err(anyhow!(
                "k3dev-criproxy reports '{}', expected '{}'",
                installed.trim(),
                expected
            ));
        }

        Ok(())
    }

    /// Install a binary into the container via bollard upload (tar stream).
    async fn install_binary_via_docker_cp(
        &self,
        binary: &[u8],
        name: &str,
        dest: &str,
    ) -> Result<()> {
        // Ensure target directory exists
        let _ = self
            .docker
            .exec_in_container(
                &self.config.container_name,
                &["mkdir", "-p", "/usr/local/bin"],
            )
            .await;

        // Extract just the filename from dest path (e.g. "/usr/local/bin/agent" -> "agent")
        let file_name = std::path::Path::new(dest)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(name);
        let dest_dir = std::path::Path::new(dest)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/usr/local/bin");

        tracing::info!(name = %file_name, dest = %dest_dir, "uploading binary to container");

        self.docker
            .copy_to_container(&self.config.container_name, file_name, binary, dest_dir)
            .await
            .with_context(|| format!("Failed to upload {} to container", name))?;

        Ok(())
    }

    /// Merge this cluster's credentials into `~/.kube/config`.
    ///
    /// k3s emits a kubeconfig whose cluster, user and context are all literally
    /// `default`. Copying that file over `~/.kube/config` — which is what k3dev
    /// used to do — destroys every unrelated context the user has. Instead the
    /// entries are renamed to the cluster name and merged in by name, so several
    /// k3dev clusters (and any pre-existing contexts) coexist.
    pub(super) async fn setup_kubeconfig(&self) -> Result<()> {
        let kube_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot find home directory"))?
            .join(".kube");

        // Create .kube directory if it doesn't exist
        fs::create_dir_all(&kube_dir).await?;

        let kubeconfig_path = kube_dir.join("config");

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
                    let name = self.config.context_name();
                    let host = PlatformInfo::docker_remote_host().unwrap_or("127.0.0.1");
                    let server = format!("https://{}:{}", host, self.config.api_port);

                    let generated = Kubeconfig::from_yaml(&content)
                        .context("Failed to parse kubeconfig emitted by k3s")?;
                    let standalone = KubeOps::rename_kubeconfig_entries(generated, &name, &server)?;

                    KubeOps::write_kubeconfig(&self.config.pinned_kubeconfig_path(), &standalone)
                        .await
                        .context("Failed to write pinned kubeconfig")?;

                    KubeOps::merge_kubeconfig_entries(&kubeconfig_path, &standalone, &name).await?;

                    return Ok(());
                }
            }

            sleep(Duration::from_secs(1)).await;
        }

        Err(anyhow!("Timeout waiting for kubeconfig"))
    }

    /// Cleanup kubeconfig entries
    pub(super) async fn cleanup_kubeconfig(&self) -> Result<()> {
        // Ignore errors as entries might not exist
        let name = self.config.context_name();
        let _ = KubeOps::cleanup_kubeconfig_entries(&name, &name, &name).await;
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
        let kubeconfig = self.config.kubeconfig.clone();
        let context = self.config.context.clone();
        let (kubeconfig2, context2) = (kubeconfig.clone(), context.clone());

        let coredns_task = tokio::spawn(async move {
            let _ = tx1.send(OutputLine::info("Waiting for coredns...")).await;
            let mut kube_ops = KubeOps::with_context(kubeconfig, context);
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
            let mut kube_ops = KubeOps::with_context(kubeconfig2, context2);
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
}
