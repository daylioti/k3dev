//! Cluster diagnostics — health check runner
//!
//! Runs a series of diagnostic tests against the cluster and reports results
//! incrementally via AppMessage channel.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, Namespace, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PersistentVolumeClaimVolumeSource, Pod, PodSpec, Service, ServicePort, ServiceSpec, Volume,
    VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, DeleteParams, PostParams};

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
        "deep_setup" => 60,
        "deep_dns" => 90,
        "deep_connectivity" => 120,
        "deep_volume" => 120,
        "deep_cleanup" => 30,
        _ => 10,
    };
    Duration::from_secs(secs)
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
        // Networking
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

    // If any prerequisite failed, skip everything after
    let prereq_failed = report
        .results
        .iter()
        .filter(|r| r.category == CAT_PREREQUISITES)
        .any(|r| matches!(r.status, DiagnosticStatus::Failed(_)));
    if prereq_failed {
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
        if test_id != "deep_setup" {
            let setup = report.results.iter().find(|r| r.id == "deep_setup");
            if !matches!(setup, Some(r) if r.status == DiagnosticStatus::Passed) {
                return Some("setup failed");
            }
        }
    }

    None
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
                return Ok(Some("not applicable (non-Linux)".to_string()));
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
                let profiles = std::fs::read_to_string(
                    "/sys/kernel/security/apparmor/profiles",
                )
                .unwrap_or_default();

                let docker_profile_enforcing = profiles
                    .lines()
                    .any(|l| l.contains("docker-default") && l.contains("(enforce)"));

                if docker_profile_enforcing {
                    Ok(Some("active, docker-default enforcing (k3s uses unconfined)".to_string()))
                } else {
                    Ok(Some("active".to_string()))
                }
            }
        }
        "br_netfilter_loaded" => {
            // Only relevant on Linux
            #[cfg(not(target_os = "linux"))]
            {
                return Ok(Some("not applicable (non-Linux)".to_string()));
            }
            #[cfg(target_os = "linux")]
            {
                // Check if br_netfilter module is loaded
                let modules = std::fs::read_to_string("/proc/modules").unwrap_or_default();
                let br_loaded = modules.lines().any(|l| l.starts_with("br_netfilter "));

                if br_loaded {
                    // Also check sysctl value
                    let sysctl_val = std::fs::read_to_string(
                        "/proc/sys/net/bridge/bridge-nf-call-iptables",
                    )
                    .unwrap_or_default();
                    if sysctl_val.trim() == "1" {
                        Ok(Some("loaded, bridge-nf-call-iptables=1".to_string()))
                    } else {
                        Err("br_netfilter loaded but bridge-nf-call-iptables != 1".to_string())
                    }
                } else {
                    Err("br_netfilter not loaded (run: sudo modprobe br_netfilter)".to_string())
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
            let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
                .await
                .map_err(|e| format!("client init failed: {}", e))?;
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
        "coredns_running" => {
            let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
                .await
                .map_err(|e| e.to_string())?;
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
            let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
                .await
                .map_err(|e| e.to_string())?;
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
                let hosts: Vec<_> = missing.iter().take(3).cloned().collect();
                let suffix = if missing.len() > 3 {
                    format!(" (+{})", missing.len() - 3)
                } else {
                    String::new()
                };
                Err(format!("missing: {}{}", hosts.join(", "), suffix))
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
                let show: Vec<_> = unhealthy.iter().take(3).copied().collect();
                let suffix = if unhealthy.len() > 3 {
                    format!(" (+{})", unhealthy.len() - 3)
                } else {
                    String::new()
                };
                Err(format!("unhealthy: {}{}", show.join(", "), suffix))
            }
        }
        "no_stuck_pods" => {
            let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            let pending = k8s
                .list_pending_pods()
                .await
                .map_err(|e| format!("failed to list pods: {}", e))?;
            if pending.is_empty() {
                Ok(None)
            } else {
                let names: Vec<_> = pending.iter().take(3).map(|p| p.name.as_str()).collect();
                let suffix = if pending.len() > 3 {
                    format!(" (+{})", pending.len() - 3)
                } else {
                    String::new()
                };
                Err(format!("stuck: {}{}", names.join(", "), suffix))
            }
        }
        "no_pull_errors" => {
            let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
                .await
                .map_err(|e| e.to_string())?;
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
        // Deep Verification tests
        "deep_setup" => deep_setup(config).await,
        "deep_dns" => deep_dns(config).await,
        "deep_connectivity" => deep_connectivity(config).await,
        "deep_volume" => deep_volume(config).await,
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
    let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
        .await
        .map_err(|e| format!("client init failed: {}", e))?;
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
    let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
        .await
        .map_err(|e| format!("client init failed: {}", e))?;
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
    let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
        .await
        .map_err(|e| format!("client init failed: {}", e))?;
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
    let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
        .await
        .map_err(|e| format!("client init failed: {}", e))?;
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

/// Delete the k3dev-diag namespace (cascades to all resources)
async fn deep_cleanup(config: &ClusterConfig) -> Result<Option<String>, String> {
    let k8s = K8sClient::new(config.kubeconfig.as_deref(), config.context.as_deref())
        .await
        .map_err(|e| format!("client init failed: {}", e))?;
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
        assert_eq!(tests.len(), 20);
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
        assert_eq!(test_timeout("deep_setup"), Duration::from_secs(60));
        assert_eq!(test_timeout("deep_dns"), Duration::from_secs(90));
        assert_eq!(test_timeout("deep_connectivity"), Duration::from_secs(120));
        assert_eq!(test_timeout("deep_volume"), Duration::from_secs(120));
        assert_eq!(test_timeout("deep_cleanup"), Duration::from_secs(30));
    }
}
