use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;

use super::kube_ops::KubeOps;
use crate::ui::components::OutputLine;

/// Get the platform-appropriate hosts file path
fn hosts_file_path() -> PathBuf {
    #[cfg(windows)]
    {
        // Windows: C:\Windows\System32\drivers\etc\hosts
        if let Ok(windir) = std::env::var("SystemRoot") {
            PathBuf::from(windir)
                .join("System32")
                .join("drivers")
                .join("etc")
                .join("hosts")
        } else {
            PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
        }
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/etc/hosts")
    }
}

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
    /// Check health of a single endpoint (host + path)
    pub async fn check_endpoint(host: &str, path: &str) -> IngressHealthStatus {
        let url = format!("http://{}{}", host, path);

        let client = match reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(c) => c,
            Err(_) => return IngressHealthStatus::Error,
        };

        match client.get(&url).send().await {
            Ok(resp) => match resp.status().as_u16() {
                200..=299 => IngressHealthStatus::Healthy,
                300..=499 => IngressHealthStatus::Warning,
                _ => IngressHealthStatus::Error,
            },
            Err(_) => IngressHealthStatus::Error,
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

/// Result of an /etc/hosts update attempt
#[allow(dead_code)]
pub enum HostsUpdateResult {
    /// No update was needed (all hosts already present)
    NoUpdateNeeded,
    /// Successfully written directly (had write permission)
    WrittenDirectly { count: usize },
    /// Needs elevated privileges — contains the full file content to write
    NeedsSudo { content: String, count: usize },
    /// Hosts file is read-only (NixOS, MicroOS, etc.) — contains manual entries
    ReadOnly { entries: Vec<String> },
}

/// Ingress manager for /etc/hosts updates
pub struct IngressManager {
    hosts_marker: String,
    domain: Option<String>,
    kube_ops: KubeOps,
}

impl IngressManager {
    pub fn new() -> Self {
        Self {
            hosts_marker: "# k3dev-ingress".to_string(),
            domain: None,
            kube_ops: KubeOps::new(),
        }
    }

    pub fn with_domain(domain: String) -> Self {
        Self {
            hosts_marker: "# k3dev-ingress".to_string(),
            domain: Some(domain),
            kube_ops: KubeOps::new(),
        }
    }

    /// Get the Traefik dashboard domain based on configured domain
    pub fn traefik_dashboard_domain(&self) -> Option<String> {
        self.domain.as_ref().map(|d| format!("traefik.{}", d))
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
        let hosts_path = hosts_file_path();
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
        let hosts_path = hosts_file_path();
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

    /// Update /etc/hosts with ingress entries.
    /// Returns a result indicating what happened or what action is needed.
    pub async fn update_hosts(
        &mut self,
        output_tx: Option<mpsc::Sender<OutputLine>>,
    ) -> Result<HostsUpdateResult> {
        let hosts = self.get_ingress_hosts().await?;

        if hosts.is_empty() {
            if let Some(tx) = &output_tx {
                let _ = tx.send(OutputLine::info("No ingress hosts found")).await;
            }
            return Ok(HostsUpdateResult::NoUpdateNeeded);
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
            return Ok(HostsUpdateResult::NoUpdateNeeded);
        }

        // Read current /etc/hosts
        let hosts_path = hosts_file_path();
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

        // Check if hosts file is on a read-only filesystem (NixOS, MicroOS, etc.)
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = std::fs::metadata(&hosts_path) {
                let mode = meta.mode();
                let uid = meta.uid();
                let is_root = std::fs::metadata("/proc/self")
                    .map(|m| m.uid() == 0)
                    .unwrap_or(false);
                if !is_root && uid == 0 && (mode & 0o200) == 0 {
                    if let Some(tx) = &output_tx {
                        let _ = tx
                            .send(OutputLine::warning(
                                "Hosts file appears read-only (NixOS/MicroOS/immutable distro)",
                            ))
                            .await;
                        let _ = tx
                            .send(OutputLine::info("Add these entries to your system config:"))
                            .await;
                        for entry in &new_entries {
                            let _ = tx.send(OutputLine::info(entry)).await;
                        }
                    }
                    return Ok(HostsUpdateResult::ReadOnly {
                        entries: new_entries,
                    });
                }
            }
        }

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
            return Ok(HostsUpdateResult::WrittenDirectly {
                count: hosts.len(),
            });
        }

        // Needs elevated privileges — caller must handle this
        if let Some(tx) = &output_tx {
            let _ = tx
                .send(OutputLine::info(
                    "Requesting elevated privileges to update /etc/hosts...",
                ))
                .await;
        }

        Ok(HostsUpdateResult::NeedsSudo {
            content: final_content,
            count: hosts.len(),
        })
    }

    /// Clean all k3dev entries from /etc/hosts
    #[allow(dead_code)]
    pub async fn clean_hosts(
        &self,
        output_tx: Option<mpsc::Sender<OutputLine>>,
    ) -> Result<HostsUpdateResult> {
        let hosts_path = hosts_file_path();
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
            return Ok(HostsUpdateResult::WrittenDirectly { count: 0 });
        }

        // Needs elevated privileges
        if let Some(tx) = &output_tx {
            let _ = tx
                .send(OutputLine::info(
                    "Requesting elevated privileges to clean /etc/hosts...",
                ))
                .await;
        }

        Ok(HostsUpdateResult::NeedsSudo {
            content: final_content,
            count: 0,
        })
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
