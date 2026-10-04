use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;

use super::config::ClusterConfig;
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

/// Does this line carry the pre-multi-cluster `# k3dev-ingress` marker
/// (i.e. without the `[cluster]` suffix)?
fn is_legacy_marker(line: &str) -> bool {
    match line.find("# k3dev-ingress") {
        Some(idx) => !line[idx + "# k3dev-ingress".len()..].starts_with('['),
        None => false,
    }
}

/// Rewrite `current` so that this cluster owns exactly one line per host in
/// `hosts`, returning the new content plus the entries it added.
///
/// Only this cluster's marked lines are dropped; every other cluster's block
/// survives untouched. Legacy unbracketed `# k3dev-ingress` lines predate
/// multi-cluster support and are cleaned up once, since no cluster claims them.
/// An empty `hosts` therefore strips this cluster's lines instead of leaving
/// them behind, which is what makes a destroyed cluster stop resolving.
fn rewrite_hosts(
    current: &str,
    marker: &str,
    target_ip: &str,
    hosts: &[String],
) -> (String, Vec<String>) {
    let cleaned: Vec<&str> = current
        .lines()
        .filter(|line| !line.contains(marker) && !is_legacy_marker(line))
        .collect();

    let mut new_entries: Vec<String> = hosts
        .iter()
        .map(|host| format!("{} {} {}", target_ip, host, marker))
        .collect();
    new_entries.sort();

    let mut content = cleaned.join("\n");
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !new_entries.is_empty() {
        content.push_str(&new_entries.join("\n"));
        content.push('\n');
    }

    (content, new_entries)
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
    /// IP address to use in /etc/hosts entries (127.0.0.1 for local, remote host IP for remote Docker)
    target_ip: String,
    kube_ops: KubeOps,
}

impl IngressManager {
    /// Build a manager bound to one cluster: its own hosts marker, its own
    /// domain and its own kubeconfig context. Two clusters would otherwise
    /// rewrite each other's /etc/hosts block every `HostsRefresh` tick.
    pub fn for_cluster(config: &ClusterConfig) -> Self {
        use crate::cluster::platform::PlatformInfo;
        let target_ip = PlatformInfo::docker_remote_host()
            .unwrap_or("127.0.0.1")
            .to_string();
        Self {
            hosts_marker: config.hosts_marker(),
            domain: Some(config.domain.clone()),
            target_ip,
            kube_ops: KubeOps::for_cluster(config),
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

    /// Read ALL hosts from /etc/hosts that point to our target IP (for checking if domain is resolvable)
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
            // Line format: "<ip> hostname [hostname2 ...]"
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Match 127.0.0.1 (always) and the configured target IP (may be remote host)
            if parts.len() >= 2 && (parts[0] == "127.0.0.1" || parts[0] == self.target_ip) {
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

        // Read current /etc/hosts
        let hosts_path = hosts_file_path();
        let current_content = fs::read_to_string(&hosts_path).await.unwrap_or_default();

        let (final_content, new_entries) = rewrite_hosts(
            &current_content,
            &self.hosts_marker,
            &self.target_ip,
            &hosts,
        );

        if hosts.is_empty() {
            // Nothing to publish for this cluster, but the file may still carry
            // its lines from an earlier run: rewriting strips them, which is how
            // a destroyed cluster stops resolving.
            if final_content == current_content {
                if let Some(tx) = &output_tx {
                    let _ = tx.send(OutputLine::info("No ingress hosts found")).await;
                }
                return Ok(HostsUpdateResult::NoUpdateNeeded);
            }
            if let Some(tx) = &output_tx {
                let _ = tx
                    .send(OutputLine::info(
                        "No ingress hosts left, removing this cluster's /etc/hosts entries",
                    ))
                    .await;
            }
        } else {
            // Check if update is needed - check ALL hosts in /etc/hosts
            let hosts_set: HashSet<String> = hosts.iter().cloned().collect();
            let etc_hosts = self.get_all_hosts_from_etc_hosts().await;
            if hosts_set.is_subset(&etc_hosts) && final_content == current_content {
                if let Some(tx) = &output_tx {
                    let _ = tx
                        .send(OutputLine::info(
                            "All hosts already in /etc/hosts, skipping update",
                        ))
                        .await;
                }
                return Ok(HostsUpdateResult::NoUpdateNeeded);
            }
        }

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
                let message = if hosts.is_empty() {
                    "Removed this cluster's /etc/hosts entries".to_string()
                } else {
                    format!("Updated /etc/hosts with {} entries", hosts.len())
                };
                let _ = tx.send(OutputLine::success(message)).await;
            }
            return Ok(HostsUpdateResult::WrittenDirectly { count: hosts.len() });
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

#[cfg(test)]
mod tests {
    use super::rewrite_hosts;

    const MINE: &str = "# k3dev-ingress[alpha]";
    const THEIRS: &str = "# k3dev-ingress[beta]";

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn empty_host_list_strips_this_clusters_lines_and_keeps_the_rest() {
        let current = format!(
            "127.0.0.1 localhost\n127.0.0.1 app.alpha.dev {MINE}\n127.0.0.1 api.alpha.dev {MINE}\n127.0.0.1 app.beta.dev {THEIRS}\n"
        );

        let (content, entries) = rewrite_hosts(&current, MINE, "127.0.0.1", &[]);

        assert!(entries.is_empty());
        assert_eq!(
            content,
            format!("127.0.0.1 localhost\n127.0.0.1 app.beta.dev {THEIRS}\n")
        );
    }

    #[test]
    fn empty_host_list_changes_nothing_when_the_cluster_owns_no_lines() {
        let current = format!("127.0.0.1 localhost\n127.0.0.1 app.beta.dev {THEIRS}\n");

        let (content, entries) = rewrite_hosts(&current, MINE, "127.0.0.1", &[]);

        assert!(entries.is_empty());
        assert_eq!(content, current, "a no-op rewrite must not touch the file");
    }

    #[test]
    fn hosts_replace_this_clusters_previous_lines() {
        let current = format!("127.0.0.1 localhost\n127.0.0.1 old.alpha.dev {MINE}\n127.0.0.1 app.beta.dev {THEIRS}\n");

        let (content, entries) =
            rewrite_hosts(&current, MINE, "127.0.0.1", &hosts(&["new.alpha.dev"]));

        assert_eq!(entries, vec![format!("127.0.0.1 new.alpha.dev {MINE}")]);
        assert!(!content.contains("old.alpha.dev"));
        assert!(content.contains(&format!("127.0.0.1 app.beta.dev {THEIRS}")));
        assert!(content.contains(&format!("127.0.0.1 new.alpha.dev {MINE}")));
    }

    #[test]
    fn legacy_unbracketed_lines_are_dropped_too() {
        let current = format!("127.0.0.1 localhost\n127.0.0.1 old.dev # k3dev-ingress\n127.0.0.1 app.beta.dev {THEIRS}\n");

        let (content, _) = rewrite_hosts(&current, MINE, "127.0.0.1", &[]);

        assert!(!content.contains("old.dev"));
        assert!(content.contains(&format!("127.0.0.1 app.beta.dev {THEIRS}")));
    }

    #[test]
    fn rewriting_twice_is_stable() {
        let current = "127.0.0.1 localhost\n".to_string();
        let wanted = hosts(&["b.alpha.dev", "a.alpha.dev"]);

        let (once, _) = rewrite_hosts(&current, MINE, "127.0.0.1", &wanted);
        let (twice, _) = rewrite_hosts(&once, MINE, "127.0.0.1", &wanted);

        assert_eq!(once, twice);
        // Entries are sorted, so the file order never flaps between runs
        assert!(once.find("a.alpha.dev") < once.find("b.alpha.dev"));
    }
}
