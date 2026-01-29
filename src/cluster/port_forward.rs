//! Port forward detection module
//!
//! Detects active port forwards by:
//! 1. Scanning /proc/net/tcp on HOST for listening ports from k8s-related processes
//! 2. Scanning pods in k8s cluster for tunnel containers (ktunnel, telepresence, etc.)

use super::kube_ops::KubeOps;
use crate::ui::components::ActivePortForward;

/// Detector for active port forwards to Kubernetes cluster
pub struct PortForwardDetector {
    #[allow(dead_code)]
    container_name: String,
    #[allow(dead_code)]
    static_ports: Vec<(u16, u16)>,
    kube_ops: KubeOps,
}

impl PortForwardDetector {
    /// Create a new port forward detector
    pub fn new(container_name: String, static_ports: Vec<(u16, u16)>) -> Self {
        Self {
            container_name,
            static_ports,
            kube_ops: KubeOps::new(),
        }
    }

    /// Detect all active port forwards
    pub async fn detect(&mut self) -> Vec<ActivePortForward> {
        let mut forwards = Vec::new();

        // Method 1: Scan host for k8s-related port forwards (kubectl port-forward style)
        let host_forwards = self.detect_host_port_forwards().await;
        forwards.extend(host_forwards);

        // Method 2: Scan inside k3s container for tunnel ports (ktunnel style)
        let container_forwards = self.detect_container_port_forwards().await;
        forwards.extend(container_forwards);

        forwards
    }

    /// Detect port forwards on the host by scanning /proc
    async fn detect_host_port_forwards(&self) -> Vec<ActivePortForward> {
        let mut forwards = Vec::new();

        // Get all listening sockets from /proc/net/tcp and /proc/net/tcp6
        let listening_sockets = get_listening_sockets().await;

        // Build inode -> (port, pid, cmdline) mapping
        let socket_info = map_sockets_to_processes(&listening_sockets).await;

        // Parse cmdlines to find port forwards
        for (port, _pid, cmdline) in socket_info {
            if let Some(pf) = parse_k8s_port_forward(&cmdline, port) {
                forwards.push(pf);
            }
        }

        forwards
    }

    /// Detect tunnel pods in k8s cluster (ktunnel, telepresence, etc.)
    /// These create pods with tunnel images that expose ports
    async fn detect_container_port_forwards(&mut self) -> Vec<ActivePortForward> {
        let mut forwards = Vec::new();

        // Query k8s for pods running tunnel-related images
        let pods = match self.kube_ops.list_all_pods().await {
            Ok(pods) => pods,
            Err(_) => return forwards,
        };

        for pod in pods {
            for container in &pod.containers {
                // Check if image is tunnel-related
                if is_tunnel_image(&container.image) {
                    // Extract deployment name from pod name (remove suffix)
                    let deploy_name = extract_deployment_name(&pod.name);

                    for port in &container.ports {
                        forwards.push(ActivePortForward {
                            local_port: *port,
                            remote_port: *port,
                            target: format!("{}:{}", pod.namespace, deploy_name),
                        });
                    }
                }
            }
        }

        forwards
    }
}

/// Check if a container image is a known tunnel tool
fn is_tunnel_image(image: &str) -> bool {
    let image_lower = image.to_lowercase();
    // Common tunnel tool images
    image_lower.contains("ktunnel")
        || image_lower.contains("telepresence")
        || image_lower.contains("kubefwd")
        || image_lower.contains("ambassador")
        || image_lower.contains("ngrok")
}

/// Extract deployment name from pod name (remove replicaset suffix)
fn extract_deployment_name(pod_name: &str) -> String {
    // Pod names are like: deployment-name-replicaset-hash-pod-hash
    // We want to extract: deployment-name
    let parts: Vec<&str> = pod_name.rsplitn(3, '-').collect();
    if parts.len() >= 3 {
        // parts is reversed, so last element is the deployment name
        parts
            .last()
            .map(|s| s.to_string())
            .unwrap_or_else(|| pod_name.to_string())
    } else {
        pod_name.to_string()
    }
}

/// Listening socket info: (port, inode)
struct ListeningSocket {
    port: u16,
    inode: u64,
}

/// Read /proc/net/tcp and /proc/net/tcp6 for listening sockets
async fn get_listening_sockets() -> Vec<ListeningSocket> {
    let mut sockets = Vec::new();

    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            for line in content.lines().skip(1) {
                // Format: sl local_address rem_address st ...
                // local_address is hex IP:PORT, st=0A means LISTEN
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 10 {
                    continue;
                }

                // Check if state is LISTEN (0A)
                if parts[3] != "0A" {
                    continue;
                }

                // Parse local address (IP:PORT in hex)
                if let Some(port_hex) = parts[1].split(':').nth(1) {
                    if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                        // Parse inode
                        if let Ok(inode) = parts[9].parse::<u64>() {
                            sockets.push(ListeningSocket { port, inode });
                        }
                    }
                }
            }
        }
    }

    sockets
}

/// Map socket inodes to processes by scanning /proc/<pid>/fd
async fn map_sockets_to_processes(sockets: &[ListeningSocket]) -> Vec<(u16, u32, String)> {
    let mut results = Vec::new();

    // Create inode -> port map for quick lookup
    let inode_to_port: std::collections::HashMap<u64, u16> =
        sockets.iter().map(|s| (s.inode, s.port)).collect();

    // Scan /proc for processes
    let proc_dir = match tokio::fs::read_dir("/proc").await {
        Ok(dir) => dir,
        Err(_) => return results,
    };

    let mut entries = proc_dir;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();

        // Only process numeric directories (PIDs)
        let pid: u32 = match path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(p) => p,
            None => continue,
        };

        // Read cmdline first (cheaper check)
        let cmdline_path = path.join("cmdline");
        let cmdline = match tokio::fs::read(&cmdline_path).await {
            Ok(bytes) => bytes
                .split(|&b| b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .collect::<Vec<_>>()
                .join(" "),
            Err(_) => continue,
        };

        // Quick check: skip if not k8s-related
        if !is_k8s_related_process(&cmdline) {
            continue;
        }

        // Scan fd directory for socket links
        let fd_dir = path.join("fd");
        if let Ok(mut fd_entries) = tokio::fs::read_dir(&fd_dir).await {
            while let Ok(Some(fd_entry)) = fd_entries.next_entry().await {
                if let Ok(link) = tokio::fs::read_link(fd_entry.path()).await {
                    let link_str = link.to_string_lossy();
                    // Socket links look like: socket:[12345]
                    if link_str.starts_with("socket:[") {
                        if let Some(inode_str) = link_str
                            .strip_prefix("socket:[")
                            .and_then(|s| s.strip_suffix(']'))
                        {
                            if let Ok(inode) = inode_str.parse::<u64>() {
                                if let Some(&port) = inode_to_port.get(&inode) {
                                    results.push((port, pid, cmdline.clone()));
                                    break; // Found a match for this process
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    results
}

/// Check if a process cmdline indicates it's k8s-related
fn is_k8s_related_process(cmdline: &str) -> bool {
    let cmdline_lower = cmdline.to_lowercase();

    // Keywords that indicate k8s port forwarding tools
    let keywords = [
        "kubectl",
        "port-forward",
        "ktunnel",
        "kubefwd",
        "telepresence",
        "k9s",
        "lens",
        "octant",
        "kubeconfig",
        "kube-proxy",
    ];

    keywords.iter().any(|kw| cmdline_lower.contains(kw))
}

/// Parse a cmdline to extract port forward info
/// Generic: looks for port patterns and k8s resource references
fn parse_k8s_port_forward(cmdline: &str, listening_port: u16) -> Option<ActivePortForward> {
    let parts: Vec<&str> = cmdline.split_whitespace().collect();

    // Look for a k8s resource reference (pod/xxx, svc/xxx, deploy/xxx, deployment/xxx)
    let mut target = String::new();
    let mut found_port_mapping = None;

    for (i, part) in parts.iter().enumerate() {
        // Skip flags
        if part.starts_with('-') {
            continue;
        }

        // Resource reference
        if (part.contains('/') && !part.contains("//") && !part.starts_with('/'))
            && target.is_empty()
        {
            let lower = part.to_lowercase();
            if lower.starts_with("pod/")
                || lower.starts_with("svc/")
                || lower.starts_with("service/")
                || lower.starts_with("deploy/")
                || lower.starts_with("deployment/")
            {
                target = part.to_string();
            }
        }

        // Port mapping (LOCAL:REMOTE or just PORT)
        if part.contains(':') && part.chars().all(|c| c.is_ascii_digit() || c == ':') {
            let port_parts: Vec<&str> = part.split(':').collect();
            if port_parts.len() == 2 {
                if let (Ok(local), Ok(remote)) =
                    (port_parts[0].parse::<u16>(), port_parts[1].parse::<u16>())
                {
                    if local == listening_port {
                        found_port_mapping = Some((local, remote));
                    }
                }
            }
        }

        // Also check for deployment/service name after certain keywords
        if target.is_empty() && i > 0 {
            let prev = parts[i - 1].to_lowercase();
            if prev == "expose" || prev == "inject" || prev == "intercept" {
                // This might be a deployment name (e.g., ktunnel expose myapp 8080:80)
                if !part.contains(':') && !part.starts_with('-') && !part.is_empty() {
                    target = format!("deploy/{}", part);
                }
            }
        }
    }

    // If we found a port mapping, use it; otherwise use the listening port
    let (local_port, remote_port) = found_port_mapping.unwrap_or((listening_port, listening_port));

    // If no target found but we have a k8s-related process, use a generic target
    if target.is_empty() {
        // Try to extract binary name as hint
        if let Some(bin) = parts.first() {
            let bin_name = bin.rsplit('/').next().unwrap_or(bin);
            if bin_name.is_empty() || bin_name == "kubectl" {
                return None; // Need more info
            }
            target = format!("via {}", bin_name);
        } else {
            return None;
        }
    }

    Some(ActivePortForward {
        local_port,
        remote_port,
        target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_tunnel_image() {
        assert!(is_tunnel_image("ghcr.io/ktunnel/ktunnel:latest"));
        assert!(is_tunnel_image("telepresence/tel2:2.0"));
        assert!(is_tunnel_image("some-registry/ngrok-agent:1.0"));
        assert!(!is_tunnel_image("nginx:latest"));
        assert!(!is_tunnel_image("redis:7"));
    }

    #[test]
    fn test_extract_deployment_name() {
        assert_eq!(extract_deployment_name("myapp-7f9d8c6b5-x4z2k"), "myapp");
        assert_eq!(extract_deployment_name("simple-pod"), "simple-pod");
    }

    #[test]
    fn test_is_k8s_related_process() {
        assert!(is_k8s_related_process(
            "kubectl port-forward svc/myapp 8080:80"
        ));
        assert!(is_k8s_related_process("ktunnel expose myapp 8080:80"));
        assert!(!is_k8s_related_process("nginx -g daemon off"));
    }

    #[test]
    fn test_parse_k8s_port_forward() {
        let pf = parse_k8s_port_forward("kubectl port-forward svc/myapp 8080:80", 8080);
        assert!(pf.is_some());
        let pf = pf.unwrap();
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_port, 80);
        assert_eq!(pf.target, "svc/myapp");
    }
}
