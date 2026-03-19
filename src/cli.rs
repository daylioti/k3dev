//! CLI command execution (headless, no TUI)
//!
//! Runs cluster actions (start, stop, restart, destroy, info) directly
//! in the terminal, printing colored output instead of rendering the TUI.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::cluster::{ClusterConfig, ClusterManager};
use crate::config::{ConfigLoader, RefreshConfig};
use crate::ui::components::{ClusterAction, OutputLine, OutputType};

/// Run a cluster action headlessly, printing output to stdout.
/// Returns the process exit code (0 = success, 1 = failure).
pub async fn run_cli_action(action: ClusterAction, config_path: Option<&str>) -> Result<i32> {
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

/// Print an OutputLine to stdout with ANSI colors
fn print_output_line(line: &OutputLine) {
    let timestamp = line.timestamp.format("[%H:%M:%S]");
    let (color_code, reset) = match line.output_type {
        OutputType::Info => ("\x1b[0m", "\x1b[0m"),       // default
        OutputType::Success => ("\x1b[32m", "\x1b[0m"),   // green
        OutputType::Error => ("\x1b[31m", "\x1b[0m"),     // red
        OutputType::Warning => ("\x1b[33m", "\x1b[0m"),   // yellow
    };
    println!("\x1b[90m{}\x1b[0m {}{}{}", timestamp, color_code, line.content, reset);
}
