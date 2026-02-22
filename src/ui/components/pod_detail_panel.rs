//! Pod detail panel — tabbed split view showing Logs, Describe, Timeline, Shell

use ratatui::{
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Paragraph, Tabs},
    Frame,
};

use crate::k8s::PodTimeline;
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

use super::shell_view::ShellView;

/// Tabs available in the detail panel
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetailTab {
    Logs,
    Describe,
    Timeline,
    Shell,
}

impl DetailTab {
    fn index(self) -> usize {
        match self {
            DetailTab::Logs => 0,
            DetailTab::Describe => 1,
            DetailTab::Timeline => 2,
            DetailTab::Shell => 3,
        }
    }

    fn from_index(i: usize) -> Self {
        match i {
            0 => DetailTab::Logs,
            1 => DetailTab::Describe,
            2 => DetailTab::Timeline,
            _ => DetailTab::Shell,
        }
    }

    fn count() -> usize {
        4
    }
}

/// Tabbed detail panel for pod inspection (Logs, Describe, Timeline, Shell)
pub struct PodDetailPanel {
    styles: Styles,
    is_open: bool,
    active_tab: DetailTab,
    pod_name: String,
    namespace: String,
    // Content per tab
    logs_lines: Vec<String>,
    describe_lines: Vec<String>,
    timeline: Option<PodTimeline>,
    shell_view: Option<ShellView>,
    // Per-tab scroll offset (indexed by DetailTab::index)
    scroll_offsets: [usize; 4],
    // Loading state per tab
    loading: [bool; 4],
    // Whether shell mode is active (keyboard input goes to shell)
    shell_interactive: bool,
}

impl PodDetailPanel {
    pub fn with_theme(theme: Theme) -> Self {
        Self {
            styles: Styles::from_theme(theme),
            is_open: false,
            active_tab: DetailTab::Logs,
            pod_name: String::new(),
            namespace: String::new(),
            logs_lines: Vec::new(),
            describe_lines: Vec::new(),
            timeline: None,
            shell_view: None,
            scroll_offsets: [0; 4],
            loading: [false; 4],
            shell_interactive: false,
        }
    }

    /// Open the panel for a pod on the given tab, clearing previous content
    pub fn open(&mut self, pod_name: String, namespace: String, tab: DetailTab) {
        self.is_open = true;
        self.active_tab = tab;
        self.pod_name = pod_name;
        self.namespace = namespace;
        self.logs_lines.clear();
        self.describe_lines.clear();
        self.timeline = None;
        self.shell_view = None;
        self.scroll_offsets = [0; 4];
        self.loading = [false; 4];
        self.shell_interactive = false;
    }

    /// Close the panel and reset all state
    pub fn close(&mut self) {
        self.is_open = false;
        self.pod_name.clear();
        self.namespace.clear();
        self.logs_lines.clear();
        self.describe_lines.clear();
        self.timeline = None;
        self.shell_view = None;
        self.scroll_offsets = [0; 4];
        self.loading = [false; 4];
        self.shell_interactive = false;
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    /// Switch to a specific tab (does NOT trigger data load — App does that)
    pub fn set_tab(&mut self, tab: DetailTab) {
        self.active_tab = tab;
    }

    /// Cycle to the next tab
    pub fn next_tab(&mut self) {
        let next = (self.active_tab.index() + 1) % DetailTab::count();
        self.active_tab = DetailTab::from_index(next);
    }

    /// Cycle to the previous tab
    pub fn prev_tab(&mut self) {
        let prev = if self.active_tab.index() == 0 {
            DetailTab::count() - 1
        } else {
            self.active_tab.index() - 1
        };
        self.active_tab = DetailTab::from_index(prev);
    }

    pub fn active_tab(&self) -> DetailTab {
        self.active_tab
    }

    pub fn pod_name(&self) -> &str {
        &self.pod_name
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Set log content and clear loading flag
    pub fn set_logs(&mut self, lines: Vec<String>) {
        self.logs_lines = lines;
        self.loading[DetailTab::Logs.index()] = false;
    }

    /// Set describe content and clear loading flag
    pub fn set_describe(&mut self, lines: Vec<String>) {
        self.describe_lines = lines;
        self.loading[DetailTab::Describe.index()] = false;
    }

    /// Set timeline content and clear loading flag
    pub fn set_timeline(&mut self, timeline: PodTimeline) {
        self.timeline = Some(timeline);
        self.loading[DetailTab::Timeline.index()] = false;
    }

    /// Mark a tab as loading
    pub fn set_loading(&mut self, tab: DetailTab, loading: bool) {
        self.loading[tab.index()] = loading;
    }

    /// Initialize the shell view with given dimensions
    pub fn init_shell_view(&mut self, rows: u16, cols: u16) {
        self.shell_view = Some(ShellView::new(rows, cols));
    }

    /// Whether a shell view is active
    pub fn has_shell_view(&self) -> bool {
        self.shell_view.is_some()
    }

    /// Feed raw shell output bytes into the VT100 parser
    pub fn feed_shell_output(&mut self, bytes: &[u8]) {
        if let Some(view) = &mut self.shell_view {
            view.process(bytes);
        }
    }

    /// Mark the shell view as connected
    pub fn set_shell_connected(&mut self) {
        if let Some(view) = &mut self.shell_view {
            view.set_connected();
        }
    }

    /// Set shell error state
    pub fn set_shell_error(&mut self, msg: String) {
        if let Some(view) = &mut self.shell_view {
            view.set_error(msg);
        }
    }

    /// Set shell disconnected state
    pub fn set_shell_disconnected(&mut self) {
        if let Some(view) = &mut self.shell_view {
            view.set_disconnected();
        }
    }

    /// Set shell interactive state (keyboard input goes to shell)
    pub fn set_shell_interactive(&mut self, interactive: bool) {
        self.shell_interactive = interactive;
    }

    /// Resize the shell VT100 parser
    pub fn resize_shell(&mut self, rows: u16, cols: u16) {
        if let Some(view) = &mut self.shell_view {
            view.set_size(rows, cols);
        }
    }

    /// Scroll current tab content up by one line
    pub fn scroll_up(&mut self) {
        let idx = self.active_tab.index();
        self.scroll_offsets[idx] = self.scroll_offsets[idx].saturating_sub(1);
    }

    /// Scroll current tab content down, bounded by visible height
    pub fn scroll_down(&mut self, visible_height: usize) {
        let idx = self.active_tab.index();
        let total = self.content_line_count();
        let max_scroll = total.saturating_sub(visible_height);
        if self.scroll_offsets[idx] < max_scroll {
            self.scroll_offsets[idx] += 1;
        }
    }

    /// Get the number of content lines for the active tab
    fn content_line_count(&self) -> usize {
        match self.active_tab {
            DetailTab::Logs => self.logs_lines.len(),
            DetailTab::Describe => self.describe_lines.len(),
            DetailTab::Timeline => self.build_timeline_lines().len(),
            DetailTab::Shell => {
                if self.shell_view.is_some() {
                    0 // VT100 handles its own screen
                } else {
                    3 // placeholder lines
                }
            }
        }
    }

    fn build_content_lines(&self) -> Vec<Line<'_>> {
        match self.active_tab {
            DetailTab::Logs => self.build_logs_lines(),
            DetailTab::Describe => self.build_describe_lines(),
            DetailTab::Timeline => self.build_timeline_lines(),
            DetailTab::Shell => self.build_shell_lines(),
        }
    }

    fn build_logs_lines(&self) -> Vec<Line<'_>> {
        if self.logs_lines.is_empty() {
            if self.loading[DetailTab::Logs.index()] {
                return vec![Line::from(Span::styled(
                    "  Loading logs...",
                    self.styles.muted_text,
                ))];
            }
            return vec![Line::from(Span::styled(
                "  No logs available",
                self.styles.muted_text,
            ))];
        }
        self.logs_lines
            .iter()
            .map(|l| Line::from(Span::styled(format!("  {}", l), self.styles.normal_text)))
            .collect()
    }

    fn build_describe_lines(&self) -> Vec<Line<'_>> {
        if self.describe_lines.is_empty() {
            if self.loading[DetailTab::Describe.index()] {
                return vec![Line::from(Span::styled(
                    "  Loading...",
                    self.styles.muted_text,
                ))];
            }
            return vec![Line::from(Span::styled(
                "  No description available",
                self.styles.muted_text,
            ))];
        }
        self.describe_lines
            .iter()
            .map(|l| Line::from(Span::styled(format!("  {}", l), self.styles.normal_text)))
            .collect()
    }

    fn build_timeline_lines(&self) -> Vec<Line<'_>> {
        let timeline = match &self.timeline {
            Some(t) => t,
            None => {
                if self.loading[DetailTab::Timeline.index()] {
                    return vec![Line::from(Span::styled(
                        "  Loading timeline...",
                        self.styles.muted_text,
                    ))];
                }
                return vec![Line::from(Span::styled(
                    "  No timeline available",
                    self.styles.muted_text,
                ))];
            }
        };

        let mut lines: Vec<Line> = Vec::new();

        // Total duration header
        let total_str = match &timeline.total_duration {
            Some(dur) => {
                let label = if timeline.is_ready {
                    "Created -> Ready"
                } else {
                    "Created -> now (not ready)"
                };
                format!("  Total: {} ({})", format_elapsed(dur), label)
            }
            None => "  Total: unknown".to_string(),
        };
        lines.push(Line::from(Span::styled(total_str, self.styles.title)));
        lines.push(Line::from(""));

        // Note
        if let Some(note) = &timeline.note {
            lines.push(Line::from(Span::styled(
                format!("  {}", note),
                self.styles.warning_text,
            )));
            lines.push(Line::from(""));
        }

        // Phase bars
        let total_secs = timeline
            .total_duration
            .map(|d| d.num_seconds().max(1))
            .unwrap_or(1) as f64;

        if timeline.phases.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No phase data available",
                self.styles.muted_text,
            )));
        } else {
            for phase in &timeline.phases {
                let phase_secs = phase.duration.num_seconds();
                let pct = (phase_secs as f64 / total_secs * 100.0).min(100.0);
                let bar_width = 12;
                let filled = ((pct / 100.0) * bar_width as f64).round() as usize;
                let empty = bar_width - filled;

                let bar_filled: String = "\u{2588}".repeat(filled);
                let bar_empty: String = "\u{2591}".repeat(empty);

                lines.push(Line::from(vec![
                    Span::styled("  \u{25b8} ", self.styles.normal_text),
                    Span::styled(format!("{:<18}", phase.name), self.styles.normal_text),
                    Span::styled(
                        format!("{:>4}  ", format_elapsed(&phase.duration)),
                        self.styles.info_text,
                    ),
                    Span::styled(bar_filled, self.styles.success_text),
                    Span::styled(bar_empty, self.styles.muted_text),
                    Span::styled(format!("  {:>3.0}%", pct), self.styles.muted_text),
                ]));
            }
        }

        lines.push(Line::from(""));

        // Events section
        lines.push(Line::from(Span::styled("  Events:", self.styles.title)));

        if timeline.events.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no events available)",
                self.styles.muted_text,
            )));
        } else {
            for event in &timeline.events {
                let time_str = event.timestamp.format("%H:%M:%S").to_string();
                let max_msg_len = 40;
                let msg = if event.message.len() > max_msg_len {
                    format!("{}...", &event.message[..max_msg_len - 3])
                } else {
                    event.message.clone()
                };

                lines.push(Line::from(vec![
                    Span::styled(format!("  {} ", time_str), self.styles.muted_text),
                    Span::styled(format!("{:<16}", event.reason), self.styles.info_text),
                    Span::styled(msg, self.styles.normal_text),
                ]));
            }
        }

        lines
    }

    fn build_shell_lines(&self) -> Vec<Line<'_>> {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Press 'e' to open interactive shell",
                self.styles.info_text,
            )),
            Line::from(Span::styled(
                format!("  Pod: {} ({})", self.pod_name, self.namespace),
                self.styles.muted_text,
            )),
        ]
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Tab bar labels — shell tab shows context-sensitive hint
        let shell_label = if self.shell_interactive {
            "Shell(Esc)"
        } else if self.shell_view.is_some() {
            "Shell(Enter)"
        } else {
            "Shell(e)"
        };
        let tab_titles: Vec<Line> = vec![
            Line::from("Logs(l)"),
            Line::from("Describe(d)"),
            Line::from("Timeline(t)"),
            Line::from(shell_label),
        ];

        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);

        // Render tab bar
        let tabs = Tabs::new(tab_titles)
            .select(self.active_tab.index())
            .style(self.styles.muted_text)
            .highlight_style(self.styles.title)
            .divider(Span::styled(" | ", self.styles.muted_text));

        frame.render_widget(tabs, chunks[0]);

        // Render content with scrolling (no border — parent draws the outer block)
        let content_area = chunks[1];

        // Shell tab with active shell_view gets special VT100 rendering
        if self.active_tab == DetailTab::Shell {
            if let Some(ref shell_view) = self.shell_view {
                shell_view.render(frame, content_area);
                return;
            }
        }

        let lines = self.build_content_lines();
        let visible_height = content_area.height as usize;
        let max_scroll = lines.len().saturating_sub(visible_height);
        let scroll = self.scroll_offsets[self.active_tab.index()].min(max_scroll);

        let visible_lines: Vec<Line> = lines
            .into_iter()
            .skip(scroll)
            .take(visible_height)
            .collect();

        let paragraph = Paragraph::new(visible_lines);
        frame.render_widget(paragraph, content_area);
    }
}

/// Format a chrono::Duration as a human-readable string
fn format_elapsed(duration: &chrono::Duration) -> String {
    let secs = duration.num_seconds();
    if secs < 0 {
        return "0s".to_string();
    }
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}
