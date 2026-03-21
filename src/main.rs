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
mod shell;
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
}

impl CliCommand {
    fn as_cluster_action(&self) -> ClusterAction {
        match self {
            CliCommand::Start => ClusterAction::Start,
            CliCommand::Stop => ClusterAction::Stop,
            CliCommand::Restart => ClusterAction::Restart,
            CliCommand::Destroy => ClusterAction::Destroy,
            CliCommand::Info => ClusterAction::Info,
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
        let exit_code = cli::run_cli_action(cmd.as_cluster_action(), cli.config.as_deref()).await?;
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

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config_path: Option<&str>,
) -> Result<()> {
    let mut app = App::new(config_path).await?;
    app.run(terminal).await
}
