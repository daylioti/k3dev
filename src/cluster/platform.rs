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
        // Only Linux is supported
        if !cfg!(target_os = "linux") {
            return Err(anyhow!("Only Linux is supported"));
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

    /// Get the Docker socket path
    pub async fn docker_socket_path(&self) -> Result<PathBuf> {
        let sock = PathBuf::from("/var/run/docker.sock");
        if sock.exists() {
            Ok(sock)
        } else {
            Err(anyhow!("Docker socket not found at /var/run/docker.sock"))
        }
    }

    /// Check if Docker is available and running
    pub async fn is_docker_available(&self) -> bool {
        // Try to get the socket path and connect via bollard
        let socket_path = match self.docker_socket_path().await {
            Ok(path) => path,
            Err(_) => return false,
        };

        let client = match Docker::connect_with_unix(
            &socket_path.to_string_lossy(),
            120,
            bollard::API_DEFAULT_VERSION,
        ) {
            Ok(c) => c,
            Err(_) => return false,
        };

        client.ping().await.is_ok()
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
