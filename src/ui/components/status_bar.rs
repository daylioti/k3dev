use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::time::Instant;

use crate::cluster::ResourceStats;
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Cluster connection status
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ClusterStatus {
    Connected,
    Disconnected,
    #[default]
    Unknown,
}

impl ClusterStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterStatus::Connected => "Connected",
            ClusterStatus::Disconnected => "Disconnected",
            ClusterStatus::Unknown => "Unknown",
        }
    }
}

/// Spinner animation frames
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Status bar component
pub struct StatusBar {
    styles: Styles,
    is_executing: bool,
    resource_stats: Option<ResourceStats>,
    pending_count: Option<String>,
    spinner_frame: usize,
    selected_item: Option<String>,
    // For network rate calculation
    prev_net_rx: f64,
    prev_net_tx: f64,
    last_stats_time: Option<Instant>,
    net_rx_rate: f64, // bytes per second
    net_tx_rate: f64, // bytes per second
    // Last refresh timestamp
    last_refresh: Option<Instant>,
    // Panel resize hint
    resize_hint: Option<i16>,
    resize_hint_time: Option<Instant>,
}

impl StatusBar {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            styles: Styles::from_theme(theme),
            is_executing: false,
            resource_stats: None,
            pending_count: None,
            spinner_frame: 0,
            selected_item: None,
            prev_net_rx: 0.0,
            prev_net_tx: 0.0,
            last_stats_time: None,
            net_rx_rate: 0.0,
            net_tx_rate: 0.0,
            last_refresh: None,
            resize_hint: None,
            resize_hint_time: None,
        }
    }

    /// Set the selected item name to display
    pub fn set_selected_item(&mut self, item: Option<String>) {
        self.selected_item = item;
    }

    /// Mark data as refreshed (call when data is successfully updated)
    pub fn mark_refreshed(&mut self) {
        self.last_refresh = Some(Instant::now());
    }

    pub fn set_executing(&mut self, executing: bool) {
        self.is_executing = executing;
    }

    pub fn set_resource_stats(&mut self, stats: Option<ResourceStats>) {
        if let Some(ref stats) = stats {
            // Calculate network rates
            if let Some(prev_time) = self.last_stats_time {
                let elapsed = prev_time.elapsed().as_secs_f64();
                if elapsed > 0.0 {
                    // Convert MB to KB/s
                    let rx_delta = (stats.net_rx_mb - self.prev_net_rx) * 1024.0; // MB to KB
                    let tx_delta = (stats.net_tx_mb - self.prev_net_tx) * 1024.0;
                    self.net_rx_rate = rx_delta / elapsed; // KB/s
                    self.net_tx_rate = tx_delta / elapsed;
                }
            }
            self.prev_net_rx = stats.net_rx_mb;
            self.prev_net_tx = stats.net_tx_mb;
            self.last_stats_time = Some(Instant::now());
            self.mark_refreshed();
        }
        self.resource_stats = stats;
    }

    pub fn set_pending_count(&mut self, count: Option<String>) {
        self.pending_count = count;
    }

    /// Set resize hint (panel width) to display temporarily
    pub fn set_resize_hint(&mut self, width: Option<i16>) {
        self.resize_hint = width;
        self.resize_hint_time = width.map(|_| Instant::now());
    }

    /// Advance the spinner animation (call every ~100ms when executing)
    pub fn tick_spinner(&mut self) {
        if self.is_executing {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        }

        // Clear resize hint after 2 seconds
        if let Some(hint_time) = self.resize_hint_time {
            if hint_time.elapsed().as_secs() >= 2 {
                self.resize_hint = None;
                self.resize_hint_time = None;
            }
        }
    }

    /// Get current spinner frame character
    fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, cluster_status: &ClusterStatus) {
        // Split into left (help/selected), center (stats), and right (status)
        // Use Min constraints to ensure stats section has enough space
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(30), // Left: selected item or help hints
                Constraint::Min(35), // Center: stats (needs enough space for NET)
                Constraint::Min(15), // Right: status indicator
            ])
            .split(area);

        // Left side: help keys or selected item (with pending count if any, or spinner if executing)
        let mut help_spans = vec![];

        if self.is_executing {
            // Show spinner and cancel hint when executing
            help_spans.push(Span::styled(
                format!("{} ", self.spinner()),
                self.styles.warning_text,
            ));
            help_spans.push(Span::styled(
                "Running... │ Ctrl+C: cancel",
                self.styles.muted_text,
            ));
        } else if let Some(width) = self.resize_hint {
            // Show panel resize hint
            let base_width = 35; // Default menu width
            let actual_width = base_width + width;
            help_spans.push(Span::styled(
                format!("Menu width: {} ", actual_width),
                self.styles.warning_text,
            ));
            help_spans.push(Span::styled("│ +/- to adjust", self.styles.muted_text));
        } else if let Some(count) = &self.pending_count {
            help_spans.push(Span::styled(count.clone(), self.styles.warning_text));
            help_spans.push(Span::styled(
                " │ q: quit │ Tab: focus │ jk: nav",
                self.styles.muted_text,
            ));
        } else if let Some(item) = &self.selected_item {
            // Show selected item name (truncate if too long)
            help_spans.push(Span::styled("▸ ", self.styles.warning_text));
            let max_item_len = 30;
            let display_item = if item.len() > max_item_len {
                format!("{}...", &item[..max_item_len - 3])
            } else {
                item.clone()
            };
            help_spans.push(Span::styled(display_item, self.styles.title));
        } else {
            help_spans.push(Span::styled(
                "q: quit │ Tab: focus │ jk: nav │ r: refresh │ ?: help",
                self.styles.muted_text,
            ));
        }

        let help = Paragraph::new(Line::from(help_spans));
        frame.render_widget(help, chunks[0]);

        // Center: resource stats with network rate
        if let Some(stats) = &self.resource_stats {
            let stats_spans = vec![
                Span::styled("CPU: ", self.styles.muted_text),
                Span::styled(
                    format!("{:.1}%", stats.cpu_percent),
                    self.get_cpu_style(stats.cpu_percent),
                ),
                Span::styled(" │ ", self.styles.muted_text),
                Span::styled("RAM: ", self.styles.muted_text),
                Span::styled(
                    format!("{:.0}/{:.0}MB", stats.memory_used_mb, stats.memory_total_mb),
                    self.get_memory_style(stats.memory_percent()),
                ),
                Span::styled(" │ ", self.styles.muted_text),
                Span::styled("NET: ", self.styles.muted_text),
                Span::styled(
                    format!(
                        "↓{} ↑{}",
                        format_rate(self.net_rx_rate),
                        format_rate(self.net_tx_rate)
                    ),
                    self.styles.muted_text,
                ),
            ];
            let stats_line = Paragraph::new(Line::from(stats_spans));
            frame.render_widget(stats_line, chunks[1]);
        }

        // Right side: cluster status with text indicator and last refresh
        let (status_icon, status_style) = match cluster_status {
            ClusterStatus::Connected => ("[OK]", self.styles.status_connected),
            ClusterStatus::Disconnected => ("[X]", self.styles.status_disconnected),
            ClusterStatus::Unknown => ("[?]", self.styles.status_unknown),
        };

        // Add last refresh timestamp if available
        let refresh_text = if let Some(last) = self.last_refresh {
            let elapsed = last.elapsed().as_secs();
            if elapsed < 60 {
                format!(" {}s", elapsed)
            } else {
                format!(" {}m", elapsed / 60)
            }
        } else {
            String::new()
        };

        let status_text = format!(
            "{} {}{}",
            status_icon,
            cluster_status.as_str(),
            refresh_text
        );
        let status =
            Paragraph::new(Line::from(Span::styled(status_text, status_style))).right_aligned();

        frame.render_widget(status, chunks[2]);
    }

    fn get_cpu_style(&self, cpu_percent: f64) -> ratatui::style::Style {
        if cpu_percent > 80.0 {
            self.styles.status_disconnected // Red for high CPU
        } else if cpu_percent > 50.0 {
            self.styles.status_unknown // Yellow/warning for medium
        } else {
            self.styles.status_connected // Green for low
        }
    }

    fn get_memory_style(&self, memory_percent: f64) -> ratatui::style::Style {
        if memory_percent > 80.0 {
            self.styles.status_disconnected // Red for high memory
        } else if memory_percent > 50.0 {
            self.styles.status_unknown // Yellow/warning for medium
        } else {
            self.styles.status_connected // Green for low
        }
    }
}

/// Format a size in MB to a human readable string
#[allow(dead_code)]
fn format_size(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1}GB", mb / 1024.0)
    } else if mb >= 1.0 {
        format!("{:.0}MB", mb)
    } else {
        format!("{:.0}KB", mb * 1024.0)
    }
}

/// Format a rate in KB/s to a compact human readable string
fn format_rate(kb_per_sec: f64) -> String {
    if kb_per_sec >= 1024.0 {
        format!("{:.0}M", kb_per_sec / 1024.0)
    } else if kb_per_sec >= 1.0 {
        format!("{:.0}K", kb_per_sec)
    } else {
        format!("{:.0}B", kb_per_sec * 1024.0)
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}
