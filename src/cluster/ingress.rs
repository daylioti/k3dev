use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::kube_ops::KubeOps;
use crate::ui::components::OutputLine;

/// Health status for an ingress endpoint
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IngressHealthStatus {
    /// Healthy - 2xx response
    Healthy,
    /// Warning - 3xx, 4xx response (accessible but issues)
    Warning,
    /// Error - 5xx, timeout, connection refused
    Error,
    /// Unknown - not yet checked
    Unknown,
}

impl IngressHealthStatus {
    /// Get a colored dot character for display
    pub fn dot(&self) -> &'static str {
        match self {
            IngressHealthStatus::Healthy => "●", // Will be styled green
            IngressHealthStatus::Warning => "●", // Will be styled yellow
            IngressHealthStatus::Error => "●",   // Will be styled red
            IngressHealthStatus::Unknown => "○", // Empty circle
        }
    }
}

/// Information about an ingress endpoint with health status
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct IngressInfo {
    pub host: String,
    pub path: String,
    pub health: IngressHealthStatus,
}

/// Ingress entry with host and all its paths
#[derive(Debug, Clone)]
pub struct IngressEntry {
    pub host: String,
    pub paths: Vec<String>,
}

/// Health checker for ingress endpoints
pub struct IngressHealthChecker;

impl IngressHealthChecker {
    /// Check health of a single endpoint (host + path) using curl
    pub async fn check_endpoint(host: &str, path: &str) -> IngressHealthStatus {
        // Use curl with short timeout to check endpoint health
        // -s: silent, -o /dev/null: discard output, -w: write status code
        // --connect-timeout: connection timeout, -m: max time
        let url = format!("http://{}{}", host, path);
        let output = Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--connect-timeout",
                "2",
                "-m",
                "5",
                "-k", // Allow self-signed certs
                &url,
            ])
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                let code = String::from_utf8_lossy(&out.stdout);
                match code.trim().parse::<u16>() {
                    Ok(200..=299) => IngressHealthStatus::Healthy,
                    Ok(300..=499) => IngressHealthStatus::Warning,
                    Ok(_) => IngressHealthStatus::Error,
                    Err(_) => IngressHealthStatus::Error,
                }
            }
            _ => IngressHealthStatus::Error,
        }
    }

    /// Check health of multiple endpoints in parallel
    /// Key format: "host|path" (e.g., "example.com|/api")
    pub async fn check_endpoints(entries: &[IngressEntry]) -> HashMap<String, IngressHealthStatus> {
        let mut results = HashMap::new();

        // Build list of all host+path combinations
        let mut endpoints: Vec<(String, String)> = Vec::new();
        for entry in entries {
            for path in &entry.paths {
                endpoints.push((entry.host.clone(), path.clone()));
            }
        }

        // Check all endpoints in parallel
        let futures: Vec<_> = endpoints
            .into_iter()
            .map(|(host, path)| async move {
                let status = Self::check_endpoint(&host, &path).await;
                let key = format!("{}|{}", host, path);
                (key, status)
            })
            .collect();

        let checked = futures::future::join_all(futures).await;

        for (key, status) in checked {
            results.insert(key, status);
        }

        results
    }
}

/// Ingress manager for /etc/hosts updates
pub struct IngressManager {
    hosts_marker: String,
    sudo_password: Option<String>,
    domain: Option<String>,
    kube_ops: KubeOps,
}

impl IngressManager {
    pub fn new() -> Self {
        Self {
            hosts_marker: "# k3dev-ingress".to_string(),
            sudo_password: None,
            domain: None,
            kube_ops: KubeOps::new(),
        }
    }

    pub fn with_domain(domain: String) -> Self {
        Self {
            hosts_marker: "# k3dev-ingress".to_string(),
            sudo_password: None,
            domain: Some(domain),
            kube_ops: KubeOps::new(),
        }
    }

    pub fn with_domain_and_sudo(domain: String, sudo_password: Option<String>) -> Self {
        Self {
            hosts_marker: "# k3dev-ingress".to_string(),
            sudo_password,
            domain: Some(domain),
            kube_ops: KubeOps::new(),
        }
    }

    /// Get the Traefik dashboard domain based on configured domain
    pub fn traefik_dashboard_domain(&self) -> Option<String> {
        self.domain.as_ref().map(|d| format!("traefik.{}", d))
    }

    /// Run a command with sudo, using password if available
    async fn run_sudo_command(&self, args: &[&str]) -> Result<std::process::Output> {
        use std::process::Stdio;

        if let Some(password) = &self.sudo_password {
            // Use sudo -S to read password from stdin
            let mut child = Command::new("sudo")
                .arg("-S")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            // Write password to stdin
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(format!("{}\n", password).as_bytes())
                    .await?;
            }

            Ok(child.wait_with_output().await?)
        } else {
            // Try non-interactive sudo
            Ok(Command::new("sudo").arg("-n").args(args).output().await?)
        }
    }

    /// Write content to /etc/hosts using sudo
    async fn write_hosts_with_sudo(&self, content: &str) -> Result<bool> {
        // Write to temp file first
        let temp_path = "/tmp/k3dev-hosts";
        fs::write(temp_path, content).await?;

        // Copy with sudo
        let output = self
            .run_sudo_command(&["cp", temp_path, "/etc/hosts"])
            .await?;

        // Cleanup temp file
        let _ = fs::remove_file(temp_path).await;

        Ok(output.status.success())
    }

    /// Get all ingress hosts from the cluster
    pub async fn get_ingress_hosts(&mut self) -> Result<Vec<String>> {
        let mut hosts = HashSet::new();

        // Add Traefik dashboard host if domain is configured and Traefik is deployed
        if let Some(traefik_domain) = self.traefik_dashboard_domain() {
            if self.is_traefik_deployed().await {
                hosts.insert(traefik_domain);
            }
        }

        // Get standard Ingress resources
        if let Ok(ingresses) = self.kube_ops.list_ingresses().await {
            for ingress in ingresses {
                if !ingress.host.is_empty() {
                    hosts.insert(ingress.host);
                }
            }
        }

        // Get Traefik IngressRoute resources
        if let Ok(ingressroutes) = self.kube_ops.list_ingressroutes().await {
            for ir in ingressroutes {
                if !ir.host.is_empty() {
                    hosts.insert(ir.host);
                }
            }
        }

        Ok(hosts.into_iter().collect())
    }

    /// Check if Traefik is deployed in the cluster
    async fn is_traefik_deployed(&mut self) -> bool {
        self.kube_ops.service_exists("traefik", "kube-system").await
    }

    /// Get all ingress entries with their paths from the cluster
    pub async fn get_ingress_entries(&mut self) -> Result<Vec<IngressEntry>> {
        let mut host_paths: HashMap<String, HashSet<String>> = HashMap::new();

        // Add Traefik dashboard if domain is configured and Traefik is deployed
        if let Some(traefik_domain) = self.traefik_dashboard_domain() {
            if self.is_traefik_deployed().await {
                let entry = host_paths.entry(traefik_domain).or_default();
                entry.insert("/dashboard/".to_string());
            }
        }

        // Get standard Ingress resources with paths
        if let Ok(ingresses) = self.kube_ops.list_ingresses().await {
            for ingress in ingresses {
                if !ingress.host.is_empty() {
                    let entry = host_paths.entry(ingress.host).or_default();
                    for path in ingress.paths {
                        entry.insert(path);
                    }
                }
            }
        }

        // Get Traefik IngressRoute resources with paths
        if let Ok(ingressroutes) = self.kube_ops.list_ingressroutes().await {
            for ir in ingressroutes {
                if !ir.host.is_empty() {
                    let entry = host_paths.entry(ir.host).or_default();
                    entry.insert(ir.path);
                }
            }
        }

        // Convert to IngressEntry list
        let mut entries: Vec<IngressEntry> = host_paths
            .into_iter()
            .map(|(host, paths)| {
                let mut paths: Vec<String> = paths.into_iter().collect();
                paths.sort();
                IngressEntry { host, paths }
            })
            .collect();

        entries.sort_by(|a, b| a.host.cmp(&b.host));
        Ok(entries)
    }

    /// Read ALL hosts from /etc/hosts that point to 127.0.0.1 (for checking if domain is resolvable)
    pub async fn get_all_hosts_from_etc_hosts(&self) -> HashSet<String> {
        let hosts_path = PathBuf::from("/etc/hosts");
        let content = fs::read_to_string(&hosts_path).await.unwrap_or_default();

        let mut hosts = HashSet::new();
        for line in content.lines() {
            let line = line.trim();
            // Skip comments and empty lines
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Line format: "127.0.0.1 hostname [hostname2 ...]"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[0] == "127.0.0.1" {
                // Add all hostnames on this line (there can be multiple)
                for hostname in &parts[1..] {
                    // Stop at comment
                    if hostname.starts_with('#') {
                        break;
                    }
                    hosts.insert(hostname.to_string());
                }
            }
        }
        hosts
    }

    /// Read hosts from /etc/hosts that belong to k3dev (for cleanup)
    #[allow(dead_code)]
    pub async fn get_k3dev_hosts_from_etc_hosts(&self) -> HashSet<String> {
        let hosts_path = PathBuf::from("/etc/hosts");
        let content = fs::read_to_string(&hosts_path).await.unwrap_or_default();

        let mut hosts = HashSet::new();
        for line in content.lines() {
            if line.contains(&self.hosts_marker) {
                // Line format: "127.0.0.1 hostname # k3dev-ingress"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    hosts.insert(parts[1].to_string());
                }
            }
        }
        hosts
    }

    /// Check if all required hosts are already in /etc/hosts
    #[allow(dead_code)]
    pub async fn hosts_need_update(&mut self) -> Result<bool> {
        let ingress_hosts: HashSet<String> = self.get_ingress_hosts().await?.into_iter().collect();

        if ingress_hosts.is_empty() {
            return Ok(false);
        }

        // Check ALL hosts in /etc/hosts
        let etc_hosts = self.get_all_hosts_from_etc_hosts().await;

        // Check if all ingress hosts are already in /etc/hosts
        Ok(!ingress_hosts.is_subset(&etc_hosts))
    }

    /// Get hosts that are NOT in /etc/hosts (for UI indication)
    pub async fn get_missing_hosts(&mut self) -> Result<HashSet<String>> {
        let ingress_hosts: HashSet<String> = self.get_ingress_hosts().await?.into_iter().collect();

        if ingress_hosts.is_empty() {
            return Ok(HashSet::new());
        }

        // Check ALL hosts in /etc/hosts (not just k3dev marked ones)
        let etc_hosts = self.get_all_hosts_from_etc_hosts().await;

        // Return hosts that are in ingress but not in /etc/hosts
        Ok(ingress_hosts.difference(&etc_hosts).cloned().collect())
    }

    /// Update /etc/hosts with ingress entries
    pub async fn update_hosts(
        &mut self,
        output_tx: Option<mpsc::Sender<OutputLine>>,
    ) -> Result<()> {
        let hosts = self.get_ingress_hosts().await?;

        if hosts.is_empty() {
            if let Some(tx) = &output_tx {
                let _ = tx.send(OutputLine::info("No ingress hosts found")).await;
            }
            return Ok(());
        }

        // Check if update is needed - check ALL hosts in /etc/hosts
        let hosts_set: HashSet<String> = hosts.iter().cloned().collect();
        let etc_hosts = self.get_all_hosts_from_etc_hosts().await;
        if hosts_set.is_subset(&etc_hosts) {
            if let Some(tx) = &output_tx {
                let _ = tx
                    .send(OutputLine::info(
                        "All hosts already in /etc/hosts, skipping update",
                    ))
                    .await;
            }
            return Ok(());
        }

        // Read current /etc/hosts
        let hosts_path = PathBuf::from("/etc/hosts");
        let current_content = fs::read_to_string(&hosts_path).await.unwrap_or_default();

        // Remove existing k3dev entries
        let cleaned: Vec<&str> = current_content
            .lines()
            .filter(|line| !line.contains(&self.hosts_marker))
            .collect();

        // Add new entries
        let mut new_entries: Vec<String> = hosts
            .iter()
            .map(|host| format!("127.0.0.1 {} {}", host, self.hosts_marker))
            .collect();
        new_entries.sort();

        // Combine
        let mut final_content = cleaned.join("\n");
        if !final_content.ends_with('\n') {
            final_content.push('\n');
        }
        final_content.push_str(&new_entries.join("\n"));
        final_content.push('\n');

        // Try to write directly first (works if run as root or have write permissions)
        if fs::write(&hosts_path, &final_content).await.is_ok() {
            if let Some(tx) = &output_tx {
                let _ = tx
                    .send(OutputLine::success(format!(
                        "Updated /etc/hosts with {} entries",
                        hosts.len()
                    )))
                    .await;
            }
            return Ok(());
        }

        // Try with sudo
        if let Ok(success) = self.write_hosts_with_sudo(&final_content).await {
            if success {
                if let Some(tx) = &output_tx {
                    let _ = tx
                        .send(OutputLine::success(format!(
                            "Updated /etc/hosts with {} entries",
                            hosts.len()
                        )))
                        .await;
                }
                return Ok(());
            }
        }

        // Failed - show manual instructions
        if let Some(tx) = &output_tx {
            let _ = tx
                .send(OutputLine::warning(
                    "Cannot write to /etc/hosts (no permission)",
                ))
                .await;
            let _ = tx
                .send(OutputLine::info("Add these entries manually:"))
                .await;
            for entry in &new_entries {
                let _ = tx.send(OutputLine::info(entry)).await;
            }
        }

        Ok(())
    }

    /// Clean all k3dev entries from /etc/hosts
    #[allow(dead_code)]
    pub async fn clean_hosts(&self, output_tx: Option<mpsc::Sender<OutputLine>>) -> Result<()> {
        let hosts_path = PathBuf::from("/etc/hosts");
        let current_content = fs::read_to_string(&hosts_path).await.unwrap_or_default();

        // Remove k3dev entries
        let cleaned: Vec<&str> = current_content
            .lines()
            .filter(|line| !line.contains(&self.hosts_marker))
            .collect();

        let final_content = cleaned.join("\n") + "\n";

        // Try to write directly first
        if fs::write(&hosts_path, &final_content).await.is_ok() {
            if let Some(tx) = &output_tx {
                let _ = tx
                    .send(OutputLine::success("Cleaned /etc/hosts entries"))
                    .await;
            }
            return Ok(());
        }

        // Try with sudo
        if let Ok(success) = self.write_hosts_with_sudo(&final_content).await {
            if success {
                if let Some(tx) = &output_tx {
                    let _ = tx
                        .send(OutputLine::success("Cleaned /etc/hosts entries"))
                        .await;
                }
                return Ok(());
            }
        }

        // Failed
        if let Some(tx) = &output_tx {
            let _ = tx
                .send(OutputLine::warning(
                    "Cannot clean /etc/hosts (no permission)",
                ))
                .await;
        }

        Ok(())
    }

    /// Show current ingress hosts
    pub async fn show_hosts(&mut self, output_tx: mpsc::Sender<OutputLine>) -> Result<()> {
        let hosts = self.get_ingress_hosts().await?;

        if hosts.is_empty() {
            let _ = output_tx
                .send(OutputLine::info("No ingress hosts found"))
                .await;
            return Ok(());
        }

        let _ = output_tx
            .send(OutputLine::info("=== Ingress Hosts ==="))
            .await;
        for host in hosts {
            let _ = output_tx
                .send(OutputLine::info(format!("  {}", host)))
                .await;
        }

        Ok(())
    }
}

impl Default for IngressManager {
    fn default() -> Self {
        Self::new()
    }
}
