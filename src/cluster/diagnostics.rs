//! Cluster diagnostics — health check runner
//!
//! Runs a series of diagnostic tests against the cluster and reports results
//! incrementally via AppMessage channel.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, Namespace, Node, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PersistentVolumeClaimVolumeSource, Pod, PodSpec, Service, ServicePort, ServiceSpec, Volume,
    VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use tokio::net::TcpStream;

use crate::app::AppMessage;
use crate::cluster::kube_ops::KubeOps;
use crate::cluster::{
    ClusterConfig, DockerManager, IngressHealthChecker, IngressManager, PlatformInfo,
};
use crate::k8s::{K8sClient, PodExecutor};

/// Status of a single diagnostic test
#[derive(Debug, Clone, PartialEq)]
pub enum DiagnosticStatus {
    Pending,
    Running,
    Passed,
    Failed(String),
    Skipped(String),
}

/// A single diagnostic test result
#[derive(Debug, Clone)]
pub struct DiagnosticResult {
    pub id: &'static str,
    pub category: &'static str,
    pub name: String,
    pub status: DiagnosticStatus,
    pub duration: Option<Duration>,
}

/// Full diagnostics report sent to UI
#[derive(Debug, Clone)]
pub struct DiagnosticsReport {
    pub results: Vec<DiagnosticResult>,
    pub finished: bool,
}

impl DiagnosticsReport {
    fn new() -> Self {
        Self {
            results: build_test_list(),
            finished: false,
        }
    }
}

/// Category indices for skip logic
const CAT_PREREQUISITES: &str = "Prerequisites";
const CAT_CLUSTER: &str = "Cluster";
const CAT_CORE_SERVICES: &str = "Core Services";
const CAT_NETWORKING: &str = "Networking";
const CAT_PODS: &str = "Pods";
const CAT_DEEP_VERIFICATION: &str = "Deep Verification";

/// Namespace used for deep verification tests
const DIAG_NAMESPACE: &str = "k3dev-diag";

/// Per-test timeout in seconds. Deep tests need longer than the default 10s.
fn test_timeout(test_id: &str) -> Duration {
    let secs = match test_id {
        "host_ports_reachable" | "no_crash_loops" => 15,
        "deep_setup" => 60,
        "deep_dns" => 90,
        "deep_connectivity" => 120,
        "deep_volume" => 120,
        "deep_host_http" | "deep_docker_in_container" | "deep_runtime_socket" => 30,
        "deep_cleanup" => 30,
        _ => 10,
    };
    Duration::from_secs(secs)
}

/// Initialize a K8s client from cluster config.
async fn init_k8s_client(config: &ClusterConfig) -> Result<K8sClient, String> {
    K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
        .await
        .map_err(|e| format!("client init failed: {}", e))
}

/// Format a list of items, showing at most `max` with a "(+N)" suffix for the rest.
fn truncated_list(items: &[&str], max: usize) -> String {
    let shown: Vec<_> = items.iter().take(max).copied().collect();
    let suffix = if items.len() > max {
        format!(" (+{})", items.len() - max)
    } else {
        String::new()
    };
    format!("{}{}", shown.join(", "), suffix)
}

fn build_test_list() -> Vec<DiagnosticResult> {
    vec![
        // Prerequisites
        DiagnosticResult {
            id: "docker_accessible",
            category: CAT_PREREQUISITES,
            name: "Docker accessible".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "kubectl_installed",
            category: CAT_PREREQUISITES,
            name: "kubectl installed".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "apparmor_check",
            category: CAT_PREREQUISITES,
            name: "AppArmor profile".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "br_netfilter_loaded",
            category: CAT_PREREQUISITES,
            name: "br_netfilter module".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        // Cluster
        DiagnosticResult {
            id: "container_running",
            category: CAT_CLUSTER,
            name: "K3s container running".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "k8s_api_reachable",
            category: CAT_CLUSTER,
            name: "K8s API reachable".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "nodes_ready",
            category: CAT_CLUSTER,
            name: "Node(s) Ready".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "container_health",
            category: CAT_CLUSTER,
            name: "Container health".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "arch_mismatch",
            category: CAT_CLUSTER,
            name: "Architecture match".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        // Core Services
        DiagnosticResult {
            id: "coredns_running",
            category: CAT_CORE_SERVICES,
            name: "CoreDNS running".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "traefik_service",
            category: CAT_CORE_SERVICES,
            name: "Traefik service exists".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "local_path_provisioner",
            category: CAT_CORE_SERVICES,
            name: "local-path-provisioner running".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "flannel_running",
            category: CAT_CORE_SERVICES,
            name: "Flannel CNI configured".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        // Networking
        DiagnosticResult {
            id: "host_ports_reachable",
            category: CAT_NETWORKING,
            name: "Host ports reachable".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "ingress_configured",
            category: CAT_NETWORKING,
            name: "Ingress routes configured".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "hosts_uptodate",
            category: CAT_NETWORKING,
            name: "/etc/hosts up-to-date".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "ingress_healthy",
            category: CAT_NETWORKING,
            name: "Ingress endpoints healthy".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "tls_cert_valid",
            category: CAT_NETWORKING,
            name: "TLS certificate valid".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        // Pods
        DiagnosticResult {
            id: "no_stuck_pods",
            category: CAT_PODS,
            name: "No stuck pods".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "no_pull_errors",
            category: CAT_PODS,
            name: "No ImagePullBackOff".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "no_crash_loops",
            category: CAT_PODS,
            name: "No CrashLoopBackOff".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "node_conditions",
            category: CAT_PODS,
            name: "Node conditions healthy".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        // Deep Verification
        DiagnosticResult {
            id: "deep_setup",
            category: CAT_DEEP_VERIFICATION,
            name: "Create test namespace".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "deep_dns",
            category: CAT_DEEP_VERIFICATION,
            name: "DNS resolution".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "deep_connectivity",
            category: CAT_DEEP_VERIFICATION,
            name: "Pod-to-Service connectivity".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "deep_volume",
            category: CAT_DEEP_VERIFICATION,
            name: "Volume write/read".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "deep_host_http",
            category: CAT_DEEP_VERIFICATION,
            name: "Host HTTP to Traefik".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "deep_docker_in_container",
            category: CAT_DEEP_VERIFICATION,
            name: "Docker socket in container".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "deep_runtime_socket",
            category: CAT_DEEP_VERIFICATION,
            name: "Container runtime socket".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "deep_cleanup",
            category: CAT_DEEP_VERIFICATION,
            name: "Cleanup test resources".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
    ]
}

/// Check if a test should be skipped based on prior failures
fn should_skip(report: &DiagnosticsReport, test_idx: usize) -> Option<&'static str> {
    let test_id = report.results[test_idx].id;
    let test_cat = report.results[test_idx].category;

    // If test is in Prerequisites, never skip
    if test_cat == CAT_PREREQUISITES {
        return None;
    }

    // Only critical prerequisites (Docker, kubectl) block everything else.
    // Advisory checks (AppArmor, br_netfilter) are informational and don't cascade.
    const CRITICAL_PREREQS: &[&str] = &["docker_accessible", "kubectl_installed"];
    let critical_failed = report
        .results
        .iter()
        .filter(|r| CRITICAL_PREREQS.contains(&r.id))
        .any(|r| matches!(r.status, DiagnosticStatus::Failed(_)));
    if critical_failed {
        return Some("prerequisite failed");
    }

    // If any cluster test failed, skip core services / networking / pods / deep
    if test_cat != CAT_CLUSTER {
        let cluster_failed = report
            .results
            .iter()
            .filter(|r| r.category == CAT_CLUSTER)
            .any(|r| matches!(r.status, DiagnosticStatus::Failed(_)));
        if cluster_failed {
            return Some("cluster not healthy");
        }
    }

    // Deep verification skip logic
    if test_cat == CAT_DEEP_VERIFICATION {
        // Deep tests require all core services to pass
        let core_failed = report
            .results
            .iter()
            .filter(|r| r.category == CAT_CORE_SERVICES)
            .any(|r| matches!(r.status, DiagnosticStatus::Failed(_)));
        if core_failed {
            return Some("core services not healthy");
        }

        // deep_cleanup always runs if setup was attempted (passed or failed)
        if test_id == "deep_cleanup" {
            let setup = report.results.iter().find(|r| r.id == "deep_setup");
            return match setup {
                Some(r)
                    if matches!(
                        r.status,
                        DiagnosticStatus::Passed | DiagnosticStatus::Failed(_)
                    ) =>
                {
                    None
                }
                _ => Some("setup not attempted"),
            };
        }

        // dns/connectivity/volume require setup to have passed
        // host_http, docker_in_container, runtime_socket don't need setup
        const DEEP_NEEDS_SETUP: &[&str] = &["deep_dns", "deep_connectivity", "deep_volume"];
        if DEEP_NEEDS_SETUP.contains(&test_id) {
            let setup = report.results.iter().find(|r| r.id == "deep_setup");
            if !matches!(setup, Some(r) if r.status == DiagnosticStatus::Passed) {
                return Some("setup failed");
            }
        }
    }

    None
}

// ==================== Preflight Check Categories ====================

const CAT_PREFLIGHT_SYSTEM: &str = "System";
const CAT_PREFLIGHT_DOCKER: &str = "Docker";
const CAT_PREFLIGHT_PORTS: &str = "Ports";
const CAT_PREFLIGHT_CONFIG: &str = "Configuration";

fn build_preflight_list(config: &ClusterConfig) -> Vec<DiagnosticResult> {
    let mut tests = vec![
        // System
        DiagnosticResult {
            id: "pre_docker_accessible",
            category: CAT_PREFLIGHT_SYSTEM,
            name: "Docker accessible".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "pre_kubectl_installed",
            category: CAT_PREFLIGHT_SYSTEM,
            name: "kubectl installed".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "pre_arch_check",
            category: CAT_PREFLIGHT_SYSTEM,
            name: "Architecture".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        // Docker
        DiagnosticResult {
            id: "pre_docker_info",
            category: CAT_PREFLIGHT_DOCKER,
            name: "Docker daemon healthy".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "pre_docker_disk",
            category: CAT_PREFLIGHT_DOCKER,
            name: "Docker disk space".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "pre_k3s_image",
            category: CAT_PREFLIGHT_DOCKER,
            name: "K3s image available".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "pre_container_conflict",
            category: CAT_PREFLIGHT_DOCKER,
            name: "No container name conflict".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
        DiagnosticResult {
            id: "pre_network_conflict",
            category: CAT_PREFLIGHT_DOCKER,
            name: "No network name conflict".to_string(),
            status: DiagnosticStatus::Pending,
            duration: None,
        },
    ];

    // Ports — add one test per core port
    let core_ports: Vec<(u16, &str)> = vec![
        (config.http_port, "HTTP"),
        (config.https_port, "HTTPS"),
        (config.api_port, "K8s API"),
    ];
    for (port, label) in &core_ports {
        tests.push(DiagnosticResult {
            id: "pre_port", // All port tests share this id; name differentiates
            category: CAT_PREFLIGHT_PORTS,
            name: format!("Port {} ({}) available", port, label),
            status: DiagnosticStatus::Pending,
            duration: None,
        });
    }
    for (host_port, _) in &config.additional_ports {
        tests.push(DiagnosticResult {
            id: "pre_port",
            category: CAT_PREFLIGHT_PORTS,
            name: format!("Port {} (additional) available", host_port),
            status: DiagnosticStatus::Pending,
            duration: None,
        });
    }

    // Configuration
    tests.push(DiagnosticResult {
        id: "pre_kubeconfig_dir",
        category: CAT_PREFLIGHT_CONFIG,
        name: "Kubeconfig directory writable".to_string(),
        status: DiagnosticStatus::Pending,
        duration: None,
    });
    tests.push(DiagnosticResult {
        id: "pre_certs_dir",
        category: CAT_PREFLIGHT_CONFIG,
        name: "Certs directory writable".to_string(),
        status: DiagnosticStatus::Pending,
        duration: None,
    });

    tests
}

/// Preflight skip logic: Docker tests require docker_accessible to pass
fn should_skip_preflight(report: &DiagnosticsReport, test_idx: usize) -> Option<&'static str> {
    let test_cat = report.results[test_idx].category;

    if test_cat == CAT_PREFLIGHT_SYSTEM {
        return None;
    }

    // Docker/Ports categories require Docker to be accessible
    let docker_ok = report
        .results
        .iter()
        .find(|r| r.id == "pre_docker_accessible")
        .map(|r| r.status == DiagnosticStatus::Passed)
        .unwrap_or(false);

    if !docker_ok && (test_cat == CAT_PREFLIGHT_DOCKER || test_cat == CAT_PREFLIGHT_PORTS) {
        return Some("Docker not accessible");
    }

    None
}

/// Execute a single preflight test
async fn execute_preflight_test(
    test_id: &str,
    test_name: &str,
    config: &ClusterConfig,
) -> Result<Option<String>, String> {
    match test_id {
        "pre_docker_accessible" => {
            let platform = PlatformInfo::detect().map_err(|e| e.to_string())?;
            if platform.is_docker_available().await {
                Ok(None)
            } else {
                Err("Docker daemon not reachable".to_string())
            }
        }
        "pre_kubectl_installed" => {
            let platform = PlatformInfo::detect().map_err(|e| e.to_string())?;
            if platform.is_kubectl_installed() {
                Ok(None)
            } else {
                Err("kubectl not found in PATH".to_string())
            }
        }
        "pre_arch_check" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            let info = docker
                .client
                .info()
                .await
                .map_err(|e| format!("docker info failed: {}", e))?;

            let docker_arch = info
                .architecture
                .as_deref()
                .unwrap_or("unknown")
                .to_string();
            let binary_arch = if cfg!(target_arch = "x86_64") {
                "x86_64"
            } else if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "unknown"
            };

            let mismatch = matches!(
                (binary_arch, docker_arch.as_str()),
                ("x86_64", "aarch64") | ("aarch64", "x86_64")
            );

            if mismatch && cfg!(target_os = "macos") {
                Err(format!(
                    "binary={}, Docker={} (Rosetta detected)",
                    binary_arch, docker_arch
                ))
            } else if mismatch {
                Err(format!("binary={}, Docker={}", binary_arch, docker_arch))
            } else {
                Ok(Some(docker_arch))
            }
        }
        "pre_docker_info" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            let info = docker
                .client
                .info()
                .await
                .map_err(|e| format!("daemon error: {}", e))?;

            let server_version = info.server_version.unwrap_or_default();
            let cgroup = info
                .cgroup_driver
                .map(|d| format!("{:?}", d))
                .unwrap_or_else(|| "unknown".to_string());
            Ok(Some(format!("v{}, cgroup={}", server_version, cgroup)))
        }
        "pre_docker_disk" => {
            // Use docker info to check available disk space
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            let info = docker
                .client
                .info()
                .await
                .map_err(|e| format!("docker info failed: {}", e))?;

            // Check images count as a rough proxy for disk usage
            let images = info.images.unwrap_or(0);
            let containers = info.containers.unwrap_or(0);
            let docker_root = info.docker_root_dir.unwrap_or_default();

            // Check disk space on Docker root dir using statvfs
            match nix::sys::statvfs::statvfs(docker_root.as_str()) {
                Ok(stat) => {
                    let avail_gb =
                        (stat.blocks_available() * stat.fragment_size()) / (1024 * 1024 * 1024);
                    if avail_gb < 5 {
                        Err(format!(
                            "low disk: {}G free on {} ({} images, {} containers)",
                            avail_gb, docker_root, images, containers
                        ))
                    } else {
                        Ok(Some(format!("{}G free, {} images", avail_gb, images)))
                    }
                }
                Err(_) => Ok(Some(format!(
                    "{} images, {} containers",
                    images, containers
                ))),
            }
        }
        "pre_k3s_image" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            let image = config.k3s_image();
            if docker.image_exists(&image).await {
                Ok(Some("cached locally".to_string()))
            } else {
                Ok(Some("will be pulled on start".to_string()))
            }
        }
        "pre_container_conflict" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            if docker.container_exists(&config.container_name).await {
                // Container exists — check if it's running or stopped
                if docker.container_running(&config.container_name).await {
                    Err(format!(
                        "'{}' already running — stop or destroy first",
                        config.container_name
                    ))
                } else {
                    Ok(Some(format!(
                        "'{}' exists (stopped) — will be restarted",
                        config.container_name
                    )))
                }
            } else {
                Ok(None)
            }
        }
        "pre_network_conflict" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            let exists = docker
                .client
                .inspect_network(
                    &config.network_name,
                    None::<bollard::query_parameters::InspectNetworkOptions>,
                )
                .await
                .is_ok();
            if exists {
                Ok(Some(format!(
                    "'{}' exists (will be reused)",
                    config.network_name
                )))
            } else {
                Ok(None)
            }
        }
        "pre_port" => {
            // Extract port number from test name: "Port XXXX (...) available"
            let port: u16 = test_name
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| "invalid port test".to_string())?;

            // First check if our own k3dev container already has this port mapped
            let socket_path = PlatformInfo::find_docker_socket_sync();
            if let Ok(docker) = DockerManager::new(socket_path) {
                if let Ok(info) = docker
                    .client
                    .inspect_container(
                        &config.container_name,
                        None::<bollard::query_parameters::InspectContainerOptions>,
                    )
                    .await
                {
                    // Container exists — check if it has this port mapped
                    let has_port = info
                        .host_config
                        .as_ref()
                        .and_then(|hc| hc.port_bindings.as_ref())
                        .map(|bindings| {
                            bindings.values().flatten().flatten().any(|pb| {
                                pb.host_port.as_ref().and_then(|p| p.parse::<u16>().ok())
                                    == Some(port)
                            })
                        })
                        .unwrap_or(false);

                    if has_port {
                        let running = info.state.and_then(|s| s.running).unwrap_or(false);
                        if running {
                            return Ok(Some("in use by k3dev (running)".to_string()));
                        } else {
                            return Ok(Some("mapped by k3dev (stopped)".to_string()));
                        }
                    }
                }
            }

            // Not used by our container — check if port is actually available
            // For privileged ports (< 1024), bind() fails without root even if free,
            // so use TCP connect instead: connection refused = port is free.
            if port < 1024 {
                match std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    std::time::Duration::from_millis(500),
                ) {
                    Ok(_) => Err(format!("port {} in use by another service", port)),
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(None),
                    Err(_) => Ok(None), // Timeout or other error = likely free
                }
            } else {
                match std::net::TcpListener::bind(("127.0.0.1", port)) {
                    Ok(_) => Ok(None),
                    Err(_) => Err(format!("port {} already in use by another service", port)),
                }
            }
        }
        "pre_kubeconfig_dir" => {
            let kc_path = ClusterConfig::kubeconfig_path();
            if let Some(parent) = kc_path.parent() {
                if parent.exists() {
                    Ok(Some(format!("{}", parent.display())))
                } else {
                    // Try to create it
                    match std::fs::create_dir_all(parent) {
                        Ok(_) => Ok(Some(format!("created {}", parent.display()))),
                        Err(e) => Err(format!("{}: {}", parent.display(), e)),
                    }
                }
            } else {
                Ok(None)
            }
        }
        "pre_certs_dir" => {
            let certs_dir = ClusterConfig::certs_dir();
            if certs_dir.exists() {
                Ok(Some(format!("{}", certs_dir.display())))
            } else {
                match std::fs::create_dir_all(&certs_dir) {
                    Ok(_) => Ok(Some(format!("created {}", certs_dir.display()))),
                    Err(e) => Err(format!("{}: {}", certs_dir.display(), e)),
                }
            }
        }
        _ => Err(format!("unknown preflight test: {}", test_id)),
    }
}

/// Run preflight checks (no running cluster required)
pub async fn run_preflight_checks(config: Arc<ClusterConfig>, tx: mpsc::Sender<AppMessage>) {
    let mut report = DiagnosticsReport {
        results: build_preflight_list(&config),
        finished: false,
    };

    let _ = tx
        .send(AppMessage::DiagnosticsUpdated(report.clone()))
        .await;

    for i in 0..report.results.len() {
        if let Some(reason) = should_skip_preflight(&report, i) {
            report.results[i].status = DiagnosticStatus::Skipped(reason.to_string());
            let _ = tx
                .send(AppMessage::DiagnosticsUpdated(report.clone()))
                .await;
            continue;
        }

        report.results[i].status = DiagnosticStatus::Running;
        let _ = tx
            .send(AppMessage::DiagnosticsUpdated(report.clone()))
            .await;

        let start = Instant::now();
        let test_id = report.results[i].id;
        let test_name = report.results[i].name.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            execute_preflight_test(test_id, &test_name, &config),
        )
        .await;

        report.results[i].duration = Some(start.elapsed());

        match result {
            Ok(Ok(msg)) => {
                if let Some(detail) = msg {
                    report.results[i].name = format!("{} ({})", report.results[i].name, detail);
                }
                report.results[i].status = DiagnosticStatus::Passed;
            }
            Ok(Err(reason)) => {
                report.results[i].status = DiagnosticStatus::Failed(reason);
            }
            Err(_) => {
                report.results[i].status = DiagnosticStatus::Failed("timed out".to_string());
            }
        }

        let _ = tx
            .send(AppMessage::DiagnosticsUpdated(report.clone()))
            .await;
    }

    report.finished = true;
    let _ = tx
        .send(AppMessage::DiagnosticsUpdated(report.clone()))
        .await;
}

/// Run all diagnostic tests, sending incremental updates to the UI
pub async fn run_all_diagnostics(config: Arc<ClusterConfig>, tx: mpsc::Sender<AppMessage>) {
    let mut report = DiagnosticsReport::new();

    // Send initial state (all Pending)
    let _ = tx
        .send(AppMessage::DiagnosticsUpdated(report.clone()))
        .await;

    for i in 0..report.results.len() {
        // Check skip logic
        if let Some(reason) = should_skip(&report, i) {
            report.results[i].status = DiagnosticStatus::Skipped(reason.to_string());
            let _ = tx
                .send(AppMessage::DiagnosticsUpdated(report.clone()))
                .await;
            continue;
        }

        // Mark as Running
        report.results[i].status = DiagnosticStatus::Running;
        let _ = tx
            .send(AppMessage::DiagnosticsUpdated(report.clone()))
            .await;

        // Execute with per-test timeout
        let start = Instant::now();
        let test_id = report.results[i].id;
        let result =
            tokio::time::timeout(test_timeout(test_id), execute_test(test_id, &config)).await;

        let elapsed = start.elapsed();
        report.results[i].duration = Some(elapsed);

        match result {
            Ok(Ok(msg)) => {
                // Update name with extra info if provided
                if let Some(detail) = msg {
                    report.results[i].name = format!("{} ({})", report.results[i].name, detail);
                }
                report.results[i].status = DiagnosticStatus::Passed;
            }
            Ok(Err(reason)) => {
                report.results[i].status = DiagnosticStatus::Failed(reason);
            }
            Err(_) => {
                report.results[i].status = DiagnosticStatus::Failed("timed out".to_string());
            }
        }

        let _ = tx
            .send(AppMessage::DiagnosticsUpdated(report.clone()))
            .await;
    }

    report.finished = true;
    let _ = tx
        .send(AppMessage::DiagnosticsUpdated(report.clone()))
        .await;
}

/// Execute a single diagnostic test by ID.
/// Returns Ok(Some(detail)) for passed with extra info, Ok(None) for simple pass, Err(reason) for failure.
async fn execute_test(test_id: &str, config: &ClusterConfig) -> Result<Option<String>, String> {
    match test_id {
        "docker_accessible" => {
            let platform = PlatformInfo::detect().map_err(|e| e.to_string())?;
            if platform.is_docker_available().await {
                Ok(None)
            } else {
                Err("Docker daemon not reachable".to_string())
            }
        }
        "kubectl_installed" => {
            let platform = PlatformInfo::detect().map_err(|e| e.to_string())?;
            if platform.is_kubectl_installed() {
                Ok(None)
            } else {
                Err("kubectl not found in PATH".to_string())
            }
        }
        "apparmor_check" => {
            // Only relevant on Linux
            #[cfg(not(target_os = "linux"))]
            {
                Ok(Some("not applicable (non-Linux)".to_string()))
            }
            #[cfg(target_os = "linux")]
            {
                // Check if AppArmor is active on the system
                let apparmor_active =
                    std::path::Path::new("/sys/kernel/security/apparmor").exists();
                if !apparmor_active {
                    return Ok(Some("not active".to_string()));
                }

                // AppArmor is active — check if Docker's default profile could interfere
                // The k3s container now uses security_opt=apparmor:unconfined, so warn
                // only if AppArmor is in enforcing mode
                let profiles = std::fs::read_to_string("/sys/kernel/security/apparmor/profiles")
                    .unwrap_or_default();

                let docker_profile_enforcing = profiles
                    .lines()
                    .any(|l| l.contains("docker-default") && l.contains("(enforce)"));

                if docker_profile_enforcing {
                    Ok(Some(
                        "active, docker-default enforcing (k3s uses unconfined)".to_string(),
                    ))
                } else {
                    Ok(Some("active".to_string()))
                }
            }
        }
        "br_netfilter_loaded" => {
            // Only relevant on Linux; k3s-in-Docker handles bridge networking
            // inside the container, so this is advisory — not a hard failure.
            #[cfg(not(target_os = "linux"))]
            {
                Ok(Some("not applicable (non-Linux)".to_string()))
            }
            #[cfg(target_os = "linux")]
            {
                let modules = std::fs::read_to_string("/proc/modules").unwrap_or_default();
                let br_loaded = modules.lines().any(|l| l.starts_with("br_netfilter "));

                if br_loaded {
                    let sysctl_val =
                        std::fs::read_to_string("/proc/sys/net/bridge/bridge-nf-call-iptables")
                            .unwrap_or_default();
                    if sysctl_val.trim() == "1" {
                        Ok(Some("loaded, bridge-nf-call-iptables=1".to_string()))
                    } else {
                        Ok(Some(
                            "loaded, bridge-nf-call-iptables!=1 (ok for Docker mode)".to_string(),
                        ))
                    }
                } else {
                    // Not loaded — advisory only since k3s uses Docker mode
                    Ok(Some("not loaded (ok for Docker mode)".to_string()))
                }
            }
        }
        "container_running" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            if docker.container_running(&config.container_name).await {
                Ok(None)
            } else {
                Err(format!("container '{}' not running", config.container_name))
            }
        }
        "k8s_api_reachable" => {
            let k8s = init_k8s_client(config).await?;
            if k8s.is_connected().await {
                Ok(None)
            } else {
                Err("API server not responding".to_string())
            }
        }
        "nodes_ready" => {
            let mut kube = KubeOps::new();
            let nodes = kube
                .list_nodes()
                .await
                .map_err(|e| format!("failed to list nodes: {}", e))?;
            if nodes.is_empty() {
                return Err("no nodes found".to_string());
            }
            let not_ready: Vec<_> = nodes.iter().filter(|n| n.status != "Ready").collect();
            if not_ready.is_empty() {
                Ok(Some(format!("{} node(s)", nodes.len())))
            } else {
                let names: Vec<_> = not_ready.iter().map(|n| n.name.as_str()).collect();
                Err(format!("not ready: {}", names.join(", ")))
            }
        }
        "container_health" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            let info = docker
                .client
                .inspect_container(
                    &config.container_name,
                    None::<bollard::query_parameters::InspectContainerOptions>,
                )
                .await
                .map_err(|e| format!("inspect failed: {}", e))?;

            let restart_count = info.restart_count.unwrap_or(0);
            let started_at = info.state.and_then(|s| s.started_at).unwrap_or_default();

            if restart_count > 5 {
                Err(format!(
                    "high restart count: {} (started: {})",
                    restart_count,
                    started_at.get(..19).unwrap_or(&started_at)
                ))
            } else {
                let ts = started_at
                    .get(..19)
                    .unwrap_or(&started_at)
                    .replace('T', " ");
                let detail = if restart_count > 0 {
                    format!("{} restart(s), up since {}", restart_count, ts)
                } else {
                    format!("up since {}", ts)
                };
                Ok(Some(detail))
            }
        }
        "arch_mismatch" => {
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;
            let info = docker
                .client
                .info()
                .await
                .map_err(|e| format!("docker info failed: {}", e))?;

            let docker_arch = info
                .architecture
                .as_deref()
                .unwrap_or("unknown")
                .to_string();
            let binary_arch = if cfg!(target_arch = "x86_64") {
                "x86_64"
            } else if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else {
                "unknown"
            };

            let mismatch = matches!(
                (binary_arch, docker_arch.as_str()),
                ("x86_64", "aarch64") | ("aarch64", "x86_64")
            );

            if mismatch {
                if cfg!(target_os = "macos") {
                    Err(format!(
                        "binary={}, Docker={} (Rosetta detected)",
                        binary_arch, docker_arch
                    ))
                } else {
                    Err(format!("binary={}, Docker={}", binary_arch, docker_arch))
                }
            } else {
                Ok(Some(docker_arch))
            }
        }
        "coredns_running" => {
            let k8s = init_k8s_client(config).await?;
            let pods = k8s
                .list_pods("kube-system", Some("k8s-app=kube-dns"))
                .await
                .map_err(|e| format!("failed to list pods: {}", e))?;
            let running = pods.iter().filter(|p| p.status == "Running").count();
            if running > 0 {
                Ok(Some(format!("{} pod(s)", running)))
            } else {
                Err("no running CoreDNS pods".to_string())
            }
        }
        "traefik_service" => {
            let mut kube = KubeOps::new();
            if kube.service_exists("traefik", "kube-system").await {
                Ok(None)
            } else {
                Err("traefik service not found in kube-system".to_string())
            }
        }
        "local_path_provisioner" => {
            let k8s = init_k8s_client(config).await?;
            let pods = k8s
                .list_pods("kube-system", Some("app=local-path-provisioner"))
                .await
                .map_err(|e| format!("failed to list pods: {}", e))?;
            let running = pods.iter().filter(|p| p.status == "Running").count();
            if running > 0 {
                Ok(None)
            } else {
                Err("no running local-path-provisioner pods".to_string())
            }
        }
        "flannel_running" => {
            // K3s embeds flannel in the k3s binary — no separate pods.
            // Verify CNI config and flannel subnet exist inside the container.
            let socket_path = PlatformInfo::find_docker_socket_sync();
            let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;

            let cni_check = docker
                .exec_in_container(
                    &config.container_name,
                    &[
                        "sh",
                        "-c",
                        "test -f /var/lib/rancher/k3s/agent/etc/cni/net.d/10-flannel.conflist && echo 'ok' || echo 'missing'",
                    ],
                )
                .await
                .map_err(|e| format!("exec failed: {}", e))?;

            if cni_check.trim() != "ok" {
                return Err("flannel CNI config not found in k3s container".to_string());
            }

            let subnet = docker
                .exec_in_container(
                    &config.container_name,
                    &[
                        "sh",
                        "-c",
                        "cat /run/flannel/subnet.env 2>/dev/null || echo 'missing'",
                    ],
                )
                .await
                .map_err(|e| format!("exec failed: {}", e))?;

            if subnet.contains("FLANNEL_NETWORK") {
                // Extract the network CIDR for display
                let network = subnet
                    .lines()
                    .find(|l| l.starts_with("FLANNEL_NETWORK="))
                    .and_then(|l| l.strip_prefix("FLANNEL_NETWORK="))
                    .unwrap_or("configured");
                Ok(Some(format!("embedded, {}", network)))
            } else {
                Err("flannel subnet not configured".to_string())
            }
        }
        "host_ports_reachable" => {
            let host = if PlatformInfo::is_docker_remote() {
                PlatformInfo::docker_remote_host()
                    .unwrap_or("127.0.0.1")
                    .to_string()
            } else {
                "127.0.0.1".to_string()
            };

            let ports: Vec<(u16, &str)> = vec![
                (config.http_port, "HTTP"),
                (config.https_port, "HTTPS"),
                (config.api_port, "K8s API"),
            ];

            let mut failed = Vec::new();
            let mut passed = 0u32;

            for (port, label) in &ports {
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    TcpStream::connect(format!("{}:{}", host, port)),
                )
                .await
                {
                    Ok(Ok(_)) => passed += 1,
                    _ => failed.push(format!("{}({})", label, port)),
                }
            }

            if failed.is_empty() {
                Ok(Some(format!("{} port(s) reachable", passed)))
            } else {
                Err(format!("unreachable: {}", failed.join(", ")))
            }
        }
        "ingress_configured" => {
            let mut ingress = IngressManager::with_domain(config.domain.clone());
            let entries = ingress
                .get_ingress_entries()
                .await
                .map_err(|e| format!("failed to query ingress: {}", e))?;
            if entries.is_empty() {
                Err("no ingress routes found".to_string())
            } else {
                Ok(Some(format!("{} route(s)", entries.len())))
            }
        }
        "hosts_uptodate" => {
            let mut ingress = IngressManager::with_domain(config.domain.clone());
            let missing = ingress
                .get_missing_hosts()
                .await
                .map_err(|e| format!("failed to check hosts: {}", e))?;
            if missing.is_empty() {
                Ok(None)
            } else {
                let hosts: Vec<_> = missing.iter().map(|s| s.as_str()).collect();
                Err(format!("missing: {}", truncated_list(&hosts, 3)))
            }
        }
        "ingress_healthy" => {
            let mut ingress = IngressManager::with_domain(config.domain.clone());
            let entries = ingress
                .get_ingress_entries()
                .await
                .map_err(|e| format!("failed to query ingress: {}", e))?;
            if entries.is_empty() {
                return Err("no ingress entries to check".to_string());
            }
            let health = IngressHealthChecker::check_endpoints(&entries).await;
            let unhealthy: Vec<_> = health
                .iter()
                .filter(|(_, s)| matches!(s, crate::cluster::IngressHealthStatus::Error))
                .map(|(url, _)| url.as_str())
                .collect();
            if unhealthy.is_empty() {
                Ok(Some(format!("{} endpoint(s)", health.len())))
            } else {
                Err(format!("unhealthy: {}", truncated_list(&unhealthy, 3)))
            }
        }
        "tls_cert_valid" => {
            let certs_dir = ClusterConfig::certs_dir();
            let cert_path = certs_dir.join("local-cert.pem");
            let key_path = certs_dir.join("local-key.pem");

            if !cert_path.exists() {
                return Err(format!("cert not found: {}", cert_path.display()));
            }
            if !key_path.exists() {
                return Err(format!("key not found: {}", key_path.display()));
            }

            // Check cert expiry using x509-parser
            {
                use x509_parser::prelude::*;
                let pem_data =
                    std::fs::read(&cert_path).map_err(|e| format!("failed to read cert: {}", e))?;
                let pem_parsed =
                    ::pem::parse(&pem_data).map_err(|e| format!("failed to parse PEM: {}", e))?;
                match X509Certificate::from_der(pem_parsed.contents()) {
                    Ok((_, cert)) => {
                        let now = chrono::Utc::now().timestamp();
                        let not_after = cert.validity().not_after.timestamp();
                        let remaining_secs = not_after - now;
                        if remaining_secs < 0 {
                            Err("certificate expired".to_string())
                        } else if remaining_secs < 86400 {
                            Err("certificate expiring within 24h".to_string())
                        } else {
                            Ok(Some("valid, not expiring within 24h".to_string()))
                        }
                    }
                    Err(e) => Err(format!("failed to parse certificate: {}", e)),
                }
            }
        }
        "no_stuck_pods" => {
            let k8s = init_k8s_client(config).await?;
            let pending = k8s
                .list_pending_pods()
                .await
                .map_err(|e| format!("failed to list pods: {}", e))?;
            if pending.is_empty() {
                Ok(None)
            } else {
                let names: Vec<_> = pending.iter().map(|p| p.name.as_str()).collect();
                Err(format!("stuck: {}", truncated_list(&names, 3)))
            }
        }
        "no_pull_errors" => {
            let k8s = init_k8s_client(config).await?;
            let pending = k8s
                .list_pending_pods()
                .await
                .map_err(|e| format!("failed to list pods: {}", e))?;
            let pull_errors: Vec<_> = pending
                .iter()
                .filter(|p| {
                    p.containers
                        .iter()
                        .any(|c| matches!(c.reason.as_str(), "ImagePullBackOff" | "ErrImagePull"))
                })
                .collect();
            if pull_errors.is_empty() {
                Ok(None)
            } else {
                let names: Vec<_> = pull_errors
                    .iter()
                    .take(3)
                    .map(|p| p.name.as_str())
                    .collect();
                Err(format!("pull errors: {}", names.join(", ")))
            }
        }
        "no_crash_loops" => {
            let k8s = init_k8s_client(config).await?;
            let client = k8s.client().clone();
            let namespaces = k8s
                .list_namespaces()
                .await
                .map_err(|e| format!("failed to list namespaces: {}", e))?;

            let mut problems = Vec::new();

            for ns in &namespaces {
                let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
                let list = pods
                    .list(&ListParams::default())
                    .await
                    .map_err(|e| format!("failed to list pods in {}: {}", ns, e))?;

                for pod in list.items {
                    let pod_name = pod.metadata.name.clone().unwrap_or_default();
                    if let Some(status) = &pod.status {
                        let all_statuses = status
                            .container_statuses
                            .iter()
                            .flatten()
                            .chain(status.init_container_statuses.iter().flatten());

                        for cs in all_statuses {
                            if let Some(state) = &cs.state {
                                if let Some(waiting) = &state.waiting {
                                    if waiting.reason.as_deref() == Some("CrashLoopBackOff") {
                                        problems
                                            .push(format!("{}/{}: CrashLoopBackOff", ns, pod_name));
                                    }
                                }
                                if let Some(terminated) = &state.terminated {
                                    let reason = terminated.reason.as_deref().unwrap_or("");
                                    if reason == "OOMKilled" || reason == "Error" {
                                        problems.push(format!("{}/{}: {}", ns, pod_name, reason));
                                    }
                                }
                            }
                            // High restart count without an obvious state issue
                            if cs.restart_count > 10 {
                                let already = problems.iter().any(|p| p.contains(&pod_name));
                                if !already {
                                    problems.push(format!(
                                        "{}/{}: {} restarts",
                                        ns, pod_name, cs.restart_count
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            if problems.is_empty() {
                Ok(None)
            } else {
                let shown: Vec<_> = problems.iter().take(3).map(|s| s.as_str()).collect();
                Err(truncated_list(&shown, 3))
            }
        }
        "node_conditions" => {
            let k8s = init_k8s_client(config).await?;
            let client = k8s.client().clone();
            let nodes: Api<Node> = Api::all(client);
            let list = nodes
                .list(&ListParams::default())
                .await
                .map_err(|e| format!("failed to list nodes: {}", e))?;

            let pressure_types = ["MemoryPressure", "DiskPressure", "PIDPressure"];
            let mut warnings = Vec::new();

            for node in &list.items {
                let node_name = node.metadata.name.as_deref().unwrap_or("unknown");
                if let Some(status) = &node.status {
                    if let Some(conditions) = &status.conditions {
                        for cond in conditions {
                            if pressure_types.contains(&cond.type_.as_str())
                                && cond.status == "True"
                            {
                                warnings.push(format!("{}: {}", node_name, cond.type_));
                            }
                        }
                    }
                }
            }

            if warnings.is_empty() {
                Ok(Some("no pressure conditions".to_string()))
            } else {
                Err(format!("pressure: {}", warnings.join(", ")))
            }
        }
        // Deep Verification tests
        "deep_setup" => deep_setup(config).await,
        "deep_dns" => deep_dns(config).await,
        "deep_connectivity" => deep_connectivity(config).await,
        "deep_volume" => deep_volume(config).await,
        "deep_host_http" => deep_host_http(config).await,
        "deep_docker_in_container" => deep_docker_in_container(config).await,
        "deep_runtime_socket" => deep_runtime_socket(config).await,
        "deep_cleanup" => deep_cleanup(config).await,

        _ => Err(format!("unknown test: {}", test_id)),
    }
}

// ==================== Deep Verification Helpers ====================

/// Wait for a pod to reach Running phase with all containers ready
async fn wait_for_pod_running(
    k8s: &K8sClient,
    namespace: &str,
    name: &str,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(format!(
                "pod '{}' not ready within {}s",
                name,
                timeout.as_secs()
            ));
        }
        match k8s.get_pod(namespace, name).await {
            Ok(pod) if pod.status == "Running" && pod.ready => return Ok(()),
            Ok(_) => {}
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Delete a pod if it exists, ignoring NotFound errors
async fn delete_pod_if_exists(client: &kube::Client, namespace: &str, name: &str) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let _ = pods.delete(name, &DeleteParams::default()).await;
}

// ==================== Deep Verification Tests ====================

/// Create the k3dev-diag namespace (clean up stale one first)
async fn deep_setup(config: &ClusterConfig) -> Result<Option<String>, String> {
    let k8s = init_k8s_client(config).await?;
    let client = k8s.client().clone();
    let namespaces: Api<Namespace> = Api::all(client.clone());

    // Delete stale namespace if it exists
    let _ = namespaces
        .delete(DIAG_NAMESPACE, &DeleteParams::default())
        .await;

    // Wait for namespace to be fully gone
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(30) {
            return Err("timed out waiting for stale namespace deletion".to_string());
        }
        match namespaces.get(DIAG_NAMESPACE).await {
            Err(_) => break, // gone
            Ok(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }

    // Create fresh namespace
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(DIAG_NAMESPACE.to_string()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".to_string(),
                "k3dev-diag".to_string(),
            )])),
            ..Default::default()
        },
        ..Default::default()
    };
    namespaces
        .create(&PostParams::default(), &ns)
        .await
        .map_err(|e| format!("failed to create namespace: {}", e))?;

    Ok(Some(DIAG_NAMESPACE.to_string()))
}

/// Deploy busybox pod and exec nslookup to verify DNS works
async fn deep_dns(config: &ClusterConfig) -> Result<Option<String>, String> {
    let k8s = init_k8s_client(config).await?;
    let client = k8s.client().clone();
    let pods: Api<Pod> = Api::namespaced(client.clone(), DIAG_NAMESPACE);

    let pod_name = "diag-dns";

    // Create busybox pod that sleeps
    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(pod_name.to_string()),
            namespace: Some(DIAG_NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "busybox".to_string(),
                image: Some("busybox:1.36".to_string()),
                command: Some(vec!["sleep".to_string(), "300".to_string()]),
                ..Default::default()
            }],
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    pods.create(&PostParams::default(), &pod)
        .await
        .map_err(|e| format!("failed to create dns test pod: {}", e))?;

    // Wait for pod to be running
    wait_for_pod_running(&k8s, DIAG_NAMESPACE, pod_name, Duration::from_secs(60)).await?;

    // Exec nslookup
    let executor = PodExecutor::new(&k8s);
    let result = executor
        .exec(
            DIAG_NAMESPACE,
            pod_name,
            None,
            vec![
                "nslookup".to_string(),
                "kubernetes.default.svc.cluster.local".to_string(),
            ],
        )
        .await
        .map_err(|e| format!("exec failed: {}", e))?;

    // Cleanup pod
    delete_pod_if_exists(&client, DIAG_NAMESPACE, pod_name).await;

    if result.exit_code == 0 {
        Ok(Some("kubernetes.default resolved".to_string()))
    } else {
        Err(format!(
            "nslookup failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        ))
    }
}

/// Deploy nginx + service + client pod, verify wget from client to nginx service
async fn deep_connectivity(config: &ClusterConfig) -> Result<Option<String>, String> {
    let k8s = init_k8s_client(config).await?;
    let client = k8s.client().clone();

    let pods_api: Api<Pod> = Api::namespaced(client.clone(), DIAG_NAMESPACE);
    let services_api: Api<Service> = Api::namespaced(client.clone(), DIAG_NAMESPACE);

    // Create nginx server pod
    let nginx_pod = Pod {
        metadata: ObjectMeta {
            name: Some("diag-nginx".to_string()),
            namespace: Some(DIAG_NAMESPACE.to_string()),
            labels: Some(BTreeMap::from([(
                "app".to_string(),
                "diag-nginx".to_string(),
            )])),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "nginx".to_string(),
                image: Some("nginx:alpine".to_string()),
                ports: Some(vec![ContainerPort {
                    container_port: 80,
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    pods_api
        .create(&PostParams::default(), &nginx_pod)
        .await
        .map_err(|e| format!("failed to create nginx pod: {}", e))?;

    // Create service pointing to nginx
    let svc = Service {
        metadata: ObjectMeta {
            name: Some("diag-nginx".to_string()),
            namespace: Some(DIAG_NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                "app".to_string(),
                "diag-nginx".to_string(),
            )])),
            ports: Some(vec![ServicePort {
                port: 80,
                target_port: Some(IntOrString::Int(80)),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    services_api
        .create(&PostParams::default(), &svc)
        .await
        .map_err(|e| format!("failed to create nginx service: {}", e))?;

    // Wait for nginx pod
    wait_for_pod_running(&k8s, DIAG_NAMESPACE, "diag-nginx", Duration::from_secs(60)).await?;

    // Create client pod
    let client_pod = Pod {
        metadata: ObjectMeta {
            name: Some("diag-client".to_string()),
            namespace: Some(DIAG_NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "busybox".to_string(),
                image: Some("busybox:1.36".to_string()),
                command: Some(vec!["sleep".to_string(), "300".to_string()]),
                ..Default::default()
            }],
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    pods_api
        .create(&PostParams::default(), &client_pod)
        .await
        .map_err(|e| format!("failed to create client pod: {}", e))?;

    // Wait for client pod
    wait_for_pod_running(&k8s, DIAG_NAMESPACE, "diag-client", Duration::from_secs(60)).await?;

    // wget from client to nginx service
    let executor = PodExecutor::new(&k8s);
    let result = executor
        .exec(
            DIAG_NAMESPACE,
            "diag-client",
            None,
            vec![
                "wget".to_string(),
                "-q".to_string(),
                "-O".to_string(),
                "-".to_string(),
                "--timeout=10".to_string(),
                format!("http://diag-nginx.{}.svc.cluster.local", DIAG_NAMESPACE),
            ],
        )
        .await
        .map_err(|e| format!("exec failed: {}", e))?;

    // Cleanup pods and service
    delete_pod_if_exists(&client, DIAG_NAMESPACE, "diag-nginx").await;
    delete_pod_if_exists(&client, DIAG_NAMESPACE, "diag-client").await;
    let _ = services_api
        .delete("diag-nginx", &DeleteParams::default())
        .await;

    if result.exit_code == 0 {
        Ok(Some("pod-to-service OK".to_string()))
    } else {
        Err(format!(
            "wget failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        ))
    }
}

/// Create PVC, mount in pod, write file, read it back
async fn deep_volume(config: &ClusterConfig) -> Result<Option<String>, String> {
    let k8s = init_k8s_client(config).await?;
    let client = k8s.client().clone();

    let pods_api: Api<Pod> = Api::namespaced(client.clone(), DIAG_NAMESPACE);
    let pvcs_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), DIAG_NAMESPACE);

    let pod_name = "diag-volume";
    let pvc_name = "diag-pvc";

    // Create PVC using local-path provisioner (default StorageClass in k3s)
    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name.to_string()),
            namespace: Some(DIAG_NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity("64Mi".to_string()),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    pvcs_api
        .create(&PostParams::default(), &pvc)
        .await
        .map_err(|e| format!("failed to create PVC: {}", e))?;

    // Create pod that mounts the PVC
    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(pod_name.to_string()),
            namespace: Some(DIAG_NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "busybox".to_string(),
                image: Some("busybox:1.36".to_string()),
                command: Some(vec!["sleep".to_string(), "300".to_string()]),
                volume_mounts: Some(vec![VolumeMount {
                    name: "test-vol".to_string(),
                    mount_path: "/data".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: "test-vol".to_string(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: pvc_name.to_string(),
                    read_only: Some(false),
                }),
                ..Default::default()
            }]),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    pods_api
        .create(&PostParams::default(), &pod)
        .await
        .map_err(|e| format!("failed to create volume test pod: {}", e))?;

    // Wait for pod (PVC binding + image pull can be slow)
    wait_for_pod_running(&k8s, DIAG_NAMESPACE, pod_name, Duration::from_secs(90)).await?;

    // Write a file
    let executor = PodExecutor::new(&k8s);
    let write_result = executor
        .exec_simple(
            DIAG_NAMESPACE,
            pod_name,
            None,
            "echo 'k3dev-diag-ok' > /data/test.txt",
        )
        .await
        .map_err(|e| format!("write exec failed: {}", e))?;

    if write_result.exit_code != 0 {
        delete_pod_if_exists(&client, DIAG_NAMESPACE, pod_name).await;
        let _ = pvcs_api.delete(pvc_name, &DeleteParams::default()).await;
        return Err(format!("write failed: {}", write_result.stderr.trim()));
    }

    // Read it back
    let read_result = executor
        .exec_simple(DIAG_NAMESPACE, pod_name, None, "cat /data/test.txt")
        .await
        .map_err(|e| format!("read exec failed: {}", e))?;

    // Cleanup
    delete_pod_if_exists(&client, DIAG_NAMESPACE, pod_name).await;
    let _ = pvcs_api.delete(pvc_name, &DeleteParams::default()).await;

    if read_result.exit_code == 0 && read_result.stdout.trim() == "k3dev-diag-ok" {
        Ok(Some("write/read OK".to_string()))
    } else {
        Err(format!(
            "read-back mismatch: got '{}'",
            read_result.stdout.trim()
        ))
    }
}

/// HTTP GET from host to Traefik — verifies full port mapping + Traefik chain
async fn deep_host_http(config: &ClusterConfig) -> Result<Option<String>, String> {
    let host = if PlatformInfo::is_docker_remote() {
        PlatformInfo::docker_remote_host()
            .unwrap_or("127.0.0.1")
            .to_string()
    } else {
        "127.0.0.1".to_string()
    };

    let url = format!("http://{}:{}", host, config.http_port);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("HTTP GET {} failed: {}", url, e))?;

    let status = resp.status().as_u16();
    // Any response from Traefik proves the path works (404 = no matching routes, etc.)
    Ok(Some(format!("HTTP {}", status)))
}

/// Verify Docker socket is accessible inside the k3s container
async fn deep_docker_in_container(config: &ClusterConfig) -> Result<Option<String>, String> {
    let socket_path = PlatformInfo::find_docker_socket_sync();
    let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;

    let output = docker
        .exec_in_container(
            &config.container_name,
            &[
                "sh",
                "-c",
                "test -S /var/run/docker.sock && echo 'ok' || echo 'missing'",
            ],
        )
        .await
        .map_err(|e| format!("exec failed: {}", e))?;

    if output.trim() == "ok" {
        Ok(Some("socket mounted".to_string()))
    } else {
        Err("Docker socket not accessible inside k3s container".to_string())
    }
}

/// Verify container runtime socket (macOS: /proc/1/root/run/docker.sock bypass)
async fn deep_runtime_socket(config: &ClusterConfig) -> Result<Option<String>, String> {
    if cfg!(not(target_os = "macos")) {
        return Ok(Some(
            "not applicable (Linux uses direct socket)".to_string(),
        ));
    }

    let socket_path = PlatformInfo::find_docker_socket_sync();
    let docker = DockerManager::new(socket_path).map_err(|e| e.to_string())?;

    // On macOS (Docker Desktop), k3s uses --container-runtime-endpoint /proc/1/root/run/docker.sock
    // to bypass the proxy socket. Verify this path exists and is a socket.
    let output = docker
        .exec_in_container(
            &config.container_name,
            &[
                "sh",
                "-c",
                "test -S /proc/1/root/run/docker.sock && echo 'ok' || echo 'missing'",
            ],
        )
        .await
        .map_err(|e| format!("exec failed: {}", e))?;

    if output.trim() == "ok" {
        Ok(Some("runtime socket accessible".to_string()))
    } else {
        Err("/proc/1/root/run/docker.sock not accessible (Docker Desktop VM issue)".to_string())
    }
}

/// Delete the k3dev-diag namespace (cascades to all resources)
async fn deep_cleanup(config: &ClusterConfig) -> Result<Option<String>, String> {
    let k8s = init_k8s_client(config).await?;
    let client = k8s.client().clone();
    let namespaces: Api<Namespace> = Api::all(client);

    // Delete namespace (cascades all resources)
    match namespaces
        .delete(DIAG_NAMESPACE, &DeleteParams::default())
        .await
    {
        Ok(_) => Ok(Some(format!("deleted {}", DIAG_NAMESPACE))),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(Some("already clean".to_string())),
        Err(e) => Err(format!("failed to delete namespace: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_test_list() {
        let tests = build_test_list();
        assert_eq!(tests.len(), 30);
        assert!(tests.iter().all(|t| t.status == DiagnosticStatus::Pending));
    }

    #[test]
    fn test_categories_in_order() {
        let tests = build_test_list();
        let categories: Vec<&str> = tests.iter().map(|t| t.category).collect::<Vec<_>>();
        // Categories should appear in order
        let mut seen = Vec::new();
        for cat in &categories {
            if seen.last() != Some(cat) {
                seen.push(*cat);
            }
        }
        assert_eq!(
            seen,
            vec![
                CAT_PREREQUISITES,
                CAT_CLUSTER,
                CAT_CORE_SERVICES,
                CAT_NETWORKING,
                CAT_PODS,
                CAT_DEEP_VERIFICATION,
            ]
        );
    }

    #[test]
    fn test_skip_on_prerequisite_failure() {
        let mut report = DiagnosticsReport::new();
        // Fail a prerequisite
        report.results[0].status = DiagnosticStatus::Failed("docker not available".to_string());

        // Cluster test should be skipped
        let cluster_idx = report
            .results
            .iter()
            .position(|r| r.category == CAT_CLUSTER)
            .unwrap();
        assert!(should_skip(&report, cluster_idx).is_some());

        // Prerequisite tests themselves should not be skipped
        assert!(should_skip(&report, 0).is_none());
        assert!(should_skip(&report, 1).is_none());
    }

    #[test]
    fn test_no_skip_on_advisory_prerequisite_failure() {
        let mut report = DiagnosticsReport::new();
        // Pass critical prerequisites, fail br_netfilter (advisory)
        for r in report.results.iter_mut() {
            if r.category == CAT_PREREQUISITES {
                r.status = DiagnosticStatus::Passed;
            }
        }
        let br_idx = report
            .results
            .iter()
            .position(|r| r.id == "br_netfilter_loaded")
            .unwrap();
        report.results[br_idx].status =
            DiagnosticStatus::Failed("br_netfilter not loaded".to_string());

        // Cluster tests should NOT be skipped (only advisory prereq failed)
        let cluster_idx = report
            .results
            .iter()
            .position(|r| r.category == CAT_CLUSTER)
            .unwrap();
        assert!(should_skip(&report, cluster_idx).is_none());
    }

    #[test]
    fn test_skip_on_cluster_failure() {
        let mut report = DiagnosticsReport::new();
        // Mark all prerequisites as passed
        for r in report.results.iter_mut() {
            if r.category == CAT_PREREQUISITES {
                r.status = DiagnosticStatus::Passed;
            }
        }
        // Fail a cluster test
        let cluster_idx = report
            .results
            .iter()
            .position(|r| r.category == CAT_CLUSTER)
            .unwrap();
        report.results[cluster_idx].status = DiagnosticStatus::Failed("not running".to_string());

        // Core services test should be skipped
        let core_idx = report
            .results
            .iter()
            .position(|r| r.category == CAT_CORE_SERVICES)
            .unwrap();
        assert!(should_skip(&report, core_idx).is_some());

        // Other cluster tests should NOT be skipped
        let next_cluster = report
            .results
            .iter()
            .enumerate()
            .find(|(i, r)| r.category == CAT_CLUSTER && *i > cluster_idx);
        if let Some((idx, _)) = next_cluster {
            assert!(should_skip(&report, idx).is_none());
        }
    }

    #[test]
    fn test_no_skip_when_all_pass() {
        let mut report = DiagnosticsReport::new();
        // Mark all as passed
        for r in report.results.iter_mut() {
            r.status = DiagnosticStatus::Passed;
        }
        // Nothing should be skipped
        for i in 0..report.results.len() {
            assert!(should_skip(&report, i).is_none());
        }
    }

    #[test]
    fn test_deep_skip_on_core_failure() {
        let mut report = DiagnosticsReport::new();
        // Pass prerequisites and cluster
        for r in report.results.iter_mut() {
            if r.category == CAT_PREREQUISITES || r.category == CAT_CLUSTER {
                r.status = DiagnosticStatus::Passed;
            }
        }
        // Fail a core services test
        let core_idx = report
            .results
            .iter()
            .position(|r| r.category == CAT_CORE_SERVICES)
            .unwrap();
        report.results[core_idx].status = DiagnosticStatus::Failed("not running".to_string());

        // Deep verification tests should be skipped
        let deep_idx = report
            .results
            .iter()
            .position(|r| r.category == CAT_DEEP_VERIFICATION)
            .unwrap();
        assert_eq!(
            should_skip(&report, deep_idx),
            Some("core services not healthy")
        );
    }

    #[test]
    fn test_deep_cleanup_runs_after_setup_fail() {
        let mut report = DiagnosticsReport::new();
        // Pass everything before deep
        for r in report.results.iter_mut() {
            if r.category != CAT_DEEP_VERIFICATION {
                r.status = DiagnosticStatus::Passed;
            }
        }
        // Fail deep_setup
        let setup_idx = report
            .results
            .iter()
            .position(|r| r.id == "deep_setup")
            .unwrap();
        report.results[setup_idx].status =
            DiagnosticStatus::Failed("ns creation failed".to_string());

        // dns should be skipped
        let dns_idx = report
            .results
            .iter()
            .position(|r| r.id == "deep_dns")
            .unwrap();
        assert_eq!(should_skip(&report, dns_idx), Some("setup failed"));

        // cleanup should NOT be skipped (setup was attempted)
        let cleanup_idx = report
            .results
            .iter()
            .position(|r| r.id == "deep_cleanup")
            .unwrap();
        assert!(should_skip(&report, cleanup_idx).is_none());
    }

    #[test]
    fn test_deep_no_setup_tests_run_independently() {
        let mut report = DiagnosticsReport::new();
        // Pass everything before deep, but fail deep_setup
        for r in report.results.iter_mut() {
            if r.category != CAT_DEEP_VERIFICATION {
                r.status = DiagnosticStatus::Passed;
            }
        }
        let setup_idx = report
            .results
            .iter()
            .position(|r| r.id == "deep_setup")
            .unwrap();
        report.results[setup_idx].status =
            DiagnosticStatus::Failed("ns creation failed".to_string());

        // deep_host_http, deep_docker_in_container, deep_runtime_socket should NOT be skipped
        for id in &[
            "deep_host_http",
            "deep_docker_in_container",
            "deep_runtime_socket",
        ] {
            let idx = report.results.iter().position(|r| r.id == *id).unwrap();
            assert!(
                should_skip(&report, idx).is_none(),
                "{} should not be skipped when setup fails",
                id
            );
        }

        // deep_dns/connectivity/volume SHOULD be skipped
        for id in &["deep_dns", "deep_connectivity", "deep_volume"] {
            let idx = report.results.iter().position(|r| r.id == *id).unwrap();
            assert_eq!(
                should_skip(&report, idx),
                Some("setup failed"),
                "{} should be skipped when setup fails",
                id
            );
        }
    }

    #[test]
    fn test_deep_cleanup_skipped_if_no_setup() {
        let mut report = DiagnosticsReport::new();
        // Pass everything before deep
        for r in report.results.iter_mut() {
            if r.category != CAT_DEEP_VERIFICATION {
                r.status = DiagnosticStatus::Passed;
            }
        }
        // deep_setup is still Pending (not attempted)
        let cleanup_idx = report
            .results
            .iter()
            .position(|r| r.id == "deep_cleanup")
            .unwrap();
        assert_eq!(
            should_skip(&report, cleanup_idx),
            Some("setup not attempted")
        );
    }

    #[test]
    fn test_timeout_values() {
        assert_eq!(test_timeout("docker_accessible"), Duration::from_secs(10));
        assert_eq!(
            test_timeout("host_ports_reachable"),
            Duration::from_secs(15)
        );
        assert_eq!(test_timeout("no_crash_loops"), Duration::from_secs(15));
        assert_eq!(test_timeout("deep_setup"), Duration::from_secs(60));
        assert_eq!(test_timeout("deep_dns"), Duration::from_secs(90));
        assert_eq!(test_timeout("deep_connectivity"), Duration::from_secs(120));
        assert_eq!(test_timeout("deep_volume"), Duration::from_secs(120));
        assert_eq!(test_timeout("deep_host_http"), Duration::from_secs(30));
        assert_eq!(
            test_timeout("deep_docker_in_container"),
            Duration::from_secs(30)
        );
        assert_eq!(test_timeout("deep_runtime_socket"), Duration::from_secs(30));
        assert_eq!(test_timeout("deep_cleanup"), Duration::from_secs(30));
    }
}
