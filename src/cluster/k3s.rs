use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::config::ClusterConfig;
use super::docker::{ContainerRunConfig, DockerManager};
use super::kube_ops::KubeOps;
use super::platform::PlatformInfo;
use crate::config::HookEvent;
use crate::hooks::HookExecutor;
use crate::ui::components::OutputLine;

/// K3s cluster lifecycle manager
pub struct K3sManager {
    config: ClusterConfig,
    docker: DockerManager,
    platform: PlatformInfo,
    kube_ops: KubeOps,
}

impl K3sManager {
    pub async fn new(config: ClusterConfig) -> Result<Self> {
        let platform = PlatformInfo::detect()?;
        let socket_path = platform.docker_socket_path().await?;
        let docker = DockerManager::new(socket_path)?;
        let kube_ops = KubeOps::new();

        Ok(Self {
            config,
            docker,
            platform,
            kube_ops,
        })
    }

    /// Docker volume name for rancher data (no sudo required)
    const RANCHER_VOLUME_NAME: &'static str = "k3s-rancher-data";

    /// Rancher data directory inside container
    const RANCHER_DATA_PATH: &'static str = "/var/lib/rancher/k3s";

    /// Docker volume name for local PV storage (no sudo required)
    const LOCAL_PV_VOLUME_NAME: &'static str = "k3s-local-pv-data";

    /// PV storage path - points to Docker volume's internal path
    /// This path is accessible to pod containers since they run on the same Docker daemon
    const LOCAL_PV_STORAGE_PATH: &'static str = "/var/lib/docker/volumes/k3s-local-pv-data/_data";

    /// Get cluster status
    pub async fn get_status(&self) -> ClusterStatus {
        if !self.docker.is_accessible().await {
            return ClusterStatus::RuntimeNotRunning;
        }

        match self
            .docker
            .container_status(&self.config.container_name)
            .await
        {
            Some(status) => match status.as_str() {
                "running" => ClusterStatus::Running,
                "exited" | "dead" => ClusterStatus::Stopped,
                "restarting" => ClusterStatus::Starting,
                "paused" => ClusterStatus::Paused,
                _ => ClusterStatus::Unknown,
            },
            None => ClusterStatus::NotCreated,
        }
    }

    /// Start the k3s cluster (create if not exists)
    pub async fn start(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Starting k3s cluster..."))
            .await;

        // Check Docker accessibility
        if !self.docker.is_accessible().await {
            return Err(anyhow!(
                "Docker is not accessible. Please start Docker first."
            ));
        }

        // Check if container already running
        if self
            .docker
            .container_running(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Cluster is already running"))
                .await;
            return Ok(());
        }

        // Check if container exists but stopped
        if self
            .docker
            .container_exists(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Starting existing cluster container..."))
                .await;
            self.docker
                .start_container(&self.config.container_name)
                .await?;
            self.wait_for_api(&output_tx).await?;

            // Execute on_cluster_available hooks
            if self.config.hooks.has_hooks() {
                let hook_executor = HookExecutor::new(self.config.hooks.clone());
                hook_executor
                    .execute_hooks(HookEvent::OnClusterAvailable, output_tx.clone())
                    .await?;
            }

            return Ok(());
        }

        // Create new cluster
        self.create_cluster(&output_tx).await
    }

    /// Create a new k3s cluster
    async fn create_cluster(&mut self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Creating new k3s cluster..."))
            .await;

        // Run pre-container setup tasks in parallel
        let _ = output_tx
            .send(OutputLine::info("Setting up cluster prerequisites..."))
            .await;
        let image = self.config.k3s_image();
        let image_exists = self.docker.image_exists(&image).await;

        // Create volume, PV directory, network, and pull image in parallel
        let pull_future = async {
            if !image_exists {
                let _ = output_tx
                    .send(OutputLine::info(format!("Pulling k3s image: {}...", image)))
                    .await;
                self.docker.pull_image(&image).await
            } else {
                Ok(())
            }
        };

        tokio::try_join!(
            async {
                let _ = output_tx
                    .send(OutputLine::info(
                        "Creating Docker volume for rancher data...",
                    ))
                    .await;
                self.docker.create_volume(Self::RANCHER_VOLUME_NAME).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Creating Docker volume for PV storage..."))
                    .await;
                self.docker.create_volume(Self::LOCAL_PV_VOLUME_NAME).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Creating Docker network..."))
                    .await;
                self.docker.create_network(&self.config.network_name).await
            },
            pull_future,
        )?;

        // Get docker socket path
        let socket_path = self.platform.docker_socket_path().await?;

        // Build port mappings
        let ports: Vec<(u16, u16)> = self
            .config
            .port_mappings()
            .iter()
            .filter_map(|p| {
                let parts: Vec<&str> = p.split(':').collect();
                if parts.len() == 2 {
                    Some((parts[0].parse().ok()?, parts[1].parse().ok()?))
                } else {
                    None
                }
            })
            .collect();

        // K3s server command
        // Note: metrics-server is disabled because we use Docker API for metrics
        // Traefik is enabled (K3s built-in) and configured via HelmChartConfig CRD
        let k3s_command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "mkdir -p /run/k3s /sys/fs/cgroup/kubepods && \
             /bin/k3s server \
             --docker \
             --disable=metrics-server \
             --service-node-port-range 80-32767 \
             --kubelet-arg 'root-dir=/var/lib/docker/kubelet'"
                .to_string(),
        ];

        // Run k3s container
        let _ = output_tx
            .send(OutputLine::info("Starting k3s container..."))
            .await;

        let run_config = ContainerRunConfig {
            name: self.config.container_name.clone(),
            hostname: Some(self.config.container_name.clone()),
            image,
            detach: true,
            privileged: true,
            ports,
            volumes: vec![
                // Docker socket for k3s --docker mode
                (
                    socket_path.to_string_lossy().to_string(),
                    "/var/run/docker.sock".to_string(),
                    String::new(),
                ),
                // Mount real /var/lib/docker - required for k3s --docker mode to access host Docker data
                (
                    "/var/lib/docker".to_string(),
                    "/var/lib/docker".to_string(),
                    "bind-propagation=rshared".to_string(),
                ),
                // Docker volume for rancher data (server config, agent data) - no sudo required
                (
                    Self::RANCHER_VOLUME_NAME.to_string(),
                    Self::RANCHER_DATA_PATH.to_string(),
                    "volume".to_string(),
                ),
                // Docker volume for local PV storage - accessible to pod containers via Docker's volume path
                (
                    Self::LOCAL_PV_VOLUME_NAME.to_string(),
                    Self::LOCAL_PV_STORAGE_PATH.to_string(),
                    "volume".to_string(),
                ),
            ],
            env: vec![],
            network: Some(self.config.network_name.clone()),
            cgroupns_host: true,
            pid_host: true,
            entrypoint: Some(String::new()),
            command: Some(k3s_command),
        };

        self.docker.run_container(&run_config).await?;

        // Wait for k3s API
        self.wait_for_api(output_tx).await?;

        // Install socat and setup kubeconfig in parallel
        let _ = output_tx
            .send(OutputLine::info("Configuring cluster access..."))
            .await;
        let (socat_result, kubeconfig_result) = tokio::join!(
            async {
                let _ = output_tx
                    .send(OutputLine::info("Installing socat in container..."))
                    .await;
                self.install_socat().await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Setting up kubeconfig..."))
                    .await;
                self.setup_kubeconfig().await
            },
        );

        // Report errors with context
        if let Err(e) = &socat_result {
            let _ = output_tx
                .send(OutputLine::error(format!("Socat install failed: {:#}", e)))
                .await;
        }
        if let Err(e) = &kubeconfig_result {
            let _ = output_tx
                .send(OutputLine::error(format!(
                    "Kubeconfig setup failed: {:#}",
                    e
                )))
                .await;
        }
        socat_result?;
        kubeconfig_result?;

        // Wait for cluster to be fully ready
        self.wait_for_cluster_ready(output_tx).await?;

        // Execute on_cluster_available hooks
        if self.config.hooks.has_hooks() {
            let hook_executor = HookExecutor::new(self.config.hooks.clone());
            hook_executor
                .execute_hooks(HookEvent::OnClusterAvailable, output_tx.clone())
                .await?;
        }

        let _ = output_tx
            .send(OutputLine::success("K3s cluster is ready!"))
            .await;

        Ok(())
    }

    /// Wait for k3s API to become accessible
    async fn wait_for_api(&self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Waiting for k3s API..."))
            .await;

        let max_retries = 60;
        for i in 0..max_retries {
            let result = Command::new("curl")
                .args(["-sk", "https://127.0.0.1:6443/healthz"])
                .output()
                .await;

            if let Ok(output) = result {
                if output.status.success() {
                    let body = String::from_utf8_lossy(&output.stdout);
                    // Accept "ok" (older k3s) or any JSON response (API is responding)
                    // A 401 Unauthorized means the API is up but requires auth
                    if body.contains("ok") || body.contains("apiVersion") {
                        return Ok(());
                    }
                }
            }

            if i % 10 == 0 && i > 0 {
                let _ = output_tx
                    .send(OutputLine::info(format!(
                        "Still waiting for API... ({}/{})",
                        i, max_retries
                    )))
                    .await;
            }
            sleep(Duration::from_secs(1)).await;
        }

        Err(anyhow!("Timeout waiting for k3s API"))
    }

    /// Install socat in the k3s container by extracting from alpine/socat image
    /// Copies binary + required libraries since k3s is a minimal image
    async fn install_socat(&self) -> Result<()> {
        const SOCAT_IMAGE: &str = "alpine/socat";
        let temp_container = format!("socat-extract-{}", std::process::id());

        // Create a stopped container from alpine/socat (just for file extraction)
        self.docker
            .create_container_stopped(&temp_container, SOCAT_IMAGE)
            .await
            .with_context(|| format!("Failed to create temp container from {}", SOCAT_IMAGE))?;

        // Create directories in target container
        let _ = self
            .docker
            .exec_in_container(
                &self.config.container_name,
                &["mkdir", "-p", "/opt/socat", "/usr/local/bin"],
            )
            .await;

        // Files to copy: socat binary and its required shared libraries
        // Note: libreadline.so.8 and libncursesw.so.6 are symlinks, copy actual versioned files
        let files_to_copy = [
            ("/usr/bin/socat1", "/opt/socat/socat1"),
            ("/usr/lib/libreadline.so.8.3", "/opt/socat/libreadline.so.8"),
            ("/usr/lib/libssl.so.3", "/opt/socat/libssl.so.3"),
            ("/usr/lib/libcrypto.so.3", "/opt/socat/libcrypto.so.3"),
            ("/usr/lib/libncursesw.so.6.5", "/opt/socat/libncursesw.so.6"),
        ];

        // Copy all files
        let mut copy_error = None;
        for (src, dst) in &files_to_copy {
            if let Err(e) = self
                .docker
                .copy_file_between_containers(
                    &temp_container,
                    src,
                    &self.config.container_name,
                    dst,
                )
                .await
            {
                copy_error = Some(e);
                break;
            }
        }

        // Create wrapper script at /usr/local/bin/socat
        if copy_error.is_none() {
            let wrapper_script =
                "#!/bin/sh\nLD_LIBRARY_PATH=/opt/socat exec /opt/socat/socat1 \"$@\"\n";
            if let Err(e) = self
                .docker
                .upload_file_content(
                    &self.config.container_name,
                    "/usr/local/bin/socat",
                    wrapper_script.as_bytes(),
                    0o755,
                )
                .await
            {
                copy_error = Some(e.context("Failed to create socat wrapper script"));
            }
        }

        // Always cleanup temp container
        let _ = self.docker.remove_container(&temp_container, true).await;

        match copy_error {
            Some(e) => Err(e.context(format!(
                "Failed to install socat in {}",
                self.config.container_name
            ))),
            None => Ok(()),
        }
    }

    /// Setup kubeconfig file
    async fn setup_kubeconfig(&self) -> Result<()> {
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
                    // Replace 127.0.0.1 with localhost for better compatibility
                    let fixed_content = content.replace("127.0.0.1", "localhost");

                    // Write to temp file first
                    fs::write(&temp_config, &fixed_content).await?;

                    // If kubeconfig exists, merge; otherwise just copy
                    if kubeconfig_path.exists() {
                        // For now, just overwrite - TODO: proper merging
                        fs::copy(&temp_config, &kubeconfig_path).await?;
                    } else {
                        fs::copy(&temp_config, &kubeconfig_path).await?;
                    }

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

    /// Wait for cluster to be fully ready
    async fn wait_for_cluster_ready(&mut self, output_tx: &mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Waiting for cluster components..."))
            .await;

        // Wait for deployments sequentially since we need &mut self
        // Note: metrics-server is disabled - we use Docker API for metrics instead
        self.wait_for_deployment("coredns", "kube-system", 60, output_tx)
            .await?;
        self.wait_for_deployment("local-path-provisioner", "kube-system", 60, output_tx)
            .await?;

        // Configure local-path-provisioner to use our custom storage path
        let _ = output_tx
            .send(OutputLine::info(
                "Configuring local-path-provisioner storage path...",
            ))
            .await;
        self.configure_local_path_provisioner(output_tx).await?;

        Ok(())
    }

    /// Wait for a deployment to be ready
    async fn wait_for_deployment(
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

    /// Stop the k3s cluster
    pub async fn stop(&self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Stopping k3s cluster..."))
            .await;

        if !self
            .docker
            .container_exists(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Cluster is not running"))
                .await;
            return Ok(());
        }

        self.docker
            .stop_container(&self.config.container_name)
            .await?;

        let _ = output_tx
            .send(OutputLine::success("K3s cluster stopped"))
            .await;
        Ok(())
    }

    /// Delete the k3s cluster and cleanup
    pub async fn delete(&self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("Deleting k3s cluster..."))
            .await;

        // Stop and remove k3s container
        if self
            .docker
            .container_exists(&self.config.container_name)
            .await
        {
            let _ = output_tx
                .send(OutputLine::info("Stopping k3s container..."))
                .await;
            let _ = self
                .docker
                .stop_container(&self.config.container_name)
                .await;
            self.docker
                .remove_container(&self.config.container_name, true)
                .await?;
        }

        // Cleanup k8s_* containers (managed pods) - must complete before network removal
        let _ = output_tx
            .send(OutputLine::info("Cleaning up pod containers..."))
            .await;
        self.docker.cleanup_containers_by_prefix("k8s_").await?;

        // Run remaining cleanup tasks in parallel (all independent after containers are gone)
        let _ = output_tx
            .send(OutputLine::info("Cleaning up cluster resources..."))
            .await;
        let (network_result, rancher_volume_result, pv_volume_result, kubeconfig_result) = tokio::join!(
            async {
                let _ = output_tx
                    .send(OutputLine::info("Removing Docker network..."))
                    .await;
                self.docker.remove_network(&self.config.network_name).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Removing rancher data volume..."))
                    .await;
                self.docker.remove_volume(Self::RANCHER_VOLUME_NAME).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Removing PV storage volume..."))
                    .await;
                self.docker.remove_volume(Self::LOCAL_PV_VOLUME_NAME).await
            },
            async {
                let _ = output_tx
                    .send(OutputLine::info("Cleaning kubeconfig..."))
                    .await;
                self.cleanup_kubeconfig().await
            },
        );
        // Propagate errors from cleanup operations
        network_result?;
        rancher_volume_result?;
        pv_volume_result?;
        kubeconfig_result?;

        let _ = output_tx
            .send(OutputLine::success("K3s cluster deleted"))
            .await;
        Ok(())
    }

    /// Cleanup kubeconfig entries
    async fn cleanup_kubeconfig(&self) -> Result<()> {
        // Remove cluster, context, and user entries using kube crate
        // Ignore errors as entries might not exist
        let _ = KubeOps::cleanup_kubeconfig_entries("default", "default", "default").await;
        Ok(())
    }

    /// Configure local-path-provisioner to use our custom storage path
    ///
    /// NOTE: The k3s addon controller manages the local-path-config ConfigMap and may revert changes.
    /// We use an annotation to prevent this and verify the configuration was applied successfully.
    async fn configure_local_path_provisioner(
        &mut self,
        output_tx: &mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        // Build the new config.json for local-path-provisioner
        // This path must be accessible to Docker containers (pod containers in --docker mode)
        let config_json = format!(
            r#"{{"nodePathMap":[{{"node":"DEFAULT_PATH_FOR_NON_LISTED_NODES","paths":["{}"]}}]}}"#,
            Self::LOCAL_PV_STORAGE_PATH
        );

        // First, remove the objectset.rio.cattle.io/applied annotation to prevent k3s from reverting our changes
        // This annotation is used by k3s's addon reconciler
        let _ = self
            .kube_ops
            .remove_configmap_annotation(
                "local-path-config",
                "kube-system",
                "objectset.rio.cattle.io/applied",
            )
            .await;

        // Retry the patch up to 3 times in case of transient failures
        let mut patch_success = false;
        for attempt in 1..=3 {
            let mut data = BTreeMap::new();
            data.insert("config.json".to_string(), config_json.clone());

            match self
                .kube_ops
                .patch_configmap_data("local-path-config", "kube-system", data)
                .await
            {
                Ok(_) => {
                    patch_success = true;
                    break;
                }
                Err(e) => {
                    if attempt < 3 {
                        let _ = output_tx
                            .send(OutputLine::info(format!(
                                "ConfigMap patch attempt {} failed, retrying...",
                                attempt
                            )))
                            .await;
                        sleep(Duration::from_secs(1)).await;
                    } else {
                        let _ = output_tx.send(OutputLine::warning(
                            format!("Failed to configure local-path-provisioner after 3 attempts: {}. PVCs may not work correctly.", e)
                        )).await;
                    }
                }
            }
        }

        if !patch_success {
            return Ok(()); // Don't fail cluster creation, but config may not be correct
        }

        // Verify the ConfigMap was actually updated
        let current_config = self
            .kube_ops
            .get_configmap_data("local-path-config", "kube-system", "config.json")
            .await
            .unwrap_or(None)
            .unwrap_or_default();

        if !current_config.contains(Self::LOCAL_PV_STORAGE_PATH) {
            let _ = output_tx.send(OutputLine::warning(
                format!("ConfigMap verification failed: config doesn't contain {}. The k3s addon controller may have reverted the change.", Self::LOCAL_PV_STORAGE_PATH)
            )).await;
            // Try one more time after a brief wait (give k3s time to settle)
            sleep(Duration::from_secs(2)).await;
            let mut data = BTreeMap::new();
            data.insert("config.json".to_string(), config_json.clone());
            let _ = self
                .kube_ops
                .patch_configmap_data("local-path-config", "kube-system", data)
                .await;
        }

        // Restart the local-path-provisioner deployment to pick up the new config
        if let Err(e) = self
            .kube_ops
            .rollout_restart_deployment("local-path-provisioner", "kube-system")
            .await
        {
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Failed to restart local-path-provisioner: {}",
                    e
                )))
                .await;
        }

        // Wait for the rollout to complete (up to 30 seconds)
        if !self
            .kube_ops
            .wait_for_rollout("local-path-provisioner", "kube-system", 30)
            .await
            .unwrap_or(false)
        {
            let _ = output_tx
                .send(OutputLine::warning(
                    "local-path-provisioner restart did not complete within timeout",
                ))
                .await;
            // Still continue - it will eventually restart
        }

        let _ = output_tx
            .send(OutputLine::info(format!(
                "Configured local-path-provisioner to use {}",
                Self::LOCAL_PV_STORAGE_PATH
            )))
            .await;

        Ok(())
    }

    /// Restart the k3s cluster
    #[allow(dead_code)]
    pub async fn restart(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        self.stop(output_tx.clone()).await?;
        sleep(Duration::from_secs(2)).await;
        self.start(output_tx).await
    }

    /// Get cluster info
    pub async fn info(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let _ = output_tx
            .send(OutputLine::info("=== K3s Cluster Info ==="))
            .await;

        // Cluster status
        let status = self.get_status().await;
        let _ = output_tx
            .send(OutputLine::info(format!("Status: {:?}", status)))
            .await;

        if status != ClusterStatus::Running {
            return Ok(());
        }

        // Kubernetes version
        if let Ok(version) = self.kube_ops.get_version().await {
            let _ = output_tx.send(OutputLine::info(version)).await;
        }

        // Nodes
        let _ = output_tx.send(OutputLine::info("\n=== Nodes ===")).await;
        let _ = output_tx
            .send(OutputLine::info(format!(
                "{:<20} {:<10} {:<15} {:<15} {}",
                "NAME", "STATUS", "ROLES", "INTERNAL-IP", "VERSION"
            )))
            .await;
        if let Ok(nodes) = self.kube_ops.list_nodes().await {
            for node in nodes {
                let _ = output_tx
                    .send(OutputLine::info(node.to_wide_string()))
                    .await;
            }
        }

        // Namespaces
        let _ = output_tx
            .send(OutputLine::info("\n=== Namespaces ==="))
            .await;
        if let Ok(namespaces) = self.kube_ops.list_namespaces().await {
            for ns in namespaces {
                let _ = output_tx.send(OutputLine::info(format!("  {}", ns))).await;
            }
        }

        // System pods
        let _ = output_tx
            .send(OutputLine::info("\n=== System Pods ==="))
            .await;
        let _ = output_tx
            .send(OutputLine::info(format!(
                "{:<50} {:<10} {}",
                "NAME", "READY", "STATUS"
            )))
            .await;
        if let Ok(pods) = self.kube_ops.list_pods("kube-system").await {
            for pod in pods {
                let _ = output_tx.send(OutputLine::info(pod.to_string_line())).await;
            }
        }

        Ok(())
    }

    /// Reset the kube client (call after kubeconfig changes)
    #[allow(dead_code)]
    pub fn reset_kube_client(&mut self) {
        self.kube_ops.reset();
    }
}

/// Cluster status
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClusterStatus {
    Running,
    Stopped,
    Starting,
    Paused,
    NotCreated,
    RuntimeNotRunning,
    Unknown,
}

impl ClusterStatus {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterStatus::Running => "Running",
            ClusterStatus::Stopped => "Stopped",
            ClusterStatus::Starting => "Starting",
            ClusterStatus::Paused => "Paused",
            ClusterStatus::NotCreated => "Not Created",
            ClusterStatus::RuntimeNotRunning => "Runtime Not Running",
            ClusterStatus::Unknown => "Unknown",
        }
    }
}
