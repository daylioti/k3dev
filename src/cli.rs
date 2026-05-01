//! CLI command execution (headless, no TUI)
//!
//! Runs cluster actions and pod operations directly in the terminal,
//! printing colored output instead of rendering the TUI.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::cluster::{ClusterConfig, ClusterManager, IngressManager};
use crate::config::{ConfigLoader, RefreshConfig};
use crate::k8s::K8sClient;
use crate::ui::components::{ClusterAction, OutputLine, OutputType};

/// Load config and build a ClusterConfig arc
fn load_cluster_config(config_path: Option<&str>) -> (crate::config::Config, Arc<ClusterConfig>) {
    let loader = ConfigLoader::new(config_path);
    let config = loader.load().unwrap_or_default();

    let kubeconfig = if config.cluster.kubeconfig.is_empty() {
        None
    } else {
        Some(config.cluster.kubeconfig.clone())
    };
    let context = if config.cluster.context.is_empty() {
        None
    } else {
        Some(config.cluster.context.clone())
    };
    let cluster_config = Arc::new(
        ClusterConfig::from(config.infrastructure.clone())
            .with_hooks(config.hooks.clone())
            .with_k8s_config(kubeconfig, context),
    );

    (config, cluster_config)
}

/// Create a K8sClient from config
async fn create_k8s_client(config_path: Option<&str>) -> Result<K8sClient> {
    let loader = ConfigLoader::new(config_path);
    let config = loader.load().unwrap_or_default();

    let kubeconfig = if config.cluster.kubeconfig.is_empty() {
        None
    } else {
        Some(config.cluster.kubeconfig.as_str())
    };
    let context = if config.cluster.context.is_empty() {
        None
    } else {
        Some(config.cluster.context.as_str())
    };

    // Need to own the strings for the async call
    let kc = kubeconfig.map(String::from);
    let ctx = context.map(String::from);

    K8sClient::new(kc.as_deref(), ctx.as_deref()).await
}

/// Run a cluster action headlessly, printing output to stdout.
/// Returns the process exit code (0 = success, 1 = failure).
pub async fn run_cli_action(action: ClusterAction, config_path: Option<&str>) -> Result<i32> {
    let (config, cluster_config) = load_cluster_config(config_path);

    let _ = crate::logging::init_logging(&config.logging, &config.infrastructure.cluster_name);

    let refresh_config = RefreshConfig::default();
    let timeout = refresh_config.cluster_operation_timeout;

    let (output_tx, mut output_rx) = mpsc::channel::<OutputLine>(100);

    // Spawn the cluster action
    let action_handle = tokio::spawn(async move {
        let mut manager = ClusterManager::new(cluster_config).await?;

        match action {
            ClusterAction::Start => manager.start(output_tx).await,
            ClusterAction::Stop => manager.stop(output_tx).await,
            ClusterAction::Restart => manager.restart(output_tx).await,
            ClusterAction::Destroy => manager.delete(output_tx).await,
            ClusterAction::Info => manager.info(output_tx).await,
            ClusterAction::DeleteSnapshots => manager.delete_snapshots(output_tx).await,
            ClusterAction::Diagnostics | ClusterAction::PreflightCheck => unreachable!(),
        }
    });

    // Print output lines as they arrive
    let printer = tokio::spawn(async move {
        while let Some(line) = output_rx.recv().await {
            print_output_line(&line);
        }
    });

    // Wait for action with timeout
    let result = tokio::time::timeout(timeout, action_handle).await;

    // Wait for remaining output to flush
    let _ = printer.await;

    match result {
        Ok(Ok(Ok(()))) => {
            print_output_line(&OutputLine::success("Done."));
            Ok(0)
        }
        Ok(Ok(Err(e))) => {
            print_output_line(&OutputLine::error(format!("Error: {}", e)));
            Ok(1)
        }
        Ok(Err(e)) => {
            print_output_line(&OutputLine::error(format!("Task panicked: {}", e)));
            Ok(1)
        }
        Err(_) => {
            print_output_line(&OutputLine::error("Operation timed out"));
            Ok(1)
        }
    }
}

/// Run cluster diagnostics headlessly
pub async fn run_cli_diagnostics(config_path: Option<&str>) -> Result<i32> {
    use crate::app::AppMessage;
    use crate::cluster::diagnostics::{run_all_diagnostics, DiagnosticStatus};

    let (config, cluster_config) = load_cluster_config(config_path);
    let _ = crate::logging::init_logging(&config.logging, &config.infrastructure.cluster_name);

    let (tx, mut rx) = mpsc::channel::<AppMessage>(100);

    let diag_handle = tokio::spawn(async move {
        run_all_diagnostics(cluster_config, tx).await;
    });

    // Track latest report to print final summary
    let mut last_category = String::new();
    let mut final_report = None;
    // Track how many results have been fully printed (completed/skipped)
    let mut printed_count: usize = 0;
    // Track whether we have a running spinner line that needs overwriting
    let mut has_running_line = false;

    println!("\x1b[1mRunning cluster diagnostics...\x1b[0m\n");

    while let Some(msg) = rx.recv().await {
        if let AppMessage::DiagnosticsUpdated(report) = msg {
            // Only process results from where we left off
            for (i, result) in report.results.iter().enumerate() {
                if i < printed_count {
                    continue; // Already printed this result's final state
                }
                match &result.status {
                    DiagnosticStatus::Running => {
                        if result.category != last_category {
                            last_category = result.category.to_string();
                            println!("\x1b[1;36m── {} ──\x1b[0m", last_category);
                        }
                        print!("  \x1b[33m⟳\x1b[0m {}...", result.name);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                        has_running_line = true;
                    }
                    DiagnosticStatus::Passed => {
                        let duration = result
                            .duration
                            .map(|d| format!(" \x1b[90m({:.1}s)\x1b[0m", d.as_secs_f64()))
                            .unwrap_or_default();
                        if has_running_line {
                            print!("\r");
                            has_running_line = false;
                        }
                        println!("  \x1b[32m✓\x1b[0m {}{}", result.name, duration);
                        printed_count = i + 1;
                    }
                    DiagnosticStatus::Failed(reason) => {
                        let duration = result
                            .duration
                            .map(|d| format!(" \x1b[90m({:.1}s)\x1b[0m", d.as_secs_f64()))
                            .unwrap_or_default();
                        if has_running_line {
                            print!("\r");
                            has_running_line = false;
                        }
                        println!(
                            "  \x1b[31m✗\x1b[0m {}{}\n    \x1b[31m{}\x1b[0m",
                            result.name, duration, reason
                        );
                        printed_count = i + 1;
                    }
                    DiagnosticStatus::Skipped(reason) => {
                        if result.category != last_category {
                            last_category = result.category.to_string();
                            println!("\x1b[1;36m── {} ──\x1b[0m", last_category);
                        }
                        println!("  \x1b[90m⊘ {} (skipped: {})\x1b[0m", result.name, reason);
                        printed_count = i + 1;
                    }
                    DiagnosticStatus::Pending => {}
                }
            }

            if report.finished {
                final_report = Some(report);
            }
        }
    }

    let _ = diag_handle.await;

    // Print summary
    if let Some(report) = final_report {
        let passed = report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Passed))
            .count();
        let failed = report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Failed(_)))
            .count();
        let skipped = report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Skipped(_)))
            .count();
        let total = report.results.len();

        println!();
        if failed == 0 {
            println!(
                "\x1b[32m✓ All tests passed ({}/{}, {} skipped)\x1b[0m",
                passed, total, skipped
            );
            Ok(0)
        } else {
            println!(
                "\x1b[31m✗ {}/{} passed, {} failed, {} skipped\x1b[0m",
                passed, total, failed, skipped
            );
            Ok(1)
        }
    } else {
        eprintln!("Diagnostics did not complete");
        Ok(1)
    }
}

/// Run preflight checks headlessly
pub async fn run_cli_preflight(config_path: Option<&str>) -> Result<i32> {
    use crate::app::AppMessage;
    use crate::cluster::diagnostics::{run_preflight_checks, DiagnosticStatus};

    let (config, cluster_config) = load_cluster_config(config_path);
    let _ = crate::logging::init_logging(&config.logging, &config.infrastructure.cluster_name);

    let (tx, mut rx) = mpsc::channel::<AppMessage>(100);

    let handle = tokio::spawn(async move {
        run_preflight_checks(cluster_config, tx).await;
    });

    let mut last_category = String::new();
    let mut final_report = None;
    let mut printed_count: usize = 0;
    let mut has_running_line = false;

    println!("\x1b[1mRunning preflight checks...\x1b[0m\n");

    while let Some(msg) = rx.recv().await {
        if let AppMessage::DiagnosticsUpdated(report) = msg {
            for (i, result) in report.results.iter().enumerate() {
                if i < printed_count {
                    continue;
                }
                match &result.status {
                    DiagnosticStatus::Running => {
                        if result.category != last_category {
                            last_category = result.category.to_string();
                            println!("\x1b[1;36m── {} ──\x1b[0m", last_category);
                        }
                        print!("  \x1b[33m⟳\x1b[0m {}...", result.name);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                        has_running_line = true;
                    }
                    DiagnosticStatus::Passed => {
                        let duration = result
                            .duration
                            .map(|d| format!(" \x1b[90m({:.1}s)\x1b[0m", d.as_secs_f64()))
                            .unwrap_or_default();
                        if has_running_line {
                            print!("\r");
                            has_running_line = false;
                        }
                        println!("  \x1b[32m✓\x1b[0m {}{}", result.name, duration);
                        printed_count = i + 1;
                    }
                    DiagnosticStatus::Failed(reason) => {
                        let duration = result
                            .duration
                            .map(|d| format!(" \x1b[90m({:.1}s)\x1b[0m", d.as_secs_f64()))
                            .unwrap_or_default();
                        if has_running_line {
                            print!("\r");
                            has_running_line = false;
                        }
                        println!(
                            "  \x1b[31m✗\x1b[0m {}{}\n    \x1b[31m{}\x1b[0m",
                            result.name, duration, reason
                        );
                        printed_count = i + 1;
                    }
                    DiagnosticStatus::Skipped(reason) => {
                        if result.category != last_category {
                            last_category = result.category.to_string();
                            println!("\x1b[1;36m── {} ──\x1b[0m", last_category);
                        }
                        println!("  \x1b[90m⊘ {} (skipped: {})\x1b[0m", result.name, reason);
                        printed_count = i + 1;
                    }
                    DiagnosticStatus::Pending => {}
                }
            }

            if report.finished {
                final_report = Some(report);
            }
        }
    }

    let _ = handle.await;

    if let Some(report) = final_report {
        let passed = report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Passed))
            .count();
        let failed = report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Failed(_)))
            .count();
        let skipped = report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Skipped(_)))
            .count();
        let total = report.results.len();

        println!();
        if failed == 0 {
            println!(
                "\x1b[32m✓ Ready to start ({}/{} passed, {} skipped)\x1b[0m",
                passed, total, skipped
            );
            Ok(0)
        } else {
            println!(
                "\x1b[31m✗ {}/{} passed, {} failed, {} skipped\x1b[0m",
                passed, total, failed, skipped
            );
            Ok(1)
        }
    } else {
        eprintln!("Preflight checks did not complete");
        Ok(1)
    }
}

/// Update /etc/hosts with ingress entries
pub async fn run_cli_update_hosts(config_path: Option<&str>) -> Result<i32> {
    use crate::cluster::HostsUpdateResult;

    let (config, _cluster_config) = load_cluster_config(config_path);
    let _ = crate::logging::init_logging(&config.logging, &config.infrastructure.cluster_name);

    let domain = config.infrastructure.domain.clone();

    let (output_tx, mut output_rx) = mpsc::channel::<OutputLine>(100);

    let update_handle = tokio::spawn(async move {
        let mut ingress_manager = IngressManager::with_domain(domain);
        ingress_manager.update_hosts(Some(output_tx)).await
    });

    // Print output as it arrives
    let printer = tokio::spawn(async move {
        while let Some(line) = output_rx.recv().await {
            print_output_line(&line);
        }
    });

    let result = update_handle.await??;
    let _ = printer.await;

    match result {
        HostsUpdateResult::NoUpdateNeeded => {
            println!("\x1b[32m✓ /etc/hosts is already up to date\x1b[0m");
            Ok(0)
        }
        HostsUpdateResult::WrittenDirectly { count } => {
            println!("\x1b[32m✓ Updated /etc/hosts with {} entries\x1b[0m", count);
            Ok(0)
        }
        HostsUpdateResult::NeedsSudo { content, count } => {
            println!(
                "\x1b[33m⚠ Need elevated privileges to write {} entries to /etc/hosts\x1b[0m",
                count
            );
            println!("Run with sudo or manually add the following:");
            println!();
            // Extract just the k3dev entries from content
            for line in content.lines() {
                if line.contains("# k3dev-ingress") {
                    println!("  {}", line);
                }
            }
            Ok(1)
        }
        HostsUpdateResult::ReadOnly { entries } => {
            println!("\x1b[33m⚠ /etc/hosts is read-only. Add these entries manually:\x1b[0m");
            println!();
            for entry in &entries {
                println!("  {}", entry);
            }
            Ok(1)
        }
    }
}

/// List pods with status
pub async fn run_cli_pods(config_path: Option<&str>, namespace: Option<&str>) -> Result<i32> {
    let k8s_client = match create_k8s_client(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mFailed to connect to cluster: {}\x1b[0m", e);
            return Ok(1);
        }
    };

    let namespaces = if let Some(ns) = namespace {
        vec![ns.to_string()]
    } else {
        match k8s_client.list_namespaces().await {
            Ok(ns) => ns,
            Err(e) => {
                eprintln!("\x1b[31mFailed to list namespaces: {}\x1b[0m", e);
                return Ok(1);
            }
        }
    };

    // Print header
    println!(
        "\x1b[1m{:<40} {:<16} {:<10} {:<8} {:<16}\x1b[0m",
        "NAME", "NAMESPACE", "STATUS", "READY", "IP"
    );
    println!("{}", "-".repeat(90));

    let mut total = 0;
    for ns in &namespaces {
        match k8s_client.list_pods(ns, None).await {
            Ok(pods) => {
                for pod in &pods {
                    total += 1;
                    let status_color = match pod.status.as_str() {
                        "Running" => "\x1b[32m",
                        "Succeeded" => "\x1b[36m",
                        "Pending" | "ContainerCreating" => "\x1b[33m",
                        _ => "\x1b[31m",
                    };
                    let ready_str = if pod.ready { "Yes" } else { "No" };
                    let ready_color = if pod.ready { "\x1b[32m" } else { "\x1b[33m" };

                    println!(
                        "{:<40} {:<16} {}{:<10}\x1b[0m {}{:<8}\x1b[0m {:<16}",
                        pod.name,
                        pod.namespace,
                        status_color,
                        pod.status,
                        ready_color,
                        ready_str,
                        pod.ip.as_deref().unwrap_or("-"),
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "\x1b[33mWarning: could not list pods in {}: {}\x1b[0m",
                    ns, e
                );
            }
        }
    }

    println!("\n\x1b[90m{} pods total\x1b[0m", total);
    Ok(0)
}

/// View pod logs
pub async fn run_cli_logs(
    config_path: Option<&str>,
    pod: &str,
    namespace: &str,
    container: Option<&str>,
    tail: i64,
    follow: bool,
) -> Result<i32> {
    let k8s_client = match create_k8s_client(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mFailed to connect to cluster: {}\x1b[0m", e);
            return Ok(1);
        }
    };

    if follow {
        // For follow mode, use kubectl directly since it handles streaming well
        let mut cmd = std::process::Command::new("kubectl");
        cmd.args(["logs", "-f", pod, "-n", namespace]);
        if let Some(c) = container {
            cmd.args(["-c", c]);
        }
        cmd.arg(format!("--tail={}", tail));

        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("Failed to run kubectl: {}", e))?;

        Ok(status.code().unwrap_or(1))
    } else {
        match k8s_client
            .get_pod_logs(namespace, pod, container, Some(tail))
            .await
        {
            Ok(logs) => {
                print!("{}", logs);
                Ok(0)
            }
            Err(e) => {
                eprintln!("\x1b[31mFailed to get logs: {}\x1b[0m", e);
                Ok(1)
            }
        }
    }
}

/// Describe a pod
pub async fn run_cli_describe(
    config_path: Option<&str>,
    pod: &str,
    namespace: &str,
) -> Result<i32> {
    let k8s_client = match create_k8s_client(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mFailed to connect to cluster: {}\x1b[0m", e);
            return Ok(1);
        }
    };

    match k8s_client.describe_pod(namespace, pod).await {
        Ok(description) => {
            print!("{}", description);
            Ok(0)
        }
        Err(e) => {
            eprintln!("\x1b[31mFailed to describe pod: {}\x1b[0m", e);
            Ok(1)
        }
    }
}

/// Delete a pod
pub async fn run_cli_delete_pod(
    config_path: Option<&str>,
    pod: &str,
    namespace: &str,
) -> Result<i32> {
    let k8s_client = match create_k8s_client(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mFailed to connect to cluster: {}\x1b[0m", e);
            return Ok(1);
        }
    };

    println!(
        "Deleting pod \x1b[1m{}\x1b[0m in namespace \x1b[1m{}\x1b[0m...",
        pod, namespace
    );

    match k8s_client.delete_pod(namespace, pod).await {
        Ok(_) => {
            println!("\x1b[32m✓ Pod {} deleted\x1b[0m", pod);
            Ok(0)
        }
        Err(e) => {
            eprintln!("\x1b[31m✗ Failed to delete pod: {}\x1b[0m", e);
            Ok(1)
        }
    }
}

/// Restart a pod (delete and let deployment recreate)
pub async fn run_cli_restart_pod(
    config_path: Option<&str>,
    pod: &str,
    namespace: &str,
) -> Result<i32> {
    let k8s_client = match create_k8s_client(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mFailed to connect to cluster: {}\x1b[0m", e);
            return Ok(1);
        }
    };

    println!(
        "Restarting pod \x1b[1m{}\x1b[0m in namespace \x1b[1m{}\x1b[0m (delete and let deployment recreate)...",
        pod, namespace
    );

    match k8s_client.delete_pod(namespace, pod).await {
        Ok(_) => {
            println!(
                "\x1b[32m✓ Pod {} deleted — deployment will recreate it\x1b[0m",
                pod
            );
            Ok(0)
        }
        Err(e) => {
            eprintln!("\x1b[31m✗ Failed to restart pod: {}\x1b[0m", e);
            Ok(1)
        }
    }
}

/// Execute a shell in a pod (interactive, uses kubectl exec)
pub async fn run_cli_exec(
    config_path: Option<&str>,
    pod: &str,
    namespace: &str,
    container: Option<&str>,
    cmd: &str,
) -> Result<i32> {
    // Verify the pod exists first via k8s client
    let k8s_client = match create_k8s_client(config_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mFailed to connect to cluster: {}\x1b[0m", e);
            return Ok(1);
        }
    };

    match k8s_client.get_pod(namespace, pod).await {
        Ok(pod_info) => {
            if pod_info.status != "Running" {
                eprintln!(
                    "\x1b[33m⚠ Pod {} is in {} state\x1b[0m",
                    pod, pod_info.status
                );
            }
        }
        Err(e) => {
            eprintln!("\x1b[31mPod not found: {}\x1b[0m", e);
            return Ok(1);
        }
    }

    // Use kubectl exec for interactive TTY support
    let mut kubectl = std::process::Command::new("kubectl");
    kubectl.args(["exec", "-it", pod, "-n", namespace]);
    if let Some(c) = container {
        kubectl.args(["-c", c]);
    }
    kubectl.args(["--", cmd]);

    let status = kubectl
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to run kubectl: {}", e))?;

    Ok(status.code().unwrap_or(1))
}

/// Run a tcpdump capture headlessly and write a .pcap file.
///
/// Returns the process exit code. Streams tcpdump status to stderr; the
/// final output path is printed on stdout so it can be piped.
#[allow(clippy::too_many_arguments)]
pub async fn run_cli_capture(
    config_path: Option<&str>,
    pod: Option<&str>,
    namespace: &str,
    container: Option<&str>,
    iface: String,
    filter: Option<&str>,
    duration: Option<&str>,
    max_bytes: Option<&str>,
    out: Option<&str>,
    out_dir: Option<&str>,
    image: Option<&str>,
    open: bool,
) -> Result<i32> {
    use crate::app::AppMessage;
    use crate::capture;

    let (config, _cluster_config) = load_cluster_config(config_path);
    let _ = crate::logging::init_logging(&config.logging, &config.infrastructure.cluster_name);

    // Resolve target.
    let target = if let Some(p) = pod {
        capture::CaptureTarget::Pod {
            pod: p.to_string(),
            namespace: namespace.to_string(),
        }
    } else if let Some(c) = container {
        capture::CaptureTarget::Container(c.to_string())
    } else {
        // clap's ArgGroup should prevent this, but guard for misuse.
        eprintln!("\x1b[31mEither --pod or --container is required\x1b[0m");
        return Ok(2);
    };

    // Resolve output path.
    let dir: std::path::PathBuf = out_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config.capture.output_dir.clone());
    let output_path: std::path::PathBuf = if let Some(o) = out {
        std::path::PathBuf::from(o)
    } else {
        capture::default_output_path(&dir, &target)
    };

    // Optional duration/max_bytes parsing.
    let duration_parsed = match duration.and_then(capture::parse_duration) {
        Some(d) => Some(d),
        None if duration.is_some() => {
            eprintln!("\x1b[31mInvalid --duration value (try '30s', '5m', '1h')\x1b[0m");
            return Ok(2);
        }
        None => None,
    };
    let max_bytes_parsed = match max_bytes.and_then(capture::parse_bytes) {
        Some(b) => Some(b),
        None if max_bytes.is_some() => {
            eprintln!("\x1b[31mInvalid --max-bytes value (try '100M', '1G')\x1b[0m");
            return Ok(2);
        }
        None => None,
    };

    let spec = capture::CaptureSpec {
        target,
        output_path: output_path.clone(),
        image: image
            .map(String::from)
            .unwrap_or_else(|| config.capture.image.clone()),
        iface,
        filter: filter.map(String::from),
        duration: duration_parsed,
        max_bytes: max_bytes_parsed,
    };

    // Connect to Docker.
    let docker = match crate::cluster::DockerManager::from_default_socket() {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("\x1b[31mFailed to connect to Docker: {}\x1b[0m", e);
            return Ok(1);
        }
    };

    // Drive the capture, listening for events on a local channel.
    let (msg_tx, mut msg_rx) = mpsc::channel::<AppMessage>(100);
    let cancel = tokio_util::sync::CancellationToken::new();

    if let Err(e) = capture::start_capture(spec, docker, msg_tx, cancel.clone()).await {
        eprintln!("\x1b[31mFailed to start capture: {}\x1b[0m", e);
        return Ok(1);
    }

    eprintln!(
        "\x1b[1mCapturing → {}\x1b[0m  (Ctrl-C to stop)",
        output_path.display()
    );

    let cancel_for_signal = cancel.clone();
    let signal_task = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_for_signal.cancel();
    });

    let mut final_path: Option<std::path::PathBuf> = None;
    let mut final_bytes: u64 = 0;
    let mut final_packets: Option<u64> = None;
    let mut failure: Option<String> = None;

    while let Some(msg) = msg_rx.recv().await {
        match msg {
            AppMessage::CaptureStatus(line) => {
                eprintln!("\x1b[90m{}\x1b[0m", line);
            }
            AppMessage::CaptureProgress { bytes, packets } => {
                let pkt = packets
                    .map(|p| format!("{} packets", p))
                    .unwrap_or_else(|| "packets: --".to_string());
                eprintln!("  \x1b[36m{} bytes  {}\x1b[0m", bytes, pkt);
            }
            AppMessage::CaptureComplete {
                path,
                bytes,
                packets,
            } => {
                final_path = Some(path);
                final_bytes = bytes;
                final_packets = packets;
                break;
            }
            AppMessage::CaptureFailed(err) => {
                failure = Some(err);
                break;
            }
            _ => {}
        }
    }

    // Cancel the signal listener so it doesn't keep the runtime alive.
    signal_task.abort();

    if let Some(err) = failure {
        eprintln!("\x1b[31mCapture failed: {}\x1b[0m", err);
        return Ok(1);
    }

    let path = final_path.unwrap_or(output_path);
    eprintln!(
        "\x1b[32m✓ Capture saved\x1b[0m  {} ({} bytes{})",
        path.display(),
        final_bytes,
        final_packets
            .map(|p| format!(", {} packets", p))
            .unwrap_or_default()
    );
    println!("{}", path.display());

    if open {
        let tool = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        if let Err(e) = std::process::Command::new(tool).arg(&path).spawn() {
            eprintln!("\x1b[33mWarning: failed to launch {}: {}\x1b[0m", tool, e);
        }
    }

    Ok(0)
}

/// Print an OutputLine to stdout with ANSI colors
fn print_output_line(line: &OutputLine) {
    let timestamp = line.timestamp.format("[%H:%M:%S]");
    let (color_code, reset) = match line.output_type {
        OutputType::Info => ("\x1b[0m", "\x1b[0m"),     // default
        OutputType::Success => ("\x1b[32m", "\x1b[0m"), // green
        OutputType::Error => ("\x1b[31m", "\x1b[0m"),   // red
        OutputType::Warning => ("\x1b[33m", "\x1b[0m"), // yellow
    };
    println!(
        "\x1b[90m{}\x1b[0m {}{}{}",
        timestamp, color_code, line.content, reset
    );
}
