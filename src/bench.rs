//! Automatic functional + performance benchmark of the application itself.
//!
//! This measures the *real* code paths the TUI uses on startup and attributes
//! wall-clock time to each phase, so a single command answers questions like
//! "why does k3dev show pods only ~2s after I open it on an already-running
//! cluster?".
//!
//! The headline metric is `time_to_pods_e2e`: it drives the *real* `App`
//! startup loop headlessly (status check → message handling →
//! `RefreshScheduler`) and reports how the total breaks down
//! (status check → pod fetch).
//!
//! Trigger: `k3dev bench` (add `--json` for machine-readable output), or the
//! `tests/perf.rs` integration test, or `test/perf.sh`.

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::cluster::{ClusterConfig, ClusterManager, ClusterStatus, DockerManager};
use crate::config::{Config, ConfigLoader, ConfigValidator};
use crate::k8s::K8sClient;

/// Outcome of a single benchmark phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseStatus {
    /// Completed within its time budget and functionally succeeded.
    Pass,
    /// Functionally failed, or exceeded its time budget.
    Fail,
    /// Not applicable in the current environment (e.g. cluster not running).
    Skip,
}

/// One measured phase of application startup.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseResult {
    /// Stable phase identifier (also used for the budget env var).
    pub name: &'static str,
    /// Measured wall-clock duration in milliseconds.
    pub millis: f64,
    /// Budget the phase is compared against, in milliseconds.
    pub budget_ms: u64,
    pub status: PhaseStatus,
    /// Human-readable detail (functional result, breakdown, error...).
    pub detail: String,
}

/// Full benchmark report.
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    /// Whether the target cluster was running during the benchmark.
    pub cluster_running: bool,
    pub phases: Vec<PhaseResult>,
    /// True iff no phase has `Fail` status.
    pub passed: bool,
}

/// Read a per-phase budget (ms) from `K3DEV_BENCH_<NAME>_MS`, else the default.
fn budget_ms(name: &str, default: u64) -> u64 {
    let env_key = format!("K3DEV_BENCH_{}_MS", name.to_uppercase());
    std::env::var(env_key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

/// Classify a measured phase against its budget.
fn classify(millis: f64, budget_ms: u64, functional_ok: bool) -> PhaseStatus {
    if !functional_ok || millis > budget_ms as f64 {
        PhaseStatus::Fail
    } else {
        PhaseStatus::Pass
    }
}

fn skipped(name: &'static str, budget_ms: u64, reason: impl Into<String>) -> PhaseResult {
    PhaseResult {
        name,
        millis: 0.0,
        budget_ms,
        status: PhaseStatus::Skip,
        detail: reason.into(),
    }
}

/// Build the shared `ClusterConfig` exactly like the CLI / TUI entry points do.
fn build_cluster_config(config: &Config) -> Arc<ClusterConfig> {
    let kubeconfig =
        (!config.cluster.kubeconfig.is_empty()).then(|| config.cluster.kubeconfig.clone());
    let context = (!config.cluster.context.is_empty()).then(|| config.cluster.context.clone());
    Arc::new(
        ClusterConfig::from(config.infrastructure.clone())
            .with_hooks(config.hooks.clone())
            .with_k8s_config(kubeconfig, context),
    )
}

/// Run the full benchmark. Never panics; phases that can't run in the current
/// environment are reported as `Skip` rather than `Fail`.
pub async fn run_bench(config_path: Option<&str>) -> BenchReport {
    let mut phases: Vec<PhaseResult> = Vec::new();

    // ---- Phase: config_load ------------------------------------------------
    let b_config = budget_ms("config_load", 100);
    let t = Instant::now();
    let config = ConfigLoader::new(config_path).load().unwrap_or_default();
    let validation = ConfigValidator::new(&config).validate();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    phases.push(PhaseResult {
        name: "config_load",
        millis: ms,
        budget_ms: b_config,
        status: classify(ms, b_config, true),
        detail: format!("{} config warning(s)", validation.warnings.len()),
    });

    // ---- Phase: app_init ---------------------------------------------------
    // Real, full App construction — identical to launching the TUI.
    let b_app = budget_ms("app_init", 400);
    let t = Instant::now();
    let app_ok = crate::app::App::new(config_path).await.is_ok();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    phases.push(PhaseResult {
        name: "app_init",
        millis: ms,
        budget_ms: b_app,
        status: classify(ms, b_app, app_ok),
        detail: if app_ok {
            "App::new() ok".into()
        } else {
            "App::new() failed".into()
        },
    });

    let cluster_config = build_cluster_config(&config);

    // ---- Phase: status_check ----------------------------------------------
    // What `spawn_status_check` does and what gates the first useful render.
    let b_status = budget_ms("status_check", 2000);
    let t = Instant::now();
    let status = match ClusterManager::new(Arc::clone(&cluster_config)).await {
        Ok(m) => m.get_status().await,
        Err(_) => ClusterStatus::Unknown,
    };
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    phases.push(PhaseResult {
        name: "status_check",
        millis: ms,
        budget_ms: b_status,
        status: classify(ms, b_status, true),
        detail: format!("cluster status: {:?}", status),
    });

    let cluster_running = matches!(status, ClusterStatus::Running);

    // Budgets for cluster-dependent phases (declared up-front so we can emit
    // Skip entries with the right budget when the cluster is down).
    let b_k8s = budget_ms("k8s_client_init", 2000);
    let b_list = budget_ms("list_pods", 1500);
    let b_docker = budget_ms("docker_pod_stats", 2000);
    let b_e2e = budget_ms("time_to_pods_e2e", 2500);

    if !cluster_running {
        let reason = format!("cluster not running ({:?})", status);
        phases.push(skipped("k8s_client_init", b_k8s, reason.clone()));
        phases.push(skipped("list_pods", b_list, reason.clone()));
        phases.push(skipped("docker_pod_stats", b_docker, reason.clone()));
        phases.push(skipped("time_to_pods_e2e", b_e2e, reason));
        let passed = !phases.iter().any(|p| p.status == PhaseStatus::Fail);
        return BenchReport {
            cluster_running,
            phases,
            passed,
        };
    }

    // ---- Phase: k8s_client_init -------------------------------------------
    // Lazy kube-rs init cost (kubeconfig parse + TLS handshake).
    let t = Instant::now();
    let k8s = K8sClient::new(
        cluster_config.kubeconfig.as_deref(),
        cluster_config.context.as_deref(),
    )
    .await;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let k8s_ok = k8s.is_ok();
    phases.push(PhaseResult {
        name: "k8s_client_init",
        millis: ms,
        budget_ms: b_k8s,
        status: classify(ms, b_k8s, k8s_ok),
        detail: if k8s_ok {
            "K8sClient::new() ok".into()
        } else {
            "K8sClient::new() failed".into()
        },
    });

    // ---- Phase: list_pods --------------------------------------------------
    // The pending-pods query the TUI's pod panel relies on.
    if let Ok(client) = &k8s {
        let t = Instant::now();
        let res = client.list_pending_pods().await;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let (ok, detail) = match &res {
            Ok(p) => (true, format!("{} pending pod(s)", p.len())),
            Err(e) => (false, format!("list_pending_pods failed: {}", e)),
        };
        phases.push(PhaseResult {
            name: "list_pods",
            millis: ms,
            budget_ms: b_list,
            status: classify(ms, b_list, ok),
            detail,
        });
    } else {
        phases.push(skipped("list_pods", b_list, "k8s client unavailable"));
    }

    // ---- Phase: docker_pod_stats ------------------------------------------
    // The running-pod stats path (agent binary, cgroup fallback).
    let t = Instant::now();
    let docker_res = async {
        let docker = DockerManager::from_default_socket()?;
        match docker
            .get_pod_stats_via_agent(&cluster_config.container_name)
            .await
        {
            Ok(s) => Ok(s),
            Err(_) => docker.get_pod_stats(&cluster_config.container_name).await,
        }
    }
    .await;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let (ok, detail) = match &docker_res {
        Ok(s) => (true, format!("{} running container(s)", s.len())),
        Err(e) => (false, format!("docker pod stats failed: {}", e)),
    };
    phases.push(PhaseResult {
        name: "docker_pod_stats",
        millis: ms,
        budget_ms: b_docker,
        status: classify(ms, b_docker, ok),
        detail,
    });

    // ---- Phase: time_to_pods_e2e ------------------------------------------
    // Drive the *real* App startup loop (status check → message handling →
    // RefreshScheduler) headlessly, so this number tracks the actual code
    // path rather than a hand-coded model. Pods are now fetched eagerly when
    // the cluster status flips to Running, so the old ~2s StatsRefresh gate
    // no longer applies.
    let (e2e_ms, detail) = match crate::app::App::new(config_path).await {
        Ok(mut app) => {
            let t = app.bench_time_to_pods().await;
            (
                t.total_ms,
                format!(
                    "status={:.0}ms + pod_fetch={:.0}ms → {} running / {} pending",
                    t.status_ms, t.pods_ms, t.running, t.pending
                ),
            )
        }
        Err(e) => (0.0, format!("App::new() failed: {}", e)),
    };
    phases.push(PhaseResult {
        name: "time_to_pods_e2e",
        millis: e2e_ms,
        budget_ms: b_e2e,
        status: classify(e2e_ms, b_e2e, true),
        detail,
    });

    let passed = !phases.iter().any(|p| p.status == PhaseStatus::Fail);
    BenchReport {
        cluster_running,
        phases,
        passed,
    }
}

impl BenchReport {
    /// Serialize to pretty JSON (used by `--json`).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Print a colored, human-readable report to stdout.
    pub fn print(&self) {
        println!("\x1b[1mk3dev startup benchmark\x1b[0m\n");
        println!(
            "  \x1b[1m{:<20} {:>9}  {:>9}  {:<6} DETAIL\x1b[0m",
            "PHASE", "TIME", "BUDGET", "RESULT"
        );
        println!("  {}", "-".repeat(86));
        for p in &self.phases {
            let (mark, color) = match p.status {
                PhaseStatus::Pass => ("PASS", "\x1b[32m"),
                PhaseStatus::Fail => ("FAIL", "\x1b[31m"),
                PhaseStatus::Skip => ("SKIP", "\x1b[90m"),
            };
            let time_str = if p.status == PhaseStatus::Skip {
                "    -".to_string()
            } else {
                format!("{:.0}ms", p.millis)
            };
            println!(
                "  {:<20} {:>9}  {:>9}  {}{:<6}\x1b[0m \x1b[90m{}\x1b[0m",
                p.name,
                time_str,
                format!("{}ms", p.budget_ms),
                color,
                mark,
                p.detail,
            );
        }
        println!();

        if !self.cluster_running {
            println!("\x1b[33m⚠ Cluster not running — cluster-dependent phases skipped.\x1b[0m");
            println!("\x1b[90m  Start it with `k3dev start`, then re-run `k3dev bench`.\x1b[0m");
        }

        // Surface the headline number; pods are fetched eagerly on the
        // status→Running transition, so a high value here means slow I/O.
        if let Some(e2e) = self
            .phases
            .iter()
            .find(|p| p.name == "time_to_pods_e2e" && p.status != PhaseStatus::Skip)
        {
            println!(
                "\x1b[1m  Time until pods are visible after opening k3dev: {:.0}ms\x1b[0m",
                e2e.millis
            );
            if e2e.millis >= 1500.0 {
                println!(
                    "\x1b[33m  Pods are fetched eagerly on the status→Running transition, so\x1b[0m"
                );
                println!(
                    "\x1b[90m  this is real Docker/K8s I/O latency, not RefreshScheduler gating\x1b[0m"
                );
                println!("\x1b[90m  (src/app/messages.rs spawns the pod checks eagerly).\x1b[0m");
            }
        }

        println!();
        if self.passed {
            println!("\x1b[32m✓ All measured phases within budget\x1b[0m");
        } else {
            let failed: Vec<&str> = self
                .phases
                .iter()
                .filter(|p| p.status == PhaseStatus::Fail)
                .map(|p| p.name)
                .collect();
            println!(
                "\x1b[31m✗ Over budget / failed: {}\x1b[0m",
                failed.join(", ")
            );
        }
    }
}
