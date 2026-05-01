//! Application module
//!
//! This module contains the main App struct and its supporting types,
//! split into focused submodules for maintainability.

mod commands;
mod events;
pub(crate) mod messages;
mod refresh;

use anyhow::Result;
use crossterm::event::{self, Event};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    Terminal,
};
use std::io::Stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use bollard::Docker;

use crate::cluster::{ClusterConfig, ClusterStatus, ContainerPullProgress, ContainerStats};
use crate::config::{
    Config, ConfigLoader, ConfigValidator, InfoBlock, RefreshConfig, RefreshScheduler, RefreshTask,
    VisibleCheck,
};
use crate::k8s::PendingPodInfo;
use crate::k8s::{K8sClient, ShellSessionHandle};
use crate::keybindings::KeybindingResolver;
use crate::ui::components::{
    ActionBar, ClusterAction, CommandPalette, ConfirmPopup, DetailTab, DiagnosticsOverlay,
    HelpOverlay, InputForm, Menu, Output, OutputPopup, PodDetailPanel, PodStats,
};
use crate::ui::{AppLayout, Styles};
use std::collections::{HashMap, HashSet};

pub use messages::{AppMessage, InfoBlockResult, InfoBlockStatus};

/// Per-block runtime state for scheduling info block refreshes.
pub(super) struct InfoBlockRuntime {
    pub(super) cfg: InfoBlock,
    pub(super) last_run: Instant,
    pub(super) in_flight: bool,
    pub(super) last_output: String,
}

/// Which UI entry a visibility task drives.
#[derive(Debug, Clone)]
pub(super) enum VisibilityTarget {
    /// Path into `config.commands`: `[group_idx, entry_idx, (sub_idx, ...)]`.
    Command { path: Vec<usize> },
    /// Index into `config.info_blocks`.
    InfoBlock { index: usize },
}

/// Runtime state for a single visibility probe.
pub(super) struct VisibilityTask {
    pub(super) check: VisibleCheck,
    pub(super) interval: Duration,
    pub(super) last_run: Instant,
    pub(super) in_flight: bool,
    pub(super) visible: bool,
    pub(super) target: VisibilityTarget,
}

/// Focus area in the UI
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FocusArea {
    ActionBar,
    Content,
    PodStats,
}

/// Application mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppMode {
    Normal,
    Input,
    Help,
    CommandPalette,
    OutputPopup,
    ConfirmDestroy,
    Diagnostics,
    Shell,
}

/// Main application
pub struct App {
    // Configuration
    config: Config,
    cluster_config: Arc<ClusterConfig>,
    refresh_config: RefreshConfig,

    // Clients
    k8s_client: Option<K8sClient>,

    // UI Components
    action_bar: ActionBar,
    menu: Menu,
    output: Output,
    output_popup: OutputPopup,
    pod_stats: PodStats,
    input_form: InputForm,
    help_overlay: HelpOverlay,
    command_palette: CommandPalette,
    confirm_popup: ConfirmPopup,
    diagnostics_overlay: DiagnosticsOverlay,
    pod_detail_panel: PodDetailPanel,
    styles: Styles,

    // State
    focus: FocusArea,
    mode: AppMode,
    cluster_status: ClusterStatus,
    is_executing: bool,
    should_quit: bool,

    // Vim-style number prefix for navigation (e.g., "3j" moves down 3)
    pending_count: String,

    // Pending command for input
    pending_command: Option<crate::config::CommandEntry>,

    // Pending cluster action (waiting for confirmation)
    pending_cluster_action: Option<ClusterAction>,

    // Pending interactive sudo for hosts update (content, host_count)
    pending_sudo_hosts_content: Option<(String, usize)>,

    // Cached data for pod stats merging
    running_pods_cache: Vec<ContainerStats>,
    pending_pods_cache: Vec<PendingPodInfo>,
    /// Cache of image pull progress (image -> progress)
    pull_progress_cache: HashMap<String, ContainerPullProgress>,
    /// Tracks images with active streaming monitors
    active_pull_monitors: HashSet<String>,
    /// Bollard Docker client for spawning pull monitors
    docker_client: Option<Docker>,

    /// Shared DockerManager for capture sidecars (lazily created on first use).
    docker_manager: Option<Arc<crate::cluster::DockerManager>>,

    /// Cache of volume/PVC entries (all volumes, filtered per pod when needed)
    volume_entries_cache: Vec<crate::k8s::PvcInfo>,

    /// Cache of pod image architectures (pod_key → architecture)
    image_arch_cache: HashMap<String, String>,
    /// Whether an image arch check is currently in flight
    image_arch_check_pending: bool,

    // Interactive shell session
    shell_session: Option<ShellSessionHandle>,
    shell_area_size: (u16, u16),

    // Pending command to send to shell once session is ready
    pending_shell_command: Option<String>,

    // Async channels
    message_tx: mpsc::Sender<AppMessage>,
    message_rx: mpsc::Receiver<AppMessage>,

    // Cancellation
    cancel_token: Option<CancellationToken>,

    // Unified refresh scheduler
    scheduler: RefreshScheduler,

    // Keybinding resolver
    keybinding_resolver: KeybindingResolver,

    // Current layout for mouse click detection
    current_layout: Option<AppLayout>,

    // Menu width offset from user adjustments (+/- keys)
    menu_width_offset: i16,

    // Whether auto-preflight has been triggered for stopped screen
    preflight_auto_triggered: bool,

    // Runtime state for user-configured sidebar info blocks
    pub(super) info_blocks: Vec<InfoBlockRuntime>,

    // Runtime state for `visible` probes attached to commands / info blocks.
    pub(super) visibility_tasks: Vec<VisibilityTask>,
    // Command paths currently hidden (mirror of `!task.visible`).
    pub(super) hidden_command_paths: HashSet<Vec<usize>>,
    // Info-block indices currently hidden.
    pub(super) hidden_info_blocks: HashSet<usize>,
}

impl App {
    pub async fn new(config_path: Option<&str>) -> Result<Self> {
        let loader = ConfigLoader::new(config_path);
        let (config, config_file_path) = loader
            .load_with_path()
            .map(|(c, p)| (c, Some(p)))
            .unwrap_or_default();

        let validation_result = ConfigValidator::new(&config).validate();
        let validation_warnings: Vec<String> = validation_result
            .warnings
            .iter()
            .map(|w| format!("Config warning: {}", w))
            .collect();

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
                .with_k8s_config(kubeconfig.clone(), context.clone()),
        );

        let _ = crate::logging::init_logging(&config.logging, &config.infrastructure.cluster_name);

        // K8s client is created lazily - not at startup.
        let k8s_client: Option<K8sClient> = None;

        let (message_tx, message_rx) = mpsc::channel(100);
        let theme = config.theme;

        let mut menu = Menu::with_theme(theme);
        menu.build_from_config(&config);

        // Seed info block runtime + placeholder views so headers render before
        // the first refresh completes.
        let now = Instant::now();
        let info_blocks: Vec<InfoBlockRuntime> = config
            .info_blocks
            .iter()
            .map(|cfg| InfoBlockRuntime {
                cfg: cfg.clone(),
                // Make the first tick fire immediately by back-dating `last_run`.
                last_run: now.checked_sub(cfg.interval).unwrap_or(now),
                in_flight: false,
                last_output: String::new(),
            })
            .collect();
        let info_block_views: Vec<crate::ui::components::InfoBlockView> = config
            .info_blocks
            .iter()
            .map(|cfg| crate::ui::components::InfoBlockView {
                name: cfg.name.clone(),
                icon: cfg.icon.clone(),
                output: String::new(),
                status: InfoBlockStatus::Skipped,
                hidden: false,
            })
            .collect();
        menu.set_info_blocks(info_block_views);

        // Walk the config for any `visible` gates — every gated entry starts
        // hidden until its first probe succeeds.
        let mut visibility_tasks: Vec<VisibilityTask> = Vec::new();
        let mut hidden_command_paths: HashSet<Vec<usize>> = HashSet::new();
        let mut hidden_info_blocks: HashSet<usize> = HashSet::new();

        for (group_idx, group) in config.commands.iter().enumerate() {
            for (entry_idx, entry) in group.commands.iter().enumerate() {
                let base_path = vec![group_idx, entry_idx];
                seed_command_visibility(
                    entry,
                    base_path,
                    now,
                    &mut visibility_tasks,
                    &mut hidden_command_paths,
                );
            }
        }
        for (idx, block) in config.info_blocks.iter().enumerate() {
            if let Some(v) = &block.visible {
                visibility_tasks.push(VisibilityTask {
                    check: v.check.clone(),
                    interval: v.interval,
                    last_run: now.checked_sub(v.interval).unwrap_or(now),
                    in_flight: false,
                    visible: false,
                    target: VisibilityTarget::InfoBlock { index: idx },
                });
                hidden_info_blocks.insert(idx);
            }
        }

        // Propagate initial hidden sets to the UI so the first render filters.
        menu.set_hidden_command_paths(hidden_command_paths.clone());
        for idx in &hidden_info_blocks {
            menu.set_info_block_hidden(*idx, true);
        }

        let refresh_config = RefreshConfig::default();
        let scheduler = RefreshScheduler::new(&refresh_config);
        let keybinding_resolver = KeybindingResolver::from_config(config.keybindings.as_ref());

        let mut output = Output::with_theme(theme);
        for warning in validation_warnings {
            output.add_warning(&warning);
        }

        let mut help_overlay = HelpOverlay::with_theme(theme);
        help_overlay.update_from_resolver(&keybinding_resolver);

        let mut command_palette = CommandPalette::with_theme(theme);
        command_palette.load_custom_commands(&config.commands, &hidden_command_paths);

        let mut action_bar = ActionBar::with_theme(theme);
        let cluster_name = context
            .clone()
            .or_else(|| Some(cluster_config.container_name.clone()));
        action_bar.set_cluster_name(cluster_name);
        action_bar.set_config_path(config_file_path);

        Ok(Self {
            config,
            cluster_config,
            refresh_config,
            k8s_client,
            action_bar,
            menu,
            output,
            output_popup: OutputPopup::with_theme(theme),
            pod_stats: PodStats::with_theme(theme),
            input_form: InputForm::with_theme(theme),
            help_overlay,
            command_palette,
            confirm_popup: ConfirmPopup::with_theme(theme),
            diagnostics_overlay: DiagnosticsOverlay::with_theme(theme),
            pod_detail_panel: PodDetailPanel::with_theme(theme),
            styles: Styles::from_theme(theme),
            focus: FocusArea::Content,
            mode: AppMode::Normal,
            cluster_status: ClusterStatus::Unknown,
            is_executing: false,
            should_quit: false,
            pending_count: String::new(),
            pending_command: None,
            pending_cluster_action: None,
            pending_sudo_hosts_content: None,
            running_pods_cache: Vec::new(),
            pending_pods_cache: Vec::new(),
            pull_progress_cache: HashMap::new(),
            active_pull_monitors: HashSet::new(),
            docker_client: crate::cluster::PlatformInfo::connect_docker().ok(),
            docker_manager: None,
            volume_entries_cache: Vec::new(),
            image_arch_cache: HashMap::new(),
            image_arch_check_pending: false,
            shell_session: None,
            shell_area_size: (0, 0),
            pending_shell_command: None,
            message_tx,
            message_rx,
            cancel_token: None,
            scheduler,
            keybinding_resolver,
            current_layout: None,
            menu_width_offset: 0,
            preflight_auto_triggered: false,
            info_blocks,
            visibility_tasks,
            hidden_command_paths,
            hidden_info_blocks,
        })
    }

    /// Merge a fresh probe result into the task list and, if the flag changed,
    /// reflect it in the UI.
    pub(super) fn apply_visibility_update(&mut self, id: usize, visible: bool) {
        let Some(task) = self.visibility_tasks.get_mut(id) else {
            return;
        };
        task.in_flight = false;
        if task.visible == visible {
            return;
        }
        task.visible = visible;

        match task.target.clone() {
            VisibilityTarget::Command { path } => {
                if visible {
                    self.hidden_command_paths.remove(&path);
                } else {
                    self.hidden_command_paths.insert(path);
                }
                self.menu
                    .set_hidden_command_paths(self.hidden_command_paths.clone());
                self.command_palette
                    .load_custom_commands(&self.config.commands, &self.hidden_command_paths);
            }
            VisibilityTarget::InfoBlock { index } => {
                if visible {
                    self.hidden_info_blocks.remove(&index);
                } else {
                    self.hidden_info_blocks.insert(index);
                }
                self.menu.set_info_block_hidden(index, !visible);
            }
        }
    }

    /// Run the application event loop
    pub async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        // Initial data load
        self.spawn_status_check();

        loop {
            // Render and capture layout
            terminal.draw(|frame| {
                let longest_menu_item = self.menu.longest_item_width();
                self.current_layout = Some(AppLayout::calculate_with_config(
                    frame.area(),
                    &self.config.ui,
                    longest_menu_item,
                    self.menu_width_offset,
                ));
                self.render(frame);
            })?;

            // Handle events with timeout for async messages
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) => self.handle_key(key.code, key.modifiers),
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
                }
            }

            // Process async messages
            while let Ok(msg) = self.message_rx.try_recv() {
                self.handle_message(msg);
            }

            // Handle pending interactive sudo (needs terminal access)
            if let Some((content, count)) = self.pending_sudo_hosts_content.take() {
                self.run_interactive_sudo_hosts_update(terminal, &content, count);
            }

            // Process scheduled refresh tasks
            for task in self.scheduler.tick() {
                match task {
                    RefreshTask::BlinkToggle => {
                        self.menu.toggle_blink();
                    }
                    RefreshTask::IngressRefresh => {
                        self.spawn_ingress_health_check();
                        self.spawn_ingress_refresh();
                        self.spawn_port_forwards_check();
                    }
                    RefreshTask::HostsCheck => {
                        self.spawn_missing_hosts_check();
                    }
                    RefreshTask::StatsRefresh => {
                        self.spawn_pod_stats_check();
                        self.spawn_pending_pods_check();
                        self.spawn_pull_progress_check();
                        // Auto-refresh logs when the Logs tab is visible
                        if self.pod_detail_panel.is_open()
                            && self.pod_detail_panel.active_tab() == DetailTab::Logs
                        {
                            let pod_name = self.pod_detail_panel.pod_name().to_string();
                            let namespace = self.pod_detail_panel.namespace().to_string();
                            self.load_detail_logs(&pod_name, &namespace);
                        }
                    }
                    RefreshTask::VolumeRefresh => {
                        self.spawn_volume_stats_check();
                    }
                }
            }

            // Process per-block info block schedules
            self.info_block_tick();

            // Re-evaluate `visible` probes on their own cadence.
            self.visibility_tick();

            // Handle shell area resize
            if self.pod_detail_panel.is_open()
                && self.pod_detail_panel.active_tab() == DetailTab::Shell
                && self.pod_detail_panel.has_shell_view()
            {
                let (rows, cols) = self.calculate_shell_dimensions();
                if rows > 0 && cols > 0 && (rows, cols) != self.shell_area_size {
                    self.shell_area_size = (rows, cols);
                    self.pod_detail_panel.resize_shell(rows, cols);
                    if let Some(session) = &self.shell_session {
                        session.resize(rows, cols);
                    }
                }
            }

            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// Run sudo interactively by temporarily exiting TUI raw mode.
    /// This allows native sudo auth (password prompt, TouchID on macOS, etc.)
    fn run_interactive_sudo_hosts_update(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        content: &str,
        count: usize,
    ) {
        use std::io::Write;

        let hosts_path = if cfg!(windows) {
            if let Ok(windir) = std::env::var("SystemRoot") {
                std::path::PathBuf::from(windir)
                    .join("System32")
                    .join("drivers")
                    .join("etc")
                    .join("hosts")
            } else {
                std::path::PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
            }
        } else {
            std::path::PathBuf::from("/etc/hosts")
        };

        // Write content to temp file
        let temp_path = std::env::temp_dir().join("k3dev-hosts");
        if std::fs::write(&temp_path, content).is_err() {
            self.output
                .add_error("Failed to write temporary hosts file");
            return;
        }

        // Exit raw mode so sudo can interact with the terminal
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);

        // Print a message so the user knows what's happening
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "\nUpdating /etc/hosts ({} entries)...\n", count);
        let _ = stdout.flush();

        // Run sudo cp interactively (allows TouchID, password prompt, etc.)
        let hosts_str = hosts_path.to_string_lossy();
        let temp_str = temp_path.to_string_lossy();

        let success = if cfg!(windows) {
            // On Windows, try direct copy
            std::process::Command::new("cmd")
                .args(["/C", "copy", &temp_str, &hosts_str])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            let cp_ok = std::process::Command::new("sudo")
                .args(["cp", &temp_str, &hosts_str])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

            // On Linux, restore SELinux label if copy succeeded
            #[cfg(target_os = "linux")]
            if cp_ok {
                let _ = std::process::Command::new("sudo")
                    .args(["restorecon", &hosts_str])
                    .status();
            }

            cp_ok
        };

        if success {
            let _ = writeln!(stdout, "\nUpdated /etc/hosts with {} entries.", count);
        } else {
            let _ = writeln!(stdout, "\nFailed to update /etc/hosts.");
        }
        let _ = writeln!(stdout, "Press Enter to return to k3dev...");
        let _ = stdout.flush();

        // Wait for user to press Enter before returning to TUI
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);

        // Cleanup temp file
        let _ = std::fs::remove_file(&temp_path);

        // Re-enter raw mode and alternate screen
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen);
        let _ = crossterm::terminal::enable_raw_mode();

        // Force terminal to clear and redraw
        let _ = terminal.clear();

        // Update status message
        if success {
            self.output
                .add_success(format!("Updated /etc/hosts with {} entries", count));
        } else {
            self.output
                .add_error("Failed to update /etc/hosts (sudo cancelled or failed)");
        }
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let longest_menu_item = self.menu.longest_item_width();
        let layout = AppLayout::calculate_with_config(
            frame.area(),
            &self.config.ui,
            longest_menu_item,
            self.menu_width_offset,
        );

        let is_cluster_stopped = !matches!(
            self.cluster_status,
            ClusterStatus::Running | ClusterStatus::Starting
        );

        // Render action bar (always visible)
        self.action_bar
            .render(frame, layout.action_bar, self.focus == FocusArea::ActionBar);

        if is_cluster_stopped {
            self.render_stopped_screen(frame, &layout);
        } else {
            self.render_running_screen(frame, &layout);
        }

        // Render modal overlays
        if self.mode == AppMode::Help {
            self.help_overlay.render(frame, frame.area());
        }
        if self.mode == AppMode::CommandPalette {
            self.command_palette.render(frame, frame.area());
        }
        if self.mode == AppMode::Input {
            self.input_form.render(frame, frame.area());
        }
        if self.mode == AppMode::OutputPopup {
            self.output_popup.render(frame, frame.area());
        }
        if self.mode == AppMode::ConfirmDestroy {
            self.confirm_popup.render(frame, frame.area());
        }
        if self.mode == AppMode::Diagnostics {
            self.diagnostics_overlay.render(frame, frame.area());
        }
    }

    /// Render the stopped screen: action list (left) + preflight results (right)
    fn render_stopped_screen(&mut self, frame: &mut ratatui::Frame, layout: &AppLayout) {
        // Use the full content area (menu + pod_stats combined)
        let content_area = ratatui::layout::Rect::new(
            layout.menu.x,
            layout.menu.y,
            layout.menu.width + layout.pod_stats.width,
            layout.menu.height,
        );

        // Split into two columns
        let columns = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(content_area);

        // Left: vertical action list
        let focused = self.focus == FocusArea::Content || self.focus == FocusArea::ActionBar;
        self.action_bar.render_vertical(frame, columns[0], focused);

        // Right: inline preflight results
        self.diagnostics_overlay.render_inline(frame, columns[1]);
    }

    /// Render the normal running screen: menu (left) + pods (right)
    fn render_running_screen(&mut self, frame: &mut ratatui::Frame, layout: &AppLayout) {
        // Render menu
        self.menu
            .render(frame, layout.menu, self.focus == FocusArea::Content);

        // Render pod stats panel
        let focused = self.focus == FocusArea::PodStats;
        if self.pod_detail_panel.is_open() {
            let border_style = if focused {
                self.styles.border_focused
            } else {
                self.styles.border_unfocused
            };
            let border_type = if focused {
                ratatui::widgets::BorderType::Thick
            } else {
                ratatui::widgets::BorderType::Rounded
            };
            let title = if focused {
                " \u{25b6} Pods \u{25c0} "
            } else {
                "   Pods "
            };
            let title_style = if focused {
                self.styles
                    .title
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                self.styles.normal_text
            };
            let block = ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_type(border_type)
                .border_style(border_style)
                .title(ratatui::text::Span::styled(title, title_style));

            let inner = block.inner(layout.pod_stats);
            frame.render_widget(block, layout.pod_stats);

            let split = Layout::vertical([
                Constraint::Percentage(50),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);

            self.pod_stats.render_inner(frame, split[0], focused);

            let sep_line = ratatui::text::Line::from("\u{2500}".repeat(split[1].width as usize));
            frame.render_widget(
                ratatui::widgets::Paragraph::new(sep_line).style(self.styles.border_unfocused),
                split[1],
            );

            self.pod_detail_panel.render(frame, split[2]);
        } else {
            self.pod_stats.render(frame, layout.pod_stats, focused);
        }
    }
}

/// Recursively walk a `CommandEntry`, pushing a `VisibilityTask` for any
/// `visible` gate found (on the entry itself and on nested children).
fn seed_command_visibility(
    entry: &crate::config::CommandEntry,
    path: Vec<usize>,
    now: Instant,
    tasks: &mut Vec<VisibilityTask>,
    hidden: &mut HashSet<Vec<usize>>,
) {
    if let Some(v) = &entry.visible {
        tasks.push(VisibilityTask {
            check: v.check.clone(),
            interval: v.interval,
            last_run: now.checked_sub(v.interval).unwrap_or(now),
            in_flight: false,
            visible: false,
            target: VisibilityTarget::Command { path: path.clone() },
        });
        hidden.insert(path.clone());
    }

    for (child_idx, child) in entry.commands.iter().enumerate() {
        let mut child_path = path.clone();
        child_path.push(child_idx);
        seed_command_visibility(child, child_path, now, tasks, hidden);
    }
}
