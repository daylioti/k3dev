use anyhow::{anyhow, Result};
use bollard::Docker;
use once_cell::sync::Lazy;
use std::path::PathBuf;

/// Detected CPU architecture
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Architecture {
    Amd64,
    Arm64,
}

/// Whether Docker is local (unix socket) or remote (TCP/SSH)
#[derive(Debug, Clone, PartialEq)]
pub enum DockerLocation {
    /// Docker daemon is on the local machine (unix socket or default)
    Local,
    /// Docker daemon is on a remote machine at the given hostname/IP
    Remote(String),
}

/// Cached Docker location detection (checked once at startup)
static DOCKER_LOCATION: Lazy<DockerLocation> = Lazy::new(detect_docker_location);

/// Check if a hostname is a loopback address (localhost, 127.x.x.x, ::1)
fn is_loopback(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if host == "::1" || host == "[::1]" {
        return true;
    }
    // Check 127.0.0.0/8 range
    if let Ok(addr) = host.parse::<std::net::Ipv4Addr>() {
        return addr.is_loopback();
    }
    false
}

/// Parse hostname from a remote Docker URL (tcp://, ssh://, http://, https://).
/// Returns None for loopback addresses (localhost, 127.x.x.x, ::1) since those
/// are local-via-TCP (e.g., Colima, OrbStack) rather than truly remote.
fn parse_remote_host(url: &str) -> Option<String> {
    for prefix in &["tcp://", "ssh://", "http://", "https://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            // Strip user@ for ssh://user@host
            let after_user = rest.split('@').next_back().unwrap_or(rest);
            // Strip :port and /path
            let host = after_user.split(':').next().unwrap_or(after_user);
            let host = host.split('/').next().unwrap_or(host);
            if !host.is_empty() {
                // Loopback addresses are local-via-TCP, not truly remote
                if is_loopback(host) {
                    return None;
                }
                return Some(host.to_string());
            }
        }
    }
    None
}

/// Check if DOCKER_HOST (or Docker context endpoint) is a TCP/HTTP URL.
/// Returns the full URL if so, None if it's a unix socket or unset.
pub fn docker_host_tcp_url() -> Option<String> {
    // Check DOCKER_HOST env var
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        for prefix in &["tcp://", "http://", "https://"] {
            if host.starts_with(prefix) {
                return Some(host);
            }
        }
        return None;
    }

    // Check Docker context endpoint
    if let Some(endpoint) = docker_endpoint_from_context() {
        for prefix in &["tcp://", "http://", "https://"] {
            if endpoint.starts_with(prefix) {
                return Some(endpoint);
            }
        }
    }

    None
}

/// Detect whether Docker is local or remote based on DOCKER_HOST and Docker context
fn detect_docker_location() -> DockerLocation {
    // 1. Check DOCKER_HOST env var
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if let Some(remote_host) = parse_remote_host(&host) {
            return DockerLocation::Remote(remote_host);
        }
        // unix:// or bare path = local
        return DockerLocation::Local;
    }

    // 2. Check Docker context for non-unix endpoints
    if let Some(endpoint) = docker_endpoint_from_context() {
        if let Some(remote_host) = parse_remote_host(&endpoint) {
            return DockerLocation::Remote(remote_host);
        }
    }

    DockerLocation::Local
}

/// Read Docker endpoint URL from Docker context configuration.
/// Returns the raw Host string (e.g., "unix:///var/run/docker.sock" or "tcp://remote:2375")
fn docker_endpoint_from_context() -> Option<String> {
    let docker_dir = dirs::home_dir()?.join(".docker");
    let config_path = docker_dir.join("config.json");
    let config_content = std::fs::read_to_string(config_path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&config_content).ok()?;
    let context_name = config.get("currentContext")?.as_str()?;

    if context_name == "default" || context_name.is_empty() {
        return None;
    }

    let contexts_dir = docker_dir.join("contexts").join("meta");
    let entries = std::fs::read_dir(contexts_dir).ok()?;
    for entry in entries.flatten() {
        let meta_path = entry.path().join("meta.json");
        if let Ok(meta_content) = std::fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_content) {
                let name = meta.get("Name").and_then(|n| n.as_str()).unwrap_or("");
                if name == context_name {
                    return meta
                        .get("Endpoints")
                        .and_then(|e| e.get("docker"))
                        .and_then(|d| d.get("Host"))
                        .and_then(|h| h.as_str())
                        .map(|s| s.to_string());
                }
            }
        }
    }

    None
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

    /// Detect whether Docker is local or remote.
    /// Result is cached — safe to call frequently.
    pub fn docker_location() -> &'static DockerLocation {
        &DOCKER_LOCATION
    }

    /// Returns true if Docker is running on a remote host (TCP/SSH)
    pub fn is_docker_remote() -> bool {
        matches!(*Self::docker_location(), DockerLocation::Remote(_))
    }

    /// Get the remote Docker host address, if Docker is remote
    pub fn docker_remote_host() -> Option<&'static str> {
        match Self::docker_location() {
            DockerLocation::Remote(host) => Some(host.as_str()),
            DockerLocation::Local => None,
        }
    }

    /// Get the Docker socket path for mounting into the k3s container.
    ///
    /// For local Docker: finds the actual socket file on the filesystem.
    /// For remote Docker: returns `/var/run/docker.sock` (the path on the remote host
    /// where Docker's socket exists — Docker resolves bind mounts on its own host).
    /// For TCP Docker (local-via-TCP): returns `/var/run/docker.sock` as a fallback
    /// path — callers should check `docker_host_tcp_url()` to decide whether to
    /// mount the socket or pass DOCKER_HOST as an env var instead.
    pub async fn docker_socket_path(&self) -> Result<PathBuf> {
        // Remote Docker: the k3s container needs the socket from the remote host.
        // Volume mounts are resolved on the Docker host, so /var/run/docker.sock
        // refers to the remote host's socket, which is correct.
        if Self::is_docker_remote() {
            return Ok(PathBuf::from("/var/run/docker.sock"));
        }

        // TCP Docker (local-via-TCP, e.g., Colima/OrbStack with tcp://127.0.0.1:2375):
        // There's no local socket file to mount. Return the default path; the caller
        // (create_cluster/start_from_snapshot) will skip the socket mount and pass
        // DOCKER_HOST as an env var instead.
        if docker_host_tcp_url().is_some() {
            return Ok(PathBuf::from("/var/run/docker.sock"));
        }

        // Local Docker: find the actual socket file
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

        // 4. Provide helpful error with permission diagnostics
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

    /// Get the Docker socket path to use as a bind mount source when creating
    /// the k3s container. On macOS with Docker Desktop, this must be
    /// `/var/run/docker.sock` (the well-known path that Docker Desktop
    /// intercepts and forwards into the VM), not the actual macOS proxy socket.
    /// On Linux or when using DOCKER_HOST, returns the actual socket path.
    pub fn docker_socket_mount_source(&self, socket_path: &std::path::Path) -> String {
        #[cfg(target_os = "macos")]
        {
            // Docker Desktop on macOS requires `/var/run/docker.sock` as the
            // bind mount source. It intercepts this specific path and creates
            // a proper socket connection inside the container's VM. Mounting
            // the macOS proxy socket (e.g., ~/.docker/run/docker.sock) via
            // virtiofs results in a non-functional socket inside the container.
            let default = "/var/run/docker.sock";
            let default_path = std::path::Path::new(default);
            if socket_path != default_path {
                tracing::info!(
                    resolved = %socket_path.display(),
                    mount_source = default,
                    "macOS detected: using standard Docker socket path for container mount"
                );
                return default.to_string();
            }
        }
        socket_path.to_string_lossy().to_string()
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

        // Docker Desktop: ~/.docker/run/docker.sock (macOS) or ~/.docker/desktop/docker.sock (Linux)
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join(".docker")
                    .join("run")
                    .join("docker.sock")
                    .to_string_lossy()
                    .to_string(),
            );
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
    /// For remote Docker, returns "legacy" (safe default — local iptables is irrelevant).
    pub fn detect_iptables_mode() -> &'static str {
        // Remote Docker: local iptables mode is irrelevant to the remote host
        if Self::is_docker_remote() {
            return "legacy";
        }
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

    /// Connect to Docker using the same socket detection logic as DockerManager.
    /// Checks Docker context and common socket locations, falling back to defaults.
    pub fn connect_docker() -> Result<Docker, bollard::errors::Error> {
        // Try the detected socket path first (handles Docker contexts, Desktop, etc.)
        let socket_path = Self::find_docker_socket_sync();
        if socket_path.exists() {
            let uri = format!("unix://{}", socket_path.display());
            return Docker::connect_with_socket(&uri, 120, bollard::API_DEFAULT_VERSION);
        }
        // Fallback to DOCKER_HOST env var or bollard defaults
        Docker::connect_with_defaults()
    }

    /// Check if Docker is available and running.
    /// Retries briefly to handle systemd socket activation, where the socket file
    /// exists but the daemon takes a few seconds to start on first connection.
    pub async fn is_docker_available(&self) -> bool {
        let client = match Self::connect_docker() {
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

    /// Find a binary by name, checking PATH first, then Homebrew fallback on macOS.
    /// On ARM64 macOS, Homebrew installs to `/opt/homebrew/bin` which may not be
    /// in PATH for non-interactive shells.
    pub fn find_binary(name: &str) -> Option<PathBuf> {
        if let Ok(path) = which::which(name) {
            return Some(path);
        }

        // On macOS, check Homebrew paths as fallback
        #[cfg(target_os = "macos")]
        {
            let homebrew_paths = ["/opt/homebrew/bin", "/usr/local/bin"];
            for dir in &homebrew_paths {
                let candidate = std::path::Path::new(dir).join(name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }

        None
    }

    /// Check if kubectl is installed
    pub fn is_kubectl_installed(&self) -> bool {
        Self::find_binary("kubectl").is_some()
    }

    /// Check if helm is installed
    pub fn is_helm_installed(&self) -> bool {
        Self::find_binary("helm").is_some()
    }

    /// Check if mkcert is installed
    #[allow(dead_code)]
    pub fn is_mkcert_installed(&self) -> bool {
        Self::find_binary("mkcert").is_some()
    }

    /// Find Docker socket path synchronously (for use in spawned tasks).
    /// For remote Docker, returns `/var/run/docker.sock` (the remote host's socket path).
    /// For TCP Docker (local-via-TCP), returns `/var/run/docker.sock` as fallback.
    pub fn find_docker_socket_sync() -> PathBuf {
        // Remote Docker: return the standard remote socket path
        if Self::is_docker_remote() {
            return PathBuf::from("/var/run/docker.sock");
        }

        // TCP Docker (local-via-TCP): no local socket file exists
        if docker_host_tcp_url().is_some() {
            return PathBuf::from("/var/run/docker.sock");
        }

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

/// Find an available TCP port starting from the given port.
pub fn find_available_port(start: u16) -> anyhow::Result<u16> {
    for port in start..start + 100 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!("No available port found in range {}-{}", start, start + 100)
}
