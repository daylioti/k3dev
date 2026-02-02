//! Application module
//!
//! This module contains the main App struct and its supporting types,
//! split into focused submodules for maintainability.

mod commands;
mod events;
mod messages;
mod refresh;

use anyhow::Result;
use crossterm::event::{self, Event};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cluster::{ClusterConfig, ClusterManager, ClusterStatus};
use crate::config::{
    Config, ConfigLoader, ConfigValidator, RefreshConfig, RefreshScheduler, RefreshTask,
};
use crate::k8s::K8sClient;
use crate::keybindings::KeybindingResolver;
use crate::ui::components::{
    ActionBar, ClusterAction, ClusterStatus as UiClusterStatus, CommandPalette, ConfirmPopup,
    HelpOverlay, InputForm, Menu, Output, OutputPopup, PasswordPopup, PodContextMenu, PodStats,
    StatusBar,
};
use crate::ui::{AppLayout, Styles};

pub use messages::AppMessage;

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
    SudoPassword,
    Help,
    CommandPalette,
    OutputPopup,
    ConfirmDestroy,
    PodContextMenu,
}

/// Main application
pub struct App {
    // Configuration
    #[allow(dead_code)]
    config: Config,
    cluster_config: Arc<ClusterConfig>,
    refresh_config: RefreshConfig,

    // Clients
    k8s_client: Option<K8sClient>,
    #[allow(dead_code)]
    cluster_manager: Option<ClusterManager>,

    // UI Components
    action_bar: ActionBar,
    menu: Menu,
    output: Output,
    output_popup: OutputPopup,
    pod_stats: PodStats,
    status_bar: StatusBar,
    input_form: InputForm,
    help_overlay: HelpOverlay,
    command_palette: CommandPalette,
    password_popup: PasswordPopup,
    confirm_popup: ConfirmPopup,
    pod_context_menu: PodContextMenu,
    #[allow(dead_code)]
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

    // Pending cluster action (waiting for sudo password)
    pending_cluster_action: Option<ClusterAction>,
    pending_hosts_update: bool,
    sudo_password: String,
    password_input: String,

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
}

impl App {
    pub async fn new(config_path: Option<&str>) -> Result<Self> {
        let loader = ConfigLoader::new(config_path);
        let config = loader.load().unwrap_or_default();

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

        let k8s_client = K8sClient::new(kubeconfig.as_deref(), context.as_deref())
            .await
            .ok();

        let cluster_manager = ClusterManager::new(Arc::clone(&cluster_config)).await.ok();
        let (message_tx, message_rx) = mpsc::channel(100);
        let theme = config.theme;

        let mut menu = Menu::with_theme(theme);
        menu.build_from_config(&config);

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
        command_palette.load_custom_commands(&config.commands);

        let mut action_bar = ActionBar::with_theme(theme);
        let cluster_name = context
            .clone()
            .or_else(|| Some(cluster_config.container_name.clone()));
        action_bar.set_cluster_name(cluster_name);

        Ok(Self {
            config,
            cluster_config,
            refresh_config,
            k8s_client,
            cluster_manager,
            action_bar,
            menu,
            output,
            output_popup: OutputPopup::with_theme(theme),
            pod_stats: PodStats::with_theme(theme),
            status_bar: StatusBar::with_theme(theme),
            input_form: InputForm::with_theme(theme),
            help_overlay,
            command_palette,
            password_popup: PasswordPopup::with_theme(theme),
            confirm_popup: ConfirmPopup::with_theme(theme),
            pod_context_menu: PodContextMenu::with_theme(theme),
            styles: Styles::from_theme(theme),
            focus: FocusArea::Content,
            mode: AppMode::Normal,
            cluster_status: ClusterStatus::Unknown,
            is_executing: false,
            should_quit: false,
            pending_count: String::new(),
            pending_command: None,
            pending_cluster_action: None,
            pending_hosts_update: false,
            sudo_password: String::new(),
            password_input: String::new(),
            message_tx,
            message_rx,
            cancel_token: None,
            scheduler,
            keybinding_resolver,
            current_layout: None,
            menu_width_offset: 0,
        })
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

            // Tick spinner animation
            self.status_bar.tick_spinner();

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
                        self.spawn_resource_stats_check();
                        self.spawn_pod_stats_check();
                    }
                }
            }

            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let longest_menu_item = self.menu.longest_item_width();
        let layout = AppLayout::calculate_with_config(
            frame.area(),
            &self.config.ui,
            longest_menu_item,
            self.menu_width_offset,
        );

        // Render action bar
        self.action_bar
            .render(frame, layout.action_bar, self.focus == FocusArea::ActionBar);

        // Render menu
        self.menu
            .render(frame, layout.menu, self.focus == FocusArea::Content);

        // Render pod stats panel (now takes full right side)
        self.pod_stats
            .render(frame, layout.pod_stats, self.focus == FocusArea::PodStats);

        // Update selected item in status bar based on focus
        let selected_item = match self.focus {
            FocusArea::PodStats => {
                self.pod_stats.selected_pod().map(|p| {
                    // Show full pod name with namespace
                    format!("{} ({})", p.name, p.namespace)
                })
            }
            FocusArea::Content => {
                // Check if ingress is selected
                if let Some((host, path)) = self.menu.selected_ingress_info() {
                    Some(format!("http://{}{}", host, path))
                } else {
                    self.menu.selected_item().map(|i| i.name.clone())
                }
            }
            FocusArea::ActionBar => None,
        };
        self.status_bar.set_selected_item(selected_item);

        // Render status bar
        let ui_status = match self.cluster_status {
            ClusterStatus::Running => UiClusterStatus::Connected,
            ClusterStatus::Stopped | ClusterStatus::NotCreated => UiClusterStatus::Disconnected,
            _ => UiClusterStatus::Unknown,
        };
        self.status_bar.render(frame, layout.status_bar, &ui_status);

        // Render help overlay if in help mode
        if self.mode == AppMode::Help {
            self.help_overlay.render(frame, frame.area());
        }

        // Render command palette if in command palette mode
        if self.mode == AppMode::CommandPalette {
            self.command_palette.render(frame, frame.area());
        }

        // Render password popup if in sudo password mode
        if self.mode == AppMode::SudoPassword {
            self.password_popup
                .render(frame, frame.area(), &self.password_input);
        }

        // Render input form as popup if in input mode
        if self.mode == AppMode::Input {
            self.input_form.render(frame, frame.area());
        }

        // Render output popup if in output popup mode
        if self.mode == AppMode::OutputPopup {
            self.output_popup.render(frame, frame.area());
        }

        // Render confirm popup if in confirm destroy mode
        if self.mode == AppMode::ConfirmDestroy {
            self.confirm_popup.render(frame, frame.area());
        }

        // Render pod context menu if in pod context menu mode
        if self.mode == AppMode::PodContextMenu {
            self.pod_context_menu.render(frame, frame.area());
        }
    }
}
