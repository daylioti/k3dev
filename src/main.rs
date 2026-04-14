use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Write};
use std::panic;

mod app;
mod cli;
mod cluster;
mod commands;
mod config;
mod hooks;
mod k8s;
mod keybindings;
mod logging;
mod ui;

use app::App;
use ui::components::ClusterAction;

#[derive(Parser)]
#[command(name = "k3dev")]
#[command(version = "0.1.0")]
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
    Destroy,
    /// Show cluster info
    Info,
    /// Delete all snapshot images
    DeleteSnapshots,
    /// Run cluster diagnostics (health checks)
    Diagnostics,
    /// Run preflight checks (verify cluster can start)
    Preflight,
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
}

impl CliCommand {
    fn as_cluster_action(&self) -> Option<ClusterAction> {
        match self {
            CliCommand::Start => Some(ClusterAction::Start),
            CliCommand::Stop => Some(ClusterAction::Stop),
            CliCommand::Restart => Some(ClusterAction::Restart),
            CliCommand::Destroy => Some(ClusterAction::Destroy),
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
    let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, cli.config.as_deref()).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Run docker CLI with DOCKER_HOST pointing to the raw Docker daemon socket relay.
/// On macOS, Docker Desktop's proxy filters container visibility. This command
/// starts a socat relay (if not running) and passes all args to `docker`.
async fn run_docker_passthrough(args: &[String], config_path: Option<&str>) -> Result<i32> {
    let loader = config::ConfigLoader::new(config_path);
    let config = loader.load().unwrap_or_default();
    let cluster_config = cluster::ClusterConfig::from(config.infrastructure);

    // Check container is running
    let docker = cluster::DockerManager::from_default_socket()?;
    let running = docker
        .container_running(&cluster_config.container_name)
        .await;
    if !running {
        eprintln!("Cluster is not running. Start it first with: k3dev start");
        return Ok(1);
    }

    // Find the relay port — check if socat is already listening on a port
    let relay_port = find_or_start_docker_relay(&docker, &cluster_config.container_name).await?;

    // Pass through to docker CLI
    let status = std::process::Command::new("docker")
        .args(args)
        .env("DOCKER_HOST", format!("tcp://localhost:{}", relay_port))
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to run docker: {}", e))?;

    Ok(status.code().unwrap_or(1))
}

/// Find an existing socat relay or start a new one. Returns the host port.
async fn find_or_start_docker_relay(
    docker: &cluster::DockerManager,
    container_name: &str,
) -> Result<u16> {
    // Check if socat is already running on any port
    let ps_output = docker
        .exec_in_container(
            container_name,
            &[
                "sh",
                "-c",
                "ps aux 2>/dev/null | grep 'socat TCP-LISTEN' | grep -v grep",
            ],
        )
        .await
        .unwrap_or_default();

    if let Some(port) = parse_socat_port(&ps_output) {
        return Ok(port);
    }

    // Discover which port was published for the relay by inspecting the container
    let port = discover_published_relay_port(docker, container_name)
        .await
        .unwrap_or_else(|| cluster::find_available_port(2375).unwrap_or(2375));

    // Ensure socat binary exists — install if missing via bollard upload
    let socat_check = docker
        .exec_in_container(container_name, &["ls", "/usr/local/bin/socat"])
        .await;
    if socat_check.is_err() {
        #[cfg(target_arch = "aarch64")]
        const SOCAT_BINARY: &[u8] = include_bytes!("../assets/socat-aarch64");
        #[cfg(target_arch = "x86_64")]
        const SOCAT_BINARY: &[u8] = include_bytes!("../assets/socat-x86_64");

        let _ = docker
            .exec_in_container(container_name, &["mkdir", "-p", "/usr/local/bin"])
            .await;
        docker
            .copy_to_container(container_name, "socat", SOCAT_BINARY, "/usr/local/bin")
            .await?;
    }

    // Start socat relay via bollard detached exec
    docker
        .exec_detached(
            container_name,
            &[
                "/usr/local/bin/socat",
                &format!("TCP-LISTEN:{},fork,reuseaddr,bind=0.0.0.0", port),
                "UNIX-CONNECT:/proc/1/root/run/docker.sock",
            ],
        )
        .await?;

    // Brief wait for socat to bind
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    Ok(port)
}

/// Discover the published relay port by inspecting container port bindings.
/// Looks for a published port in the 2375-2474 range (Docker API relay).
async fn discover_published_relay_port(
    docker: &cluster::DockerManager,
    container_name: &str,
) -> Option<u16> {
    let ports = docker.get_container_ports(container_name).await.ok()?;
    for container_port in 2375..2475u16 {
        if let Some(&host_port) = ports.get(&container_port) {
            return Some(host_port);
        }
    }
    None
}

/// Parse the listening port from socat process listing
fn parse_socat_port(ps_output: &str) -> Option<u16> {
    // Look for "TCP-LISTEN:NNNN" in process listing
    for line in ps_output.lines() {
        if let Some(idx) = line.find("TCP-LISTEN:") {
            let after = &line[idx + 11..];
            let port_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(port) = port_str.parse::<u16>() {
                return Some(port);
            }
        }
    }
    None
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config_path: Option<&str>,
) -> Result<()> {
    let mut app = App::new(config_path).await?;
    app.run(terminal).await
}
