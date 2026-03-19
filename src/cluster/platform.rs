use anyhow::{anyhow, Result};
use bollard::Docker;
use std::path::PathBuf;

/// Detected CPU architecture
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Architecture {
    Amd64,
    Arm64,
}

/// Platform detection and runtime management
pub struct PlatformInfo {
    pub arch: Architecture,
}

impl PlatformInfo {
    /// Detect the current platform
    pub fn detect() -> Result<Self> {
        // Linux and macOS are supported
        if !cfg!(target_os = "linux") && !cfg!(target_os = "macos") {
            return Err(anyhow!(
                "Unsupported platform. k3dev requires Linux or macOS."
            ));
        }

        // Detect WSL1 (syscall translation layer, no real kernel — Docker won't work)
        #[cfg(target_os = "linux")]
        if Self::is_wsl1() {
            return Err(anyhow!(
                "WSL1 detected (no real Linux kernel). k3dev requires WSL2 or native Linux. \
                 Convert with: wsl --set-version <distro> 2"
            ));
        }

        let arch = if cfg!(target_arch = "x86_64") {
            Architecture::Amd64
        } else if cfg!(target_arch = "aarch64") {
            Architecture::Arm64
        } else {
            return Err(anyhow!("Unsupported architecture"));
        };

        Ok(Self { arch })
    }

    /// Detect if running under WSL1 (no real Linux kernel)
    /// WSL1 has "Microsoft" in /proc/version but no /proc/sys/fs/binfmt_misc/WSLInterop
    /// WSL2 has a real kernel and WSLInterop exists
    #[cfg(target_os = "linux")]
    fn is_wsl1() -> bool {
        let proc_version = std::fs::read_to_string("/proc/version").unwrap_or_default();
        if !proc_version.contains("microsoft") && !proc_version.contains("Microsoft") {
            return false; // Not WSL at all
        }
        // WSL2 has a real kernel — check for WSLInterop or kernel version >= 4.19
        // WSL1 uses syscall translation and typically reports kernel 4.4.x
        !std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
    }

    /// Get the Docker socket path
    ///
    /// Checks `DOCKER_HOST` env var first (stripping `unix://` prefix if present),
    /// then tries common socket locations in order.
    pub async fn docker_socket_path(&self) -> Result<PathBuf> {
        // 1. Check DOCKER_HOST env var
        if let Ok(host) = std::env::var("DOCKER_HOST") {
            let path = host.strip_prefix("unix://").unwrap_or(&host);
            let sock = PathBuf::from(path);
            if sock.exists() {
                return Ok(sock);
            }
        }

        // 2. Check Docker context (currentContext in ~/.docker/config.json)
        if let Some(sock) = Self::docker_socket_from_context() {
            if sock.exists() {
                return Ok(sock);
            }
        }

        // 3. Try common socket locations
        let candidates = Self::docker_socket_candidates();
        for candidate in &candidates {
            let sock = PathBuf::from(candidate);
            if sock.exists() {
                return Ok(sock);
            }
        }

        // 3. Provide helpful error with permission diagnostics
        let mut error_msg =
            "Docker socket not found. Set DOCKER_HOST or ensure Docker is running.".to_string();

        // Check if a socket exists but might have permission issues
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            for candidate in &candidates {
                let sock = PathBuf::from(candidate);
                if let Ok(meta) = std::fs::metadata(&sock) {
                    let mode = meta.mode();
                    let uid = Self::current_uid().unwrap_or(u32::MAX);
                    if uid != 0 && (mode & 0o006) == 0 {
                        error_msg = format!(
                            "Docker socket found at {} but permission denied. \
                             Add your user to the 'docker' group: sudo usermod -aG docker $USER",
                            candidate
                        );
                        break;
                    }
                }
            }
        }

        let checked = candidates.join(", ");
        Err(anyhow!("{}\nChecked: {}", error_msg, checked))
    }

    /// Common Docker socket paths to check, in priority order
    fn docker_socket_candidates() -> Vec<String> {
        let mut candidates = vec![
            "/var/run/docker.sock".to_string(),
            "/run/docker.sock".to_string(),
        ];

        #[cfg(unix)]
        if let Some(uid) = Self::current_uid() {
            // Rootless Docker
            candidates.push(format!("/run/user/{}/docker.sock", uid));
            // Podman (rootless) — API-compatible for basic operations
            candidates.push(format!("/run/user/{}/podman/podman.sock", uid));
        }

        // Docker Desktop on Linux: ~/.docker/desktop/docker.sock
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join(".docker")
                    .join("desktop")
                    .join("docker.sock")
                    .to_string_lossy()
                    .to_string(),
            );
        }

        // Snap-installed Docker
        candidates.push("/var/snap/docker/current/run/docker.sock".to_string());
        // Podman (rootful)
        candidates.push("/run/podman/podman.sock".to_string());

        candidates
    }

    /// Read Docker socket endpoint from Docker context configuration.
    /// Parses ~/.docker/config.json for currentContext, then reads the
    /// context's endpoint from ~/.docker/contexts/meta/<hash>/meta.json.
    fn docker_socket_from_context() -> Option<PathBuf> {
        let docker_dir = dirs::home_dir()?.join(".docker");

        // Read currentContext from config.json
        let config_path = docker_dir.join("config.json");
        let config_content = std::fs::read_to_string(config_path).ok()?;
        let config: serde_json::Value = serde_json::from_str(&config_content).ok()?;
        let context_name = config.get("currentContext")?.as_str()?;

        // "default" context means use default socket — skip context lookup
        if context_name == "default" || context_name.is_empty() {
            return None;
        }

        // Search context meta directories for the matching context
        let contexts_dir = docker_dir.join("contexts").join("meta");
        let entries = std::fs::read_dir(contexts_dir).ok()?;
        for entry in entries.flatten() {
            let meta_path = entry.path().join("meta.json");
            if let Ok(meta_content) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_content) {
                    let name = meta.get("Name").and_then(|n| n.as_str()).unwrap_or("");
                    if name == context_name {
                        // Extract Docker endpoint host
                        let host = meta
                            .get("Endpoints")
                            .and_then(|e| e.get("docker"))
                            .and_then(|d| d.get("Host"))
                            .and_then(|h| h.as_str())?;
                        let path = host.strip_prefix("unix://").unwrap_or(host);
                        return Some(PathBuf::from(path));
                    }
                }
            }
        }

        None
    }

    /// Get current user's UID (Unix only)
    #[cfg(unix)]
    fn current_uid() -> Option<u32> {
        use std::os::unix::fs::MetadataExt;
        // /proc/self on Linux, current dir metadata as fallback (macOS)
        std::fs::metadata("/proc/self")
            .or_else(|_| std::fs::metadata("."))
            .map(|m| m.uid())
            .ok()
    }

    /// Check if SELinux is enforcing on this system
    /// Returns true if SELinux is in enforcing or permissive mode
    #[allow(dead_code)]
    pub fn is_selinux_active() -> bool {
        #[cfg(target_os = "linux")]
        {
            // Check /sys/fs/selinux/enforce (most reliable)
            if let Ok(content) = std::fs::read_to_string("/sys/fs/selinux/enforce") {
                return matches!(content.trim(), "0" | "1"); // 0=permissive, 1=enforcing
            }
            false
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Detect the host's iptables backend mode ("nft" or "legacy").
    /// K3s should use the same mode to avoid invisible rules.
    /// Returns "nft" if iptables is backed by nftables, "legacy" otherwise.
    pub fn detect_iptables_mode() -> &'static str {
        #[cfg(target_os = "linux")]
        {
            // Check what /usr/sbin/iptables or /sbin/iptables resolves to
            let check_paths = ["/usr/sbin/iptables", "/sbin/iptables", "/usr/bin/iptables"];
            for path in &check_paths {
                if let Ok(resolved) = std::fs::read_link(path) {
                    let resolved_str = resolved.to_string_lossy();
                    if resolved_str.contains("nft") {
                        return "nft";
                    }
                    if resolved_str.contains("legacy") {
                        return "legacy";
                    }
                }
                // Also check iptables --version output
                if let Ok(output) = std::process::Command::new(path).arg("--version").output() {
                    let version = String::from_utf8_lossy(&output.stdout);
                    if version.contains("nf_tables") {
                        return "nft";
                    }
                    if version.contains("legacy") {
                        return "legacy";
                    }
                }
            }
            "legacy"
        }
        #[cfg(not(target_os = "linux"))]
        {
            "legacy"
        }
    }

    /// Check if Docker is available and running.
    /// Retries briefly to handle systemd socket activation, where the socket file
    /// exists but the daemon takes a few seconds to start on first connection.
    pub async fn is_docker_available(&self) -> bool {
        let client = match Docker::connect_with_defaults() {
            Ok(c) => c,
            Err(_) => return false,
        };

        // First attempt
        if client.ping().await.is_ok() {
            return true;
        }

        // Socket activation: socket exists but daemon may be starting up.
        // Retry a few times with short delays.
        for _ in 0..3 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if client.ping().await.is_ok() {
                return true;
            }
        }

        false
    }

    /// Check if kubectl is installed
    pub fn is_kubectl_installed(&self) -> bool {
        which::which("kubectl").is_ok()
    }

    /// Check if helm is installed
    pub fn is_helm_installed(&self) -> bool {
        which::which("helm").is_ok()
    }

    /// Check if mkcert is installed
    #[allow(dead_code)]
    pub fn is_mkcert_installed(&self) -> bool {
        which::which("mkcert").is_ok()
    }

    /// Find Docker socket path synchronously (for use in spawned tasks)
    /// Checks DOCKER_HOST, then common socket locations
    pub fn find_docker_socket_sync() -> PathBuf {
        // Check DOCKER_HOST first
        if let Ok(host) = std::env::var("DOCKER_HOST") {
            let path = host.strip_prefix("unix://").unwrap_or(&host);
            let sock = PathBuf::from(path);
            if sock.exists() {
                return sock;
            }
        }

        // Check Docker context
        if let Some(sock) = Self::docker_socket_from_context() {
            if sock.exists() {
                return sock;
            }
        }

        // Try common locations
        for candidate in Self::docker_socket_candidates() {
            let sock = PathBuf::from(&candidate);
            if sock.exists() {
                return sock;
            }
        }

        // Fallback to default (DockerManager::new will handle the error)
        PathBuf::from("/var/run/docker.sock")
    }

    /// Get all missing prerequisites
    pub async fn get_missing_prerequisites(&self) -> Vec<String> {
        let mut missing = Vec::new();

        if !self.is_docker_available().await {
            missing.push("docker".to_string());
        }

        if !self.is_kubectl_installed() {
            missing.push("kubectl".to_string());
        }

        if !self.is_helm_installed() {
            missing.push("helm".to_string());
        }

        missing
    }
}
