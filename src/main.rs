use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Write};
use std::panic;

mod app;
mod bench;
mod capture;
mod cli;
mod cluster;
mod commands;
mod config;
mod hooks;
mod k8s;
mod keybindings;
mod logging;
mod tty;
mod ui;

use app::{App, RunOutcome};
use ui::components::ClusterAction;

#[derive(Parser)]
#[command(name = "k3dev")]
#[command(version = env!("K3DEV_VERSION"))]
#[command(about = "TUI for local k3s cluster development")]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Start the cluster
    Start,
    /// Stop the cluster
    Stop,
    /// Restart the cluster
    Restart,
    /// Destroy the cluster
    #[command(alias = "delete")]
    Destroy {
        /// Also delete the snapshot images (otherwise the next start restores
        /// cluster state from them)
        #[arg(long)]
        all: bool,
    },
    /// Show cluster info
    Info,
    /// Delete all snapshot images
    DeleteSnapshots,
    /// Run cluster diagnostics (health checks)
    Diagnostics,
    /// Run preflight checks (verify cluster can start)
    Preflight,
    /// Benchmark app startup performance (config load, status, time-to-pods)
    Bench {
        /// Emit the report as JSON instead of a colored table
        #[arg(long)]
        json: bool,
    },
    /// Update /etc/hosts with ingress entries
    UpdateHosts,
    /// List pods with status
    Pods {
        /// Namespace to list pods from (default: all namespaces)
        #[arg(short, long)]
        namespace: Option<String>,
    },
    /// View pod logs
    Logs {
        /// Pod name
        pod: String,
        /// Namespace (default: "default")
        #[arg(short, long, default_value = "default")]
        namespace: String,
        /// Container name (for multi-container pods)
        #[arg(long)]
        container: Option<String>,
        /// Number of tail lines
        #[arg(short, long, default_value = "100")]
        tail: i64,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Describe a pod
    Describe {
        /// Pod name
        pod: String,
        /// Namespace (default: "default")
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
    /// Delete a pod
    DeletePod {
        /// Pod name
        pod: String,
        /// Namespace (default: "default")
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
    /// Restart a pod (delete and let deployment recreate)
    RestartPod {
        /// Pod name
        pod: String,
        /// Namespace (default: "default")
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
    /// Execute a shell in a pod
    Exec {
        /// Pod name
        pod: String,
        /// Namespace (default: "default")
        #[arg(short, long, default_value = "default")]
        namespace: String,
        /// Container name (for multi-container pods)
        #[arg(long)]
        container: Option<String>,
        /// Command to execute (default: /bin/sh)
        #[arg(long, default_value = "/bin/sh")]
        cmd: String,
    },
    /// Run docker CLI against the cluster's Docker daemon (bypasses Docker Desktop proxy)
    #[command(trailing_var_arg = true)]
    Docker {
        /// Arguments passed to docker CLI
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Capture network traffic to a .pcap file (Wireshark-compatible).
    ///
    /// Either `--pod` or `--container` must be given.
    #[command(group(
        clap::ArgGroup::new("capture-target")
            .required(true)
            .args(["pod", "container"]),
    ))]
    Capture {
        /// Pod name to capture from (joins the pod's pause container netns).
        #[arg(long)]
        pod: Option<String>,
        /// Namespace for `--pod`.
        #[arg(short, long, default_value = "default")]
        namespace: String,
        /// Docker container name to capture from directly.
        #[arg(long)]
        container: Option<String>,
        /// Network interface to capture on (`-i` flag of tcpdump).
        #[arg(long, default_value = "any")]
        iface: String,
        /// BPF filter, e.g. "port 80 or icmp".
        #[arg(short, long)]
        filter: Option<String>,
        /// Stop after this duration (e.g. "30s", "5m", "1h").
        #[arg(short, long)]
        duration: Option<String>,
        /// Stop after this many bytes (e.g. "100M", "1G").
        #[arg(long)]
        max_bytes: Option<String>,
        /// Explicit output path (overrides `--out-dir`).
        #[arg(long)]
        out: Option<String>,
        /// Directory to write the .pcap file (overrides config).
        #[arg(long)]
        out_dir: Option<String>,
        /// Sidecar Docker image (overrides config).
        #[arg(long)]
        image: Option<String>,
        /// Open the resulting .pcap in Wireshark on stop.
        #[arg(long)]
        open: bool,
    },
}

impl CliCommand {
    fn as_cluster_action(&self) -> Option<ClusterAction> {
        match self {
            CliCommand::Start => Some(ClusterAction::Start),
            CliCommand::Stop => Some(ClusterAction::Stop),
            CliCommand::Restart => Some(ClusterAction::Restart),
            CliCommand::Destroy { all } => Some(ClusterAction::Destroy { all: *all }),
            CliCommand::Info => Some(ClusterAction::Info),
            CliCommand::DeleteSnapshots => Some(ClusterAction::DeleteSnapshots),
            _ => None,
        }
    }
}

/// Restore terminal to normal state
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = stdout.flush();
}

#[tokio::main]
async fn main() -> Result<()> {
    // On macOS, ensure Homebrew binary paths are in PATH.
    // ARM64 macOS installs Homebrew to /opt/homebrew/bin, which may not be
    // in PATH for non-interactive shells (e.g., launched from GUI apps).
    #[cfg(target_os = "macos")]
    {
        let homebrew_dirs = ["/opt/homebrew/bin", "/usr/local/bin"];
        let current_path = std::env::var("PATH").unwrap_or_default();
        let mut needs_update = false;
        let mut new_path = current_path.clone();
        for dir in &homebrew_dirs {
            if std::path::Path::new(dir).is_dir() && !current_path.split(':').any(|p| p == *dir) {
                new_path = format!("{}:{}", dir, new_path);
                needs_update = true;
            }
        }
        if needs_update {
            std::env::set_var("PATH", &new_path);
        }
    }

    let cli = Cli::parse();

    // If a subcommand was given, run headlessly (no TUI)
    if let Some(cmd) = &cli.command {
        let config_path = cli.config.as_deref();
        let exit_code = match cmd {
            CliCommand::Docker { args } => run_docker_passthrough(args, config_path).await?,
            CliCommand::Diagnostics => cli::run_cli_diagnostics(config_path).await?,
            CliCommand::Preflight => cli::run_cli_preflight(config_path).await?,
            CliCommand::Bench { json } => cli::run_cli_bench(config_path, *json).await?,
            CliCommand::UpdateHosts => cli::run_cli_update_hosts(config_path).await?,
            CliCommand::Pods { namespace } => {
                cli::run_cli_pods(config_path, namespace.as_deref()).await?
            }
            CliCommand::Logs {
                pod,
                namespace,
                container,
                tail,
                follow,
            } => {
                cli::run_cli_logs(
                    config_path,
                    pod,
                    namespace,
                    container.as_deref(),
                    *tail,
                    *follow,
                )
                .await?
            }
            CliCommand::Describe { pod, namespace } => {
                cli::run_cli_describe(config_path, pod, namespace).await?
            }
            CliCommand::DeletePod { pod, namespace } => {
                cli::run_cli_delete_pod(config_path, pod, namespace).await?
            }
            CliCommand::RestartPod { pod, namespace } => {
                cli::run_cli_restart_pod(config_path, pod, namespace).await?
            }
            CliCommand::Exec {
                pod,
                namespace,
                container,
                cmd: shell_cmd,
            } => {
                cli::run_cli_exec(config_path, pod, namespace, container.as_deref(), shell_cmd)
                    .await?
            }
            CliCommand::Capture {
                pod,
                namespace,
                container,
                iface,
                filter,
                duration,
                max_bytes,
                out,
                out_dir,
                image,
                open,
            } => {
                cli::run_cli_capture(
                    config_path,
                    pod.as_deref(),
                    namespace,
                    container.as_deref(),
                    iface.clone(),
                    filter.as_deref(),
                    duration.as_deref(),
                    max_bytes.as_deref(),
                    out.as_deref(),
                    out_dir.as_deref(),
                    image.as_deref(),
                    *open,
                )
                .await?
            }
            _ => {
                if let Some(action) = cmd.as_cluster_action() {
                    cli::run_cli_action(action, config_path).await?
                } else {
                    0
                }
            }
        };
        std::process::exit(exit_code);
    }

    // Otherwise, launch the TUI
    // Set up panic hook to restore terminal on panic
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, cli.config.as_deref()).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    result
}

/// Run the docker CLI against the daemon the clusters run on.
///
/// Docker Desktop's host socket hides the pod containers cri-dockerd creates,
/// so on macOS the CLI goes through a shared relay to the VM's raw socket.
/// Elsewhere, and for a TCP or remote endpoint, the CLI already talks to that
/// daemon directly and gets the args unchanged.
async fn run_docker_passthrough(args: &[String], config_path: Option<&str>) -> Result<i32> {
    let mut command = std::process::Command::new("docker");
    command.args(args);

    #[cfg(target_os = "macos")]
    if cluster::docker_relay::needed() {
        let loader = config::ConfigLoader::new(config_path);
        let config = loader.load().unwrap_or_default();
        let cluster_config = cluster::ClusterConfig::from(config.infrastructure);

        let docker = cluster::DockerManager::from_default_socket()?;
        if !docker
            .container_running(&cluster_config.container_name)
            .await
        {
            eprintln!("Cluster is not running. Start it first with: k3dev start");
            return Ok(1);
        }

        let relay_port =
            cluster::docker_relay::ensure(&docker, &cluster_config.k3s_image()).await?;
        command.env("DOCKER_HOST", format!("tcp://127.0.0.1:{}", relay_port));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = config_path;

    let status = command
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to run docker: {}", e))?;

    Ok(status.code().unwrap_or(1))
}

/// Run the TUI, rebuilding the App whenever the user switches cluster.
///
/// A cluster is defined by its whole config file (menu, hooks, info blocks,
/// keybindings, theme), so switching rebuilds `App` from scratch rather than
/// mutating it. The terminal stays in raw/alternate-screen mode across the
/// swap, so there is no flicker.
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config_path: Option<&str>,
) -> Result<()> {
    let mut path = config_path.map(str::to_string);

    loop {
        let mut app = App::new(path.as_deref()).await?;
        match app.run(terminal).await? {
            RunOutcome::Quit => return Ok(()),
            RunOutcome::Switch(next) => path = Some(next.to_string_lossy().into_owned()),
        }
    }
}
