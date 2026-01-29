mod config;
mod docker;
mod ingress;
mod k3s;
mod kube_ops;
mod platform;
mod port_forward;
mod traefik;

pub use config::ClusterConfig;
#[allow(unused_imports)]
pub use docker::ContainerRunConfig;
pub use docker::{ContainerStats, DockerManager, ResourceStats};
pub use ingress::{IngressEntry, IngressHealthChecker, IngressHealthStatus, IngressManager};
pub use k3s::{ClusterStatus, K3sManager};
pub use platform::PlatformInfo;
pub use port_forward::PortForwardDetector;
pub use traefik::TraefikManager;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::HookEvent;
use crate::hooks::HookExecutor;
use crate::ui::components::OutputLine;

/// Unified cluster manager that orchestrates all cluster operations
pub struct ClusterManager {
    config: ClusterConfig,
    k3s: Option<K3sManager>,
    traefik: TraefikManager,
    ingress: IngressManager,
    platform: PlatformInfo,
}

impl ClusterManager {
    pub async fn new(config: Option<ClusterConfig>) -> Result<Self> {
        let config = config.unwrap_or_default();
        let platform = PlatformInfo::detect()?;

        // Try to create K3sManager, but don't fail if Docker isn't available yet
        let k3s = K3sManager::new(config.clone()).await.ok();

        let traefik = TraefikManager::new(config.clone());
        // IngressManager without sudo - auto hosts update will try non-interactive
        let ingress = IngressManager::new();

        Ok(Self {
            config,
            k3s,
            traefik,
            ingress,
            platform,
        })
    }

    /// Get the cluster configuration
    #[allow(dead_code)]
    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }

    /// Get cluster status
    pub async fn get_status(&self) -> ClusterStatus {
        if let Some(k3s) = &self.k3s {
            k3s.get_status().await
        } else {
            ClusterStatus::RuntimeNotRunning
        }
    }

    /// Start the cluster and all services
    pub async fn start(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        // Ensure K3sManager is available
        if self.k3s.is_none() {
            self.k3s = K3sManager::new(self.config.clone()).await.ok();
        }

        // Start k3s cluster
        if let Some(k3s) = &mut self.k3s {
            k3s.start(output_tx.clone()).await?;
        } else {
            let _ = output_tx
                .send(OutputLine::error("Failed to initialize cluster manager"))
                .await;
            return Ok(());
        }

        // Deploy Traefik and update hosts (traefik needs &mut self, so run sequentially)
        if self.config.deploy_traefik {
            self.traefik.deploy(output_tx.clone()).await?;
        }

        if self.config.auto_update_hosts {
            // Try to update hosts without sudo - silently skip if no permission
            // User can manually update with 'H' key if needed
            if let Err(e) = self.ingress.update_hosts(Some(output_tx.clone())).await {
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "Could not update /etc/hosts (use 'H' key for manual update): {}",
                        e
                    )))
                    .await;
            }
        }

        // Execute on_services_deployed hooks
        if self.config.hooks.has_hooks() {
            let hook_executor = HookExecutor::new(self.config.hooks.clone());
            hook_executor
                .execute_hooks(HookEvent::OnServicesDeployed, output_tx.clone())
                .await?;
        }

        let _ = output_tx
            .send(OutputLine::success("Cluster started successfully!"))
            .await;
        Ok(())
    }

    /// Stop the cluster
    pub async fn stop(&self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        if let Some(k3s) = &self.k3s {
            k3s.stop(output_tx).await?;
        }
        Ok(())
    }

    /// Restart the cluster
    pub async fn restart(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        self.stop(output_tx.clone()).await?;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        self.start(output_tx).await
    }

    /// Delete the cluster and cleanup
    pub async fn delete(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        // Uninstall Traefik
        self.traefik.uninstall(output_tx.clone()).await?;

        // Delete k3s cluster
        if let Some(k3s) = &self.k3s {
            k3s.delete(output_tx.clone()).await?;
        }

        // Note: /etc/hosts entries are kept on purpose - user can manually update with 'H' key

        Ok(())
    }

    /// Get cluster info
    pub async fn info(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        // Platform info
        let _ = output_tx.send(OutputLine::info("=== Platform ===")).await;
        let _ = output_tx.send(OutputLine::info("OS: Linux")).await;
        let _ = output_tx
            .send(OutputLine::info(format!("Arch: {:?}", self.platform.arch)))
            .await;

        // Check prerequisites
        let missing = self.platform.get_missing_prerequisites().await;
        if !missing.is_empty() {
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Missing: {}",
                    missing.join(", ")
                )))
                .await;
        }

        // K3s info
        if let Some(k3s) = &mut self.k3s {
            k3s.info(output_tx.clone()).await?;
        }

        // Show ingress hosts
        self.ingress.show_hosts(output_tx).await?;

        Ok(())
    }

    /// Update /etc/hosts with current ingress entries
    #[allow(dead_code)]
    pub async fn update_hosts(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        self.ingress.update_hosts(Some(output_tx)).await
    }
}
