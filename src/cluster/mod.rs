pub(crate) mod config;
pub(crate) mod config_registry;
pub mod diagnostics;
pub(crate) mod docker;
mod ingress;
mod k3s;
pub(crate) mod kube_ops;
mod platform;
mod port_forward;
pub(crate) mod registry;
mod router;
mod traefik;

pub use config::ClusterConfig;
#[allow(unused_imports)]
pub use docker::ContainerRunConfig;
pub use docker::{ContainerPullProgress, ContainerStats, DockerManager, PullPhase};
pub use ingress::{
    HostsUpdateResult, IngressEntry, IngressHealthChecker, IngressHealthStatus, IngressManager,
};
pub use k3s::{ClusterStatus, K3sManager};
pub use platform::{find_available_port, PlatformInfo};
pub use port_forward::PortForwardDetector;
pub use router::{RouterManager, ROUTER_CONTAINER};
pub use traefik::TraefikManager;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::HookEvent;
use crate::hooks::HookExecutor;
use crate::ui::components::OutputLine;

/// Unified cluster manager that orchestrates all cluster operations
pub struct ClusterManager {
    config: Arc<ClusterConfig>,
    k3s: Option<K3sManager>,
    ingress: IngressManager,
    router: RouterManager,
    platform: PlatformInfo,
}

impl ClusterManager {
    /// Create a new ClusterManager from a shared config
    pub async fn new(config: Arc<ClusterConfig>) -> Result<Self> {
        let platform = PlatformInfo::detect()?;

        // Try to create K3sManager, but don't fail if Docker isn't available yet
        let k3s = K3sManager::new(Arc::clone(&config)).await.ok();

        // IngressManager without sudo - auto hosts update will try non-interactive
        let ingress = IngressManager::for_cluster(&config);
        let router = RouterManager::new(config.router_image.clone());

        Ok(Self {
            config,
            k3s,
            ingress,
            router,
            platform,
        })
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
            match K3sManager::new(Arc::clone(&self.config)).await {
                Ok(mgr) => self.k3s = Some(mgr),
                Err(e) => {
                    let _ = output_tx
                        .send(OutputLine::error(format!(
                            "Failed to initialize cluster manager: {:#}",
                            e
                        )))
                        .await;
                    return Ok(());
                }
            }
        }

        // Start k3s cluster (core components only)
        let outcome = if let Some(k3s) = &mut self.k3s {
            k3s.start(output_tx.clone()).await?
        } else {
            let _ = output_tx
                .send(OutputLine::error("Failed to initialize cluster manager"))
                .await;
            return Ok(());
        };

        // Bring the shared front router in line with what is actually running.
        // reconcile() first so a crash mid-teardown does not leave the router
        // advertising a cluster that no longer exists.
        if self.config.use_router {
            if let Some(k3s) = &self.k3s {
                if let Err(e) = self.router.reconcile(&k3s.docker, &output_tx).await {
                    tracing::warn!(error = %e, "Router reconcile failed");
                }
                if let Err(e) = self
                    .router
                    .ensure(&k3s.docker, &self.config, &output_tx)
                    .await
                {
                    let _ = output_tx
                        .send(OutputLine::error(format!("Router setup failed: {:#}", e)))
                        .await;
                }
            }
        }

        // Determine if we need to create a deep snapshot after Traefik + hooks
        let needs_deep_snapshot =
            matches!(outcome, k3s::StartOutcome::FreshCreated) && self.config.speedup.use_snapshot;

        // Deploy Traefik (ingress controller) in background for faster cluster availability
        let _ = output_tx
            .send(OutputLine::info(
                "Deploying Traefik ingress in background (cluster is usable now)...",
            ))
            .await;

        let mut traefik_manager = TraefikManager::new(Arc::clone(&self.config));
        let config = Arc::clone(&self.config);
        let socket_path = self.platform.docker_socket_path().await?;
        let tx = output_tx.clone();

        // Spawn background task for Traefik deployment and post-deployment tasks
        tokio::spawn(async move {
            // Deploy Traefik
            if let Err(e) = traefik_manager.deploy(tx.clone()).await {
                let _ = tx
                    .send(OutputLine::error(format!(
                        "Traefik deployment failed: {}",
                        e
                    )))
                    .await;
                return;
            }

            // Execute on_services_deployed hooks
            if config.hooks.has_hooks() {
                let hook_executor = HookExecutor::new(
                    config.hooks.clone(),
                    config.pinned_kubeconfig(),
                    config.context_name(),
                );
                if let Err(e) = hook_executor
                    .execute_hooks(HookEvent::OnServicesDeployed, tx.clone())
                    .await
                {
                    let _ = tx
                        .send(OutputLine::error(format!("Hook execution failed: {}", e)))
                        .await;
                    return;
                }
            }

            let _ = tx
                .send(OutputLine::success("All services deployed successfully!"))
                .await;

            // Create deep snapshot after all services are deployed
            if needs_deep_snapshot {
                match DockerManager::new(socket_path) {
                    Ok(docker) => {
                        let docker_root = docker.get_docker_root_dir().await;
                        let snapshot_image =
                            K3sManager::compute_snapshot_image_name(&config, &docker_root);

                        if let Err(e) = K3sManager::create_deep_snapshot(
                            &config.container_name,
                            &docker,
                            &config,
                            &tx,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "Deep snapshot creation failed");
                            let _ = tx
                                .send(OutputLine::warning(format!(
                                    "Deep snapshot creation failed: {}",
                                    e
                                )))
                                .await;
                        } else {
                            // Execute on_snapshot_created hooks
                            if config.hooks.has_hooks() {
                                let hook_executor = HookExecutor::new(
                                    config.hooks.clone(),
                                    config.pinned_kubeconfig(),
                                    config.context_name(),
                                );
                                if let Err(e) = hook_executor
                                    .execute_hooks(HookEvent::OnSnapshotCreated, tx.clone())
                                    .await
                                {
                                    let _ = tx
                                        .send(OutputLine::error(format!(
                                            "Hook execution failed: {}",
                                            e
                                        )))
                                        .await;
                                }
                            }

                            if config.speedup.snapshot_auto_cleanup {
                                if let Err(e) = K3sManager::cleanup_old_snapshots_static(
                                    &docker,
                                    &config.snapshot_prefix(),
                                    &config.cluster_name,
                                    &snapshot_image,
                                    &tx,
                                )
                                .await
                                {
                                    tracing::warn!(error = %e, "Snapshot cleanup failed");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to create DockerManager for deep snapshot");
                    }
                }
            }
        });

        let _ = output_tx
            .send(OutputLine::success("Cluster started successfully!"))
            .await;
        Ok(())
    }

    /// Stop the cluster
    pub async fn stop(&self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        if let Some(k3s) = &self.k3s {
            // Drop the router entry first so it stops advertising a backend that
            // is about to disappear; `start` re-registers it.
            if self.config.use_router {
                if let Err(e) = self
                    .router
                    .release(&k3s.docker, &self.config, &output_tx)
                    .await
                {
                    tracing::warn!(error = %e, "Router release failed");
                }
            }
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

    /// Delete the cluster and cleanup. `delete_snapshots` also removes the
    /// snapshot images (opt-in: `k3dev destroy --all`).
    pub async fn delete(
        &mut self,
        output_tx: mpsc::Sender<OutputLine>,
        delete_snapshots: bool,
    ) -> Result<()> {
        // Skip Traefik uninstall - resources live inside k3s container which is being deleted
        // This saves ~2-3 seconds since we don't need to wait for K8s API calls

        // Delete k3s cluster
        if let Some(k3s) = &self.k3s {
            // Must detach the router before k3s::delete removes the network,
            // otherwise `remove_network` fails with "has active endpoints".
            if self.config.use_router {
                if let Err(e) = self
                    .router
                    .release(&k3s.docker, &self.config, &output_tx)
                    .await
                {
                    tracing::warn!(error = %e, "Router release failed");
                }
            }
            k3s.delete(output_tx.clone(), delete_snapshots).await?;
        }

        // Note: /etc/hosts is left to the caller, which owns the elevated-write
        // path (the rewrite may need root, and that must not fail the destroy).

        Ok(())
    }

    /// Delete all snapshot images
    pub async fn delete_snapshots(&self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        if let Some(k3s) = &self.k3s {
            k3s.delete_snapshots(&output_tx).await?;
        }
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
}
