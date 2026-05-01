//! Pod detail panel — tabbed split view showing Logs, Describe, Timeline, Volumes, Shell

use ratatui::{
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Paragraph, Tabs},
    Frame,
};

use crate::k8s::{PodTimeline, PvcInfo};
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

use super::shell_view::ShellView;

/// Tabs available in the detail panel
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetailTab {
    Logs,
    Describe,
    Timeline,
    Volumes,
    Shell,
    Capture,
}

impl DetailTab {
    fn index(self) -> usize {
        match self {
            DetailTab::Logs => 0,
            DetailTab::Describe => 1,
            DetailTab::Timeline => 2,
            DetailTab::Volumes => 3,
            DetailTab::Shell => 4,
            DetailTab::Capture => 5,
        }
    }

    fn from_index(i: usize) -> Self {
        match i {
            0 => DetailTab::Logs,
            1 => DetailTab::Describe,
            2 => DetailTab::Timeline,
            3 => DetailTab::Volumes,
            4 => DetailTab::Shell,
            _ => DetailTab::Capture,
        }
    }

    fn count() -> usize {
        6
    }
}

/// Per-tab capture state for the Capture tab.
#[derive(Debug, Clone, Default)]
pub struct CaptureTabState {
    /// Whether a capture is currently running for this pod.
    pub running: bool,
    /// Where the .pcap file is being written (set when a capture starts).
    pub output_path: Option<std::path::PathBuf>,
    /// Bytes written so far (live).
    pub bytes: u64,
    /// Packets captured so far (parsed from tcpdump stderr; may be `None`).
    pub packets: Option<u64>,
    /// Last few status lines from tcpdump / orchestrator.
    pub status_lines: Vec<String>,
    /// Error message if the capture failed.
    pub error: Option<String>,
    /// True once the capture has completed (file exists, ready to open).
    pub completed: bool,
}

impl CaptureTabState {
    const MAX_STATUS_LINES: usize = 50;

    fn push_status(&mut self, line: String) {
        self.status_lines.push(line);
        if self.status_lines.len() > Self::MAX_STATUS_LINES {
            let drop = self.status_lines.len() - Self::MAX_STATUS_LINES;
            self.status_lines.drain(..drop);
        }
    }
}

/// Tabbed detail panel for pod inspection (Logs, Describe, Timeline, Volumes, Shell, Capture)
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
    volume_entries: Vec<PvcInfo>,
    shell_view: Option<ShellView>,
    capture: CaptureTabState,
    // Per-tab scroll offset (indexed by DetailTab::index)
    scroll_offsets: [usize; 6],
    // Loading state per tab
    loading: [bool; 6],
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
            volume_entries: Vec::new(),
            shell_view: None,
            capture: CaptureTabState::default(),
            scroll_offsets: [0; 6],
            loading: [false; 6],
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
        // Keep volume_entries — they are updated externally and shared across pods
        self.shell_view = None;
        // Preserve capture state when switching to a different tab on the same pod;
        // a capture is per-pod and survives tab switches.
        if !self.capture.running {
            self.capture = CaptureTabState::default();
        }
        self.scroll_offsets = [0; 6];
        self.loading = [false; 6];
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
        self.volume_entries.clear();
        self.shell_view = None;
        self.capture = CaptureTabState::default();
        self.scroll_offsets = [0; 6];
        self.loading = [false; 6];
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

    /// Set log content, clear loading flag, and auto-scroll to bottom
    pub fn set_logs(&mut self, lines: Vec<String>) {
        self.logs_lines = lines;
        self.loading[DetailTab::Logs.index()] = false;
        // Auto-scroll to bottom so the latest logs are visible;
        // render() clamps this to the actual max scroll offset.
        self.scroll_offsets[DetailTab::Logs.index()] = usize::MAX;
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

    /// Update volume entries (filtered for this pod by the caller)
    pub fn set_volume_entries(&mut self, entries: Vec<PvcInfo>) {
        self.volume_entries = entries;
    }

    /// Read-only access to capture tab state.
    pub fn capture_state(&self) -> &CaptureTabState {
        &self.capture
    }

    /// Mark a new capture as starting for the currently-open pod.
    pub fn capture_started(&mut self, output_path: std::path::PathBuf) {
        self.capture = CaptureTabState {
            running: true,
            output_path: Some(output_path),
            bytes: 0,
            packets: None,
            status_lines: Vec::new(),
            error: None,
            completed: false,
        };
    }

    /// Append a tcpdump status line to the capture tab.
    pub fn push_capture_status(&mut self, line: String) {
        self.capture.push_status(line);
    }

    /// Update live capture progress (bytes/packets).
    pub fn set_capture_progress(&mut self, bytes: u64, packets: Option<u64>) {
        if bytes >= self.capture.bytes {
            self.capture.bytes = bytes;
        }
        if let Some(p) = packets {
            self.capture.packets = Some(p);
        }
    }

    /// Finalise capture state on success.
    pub fn set_capture_complete(
        &mut self,
        path: std::path::PathBuf,
        bytes: u64,
        packets: Option<u64>,
    ) {
        self.capture.running = false;
        self.capture.completed = true;
        self.capture.output_path = Some(path);
        if bytes >= self.capture.bytes {
            self.capture.bytes = bytes;
        }
        if packets.is_some() {
            self.capture.packets = packets;
        }
    }

    /// Mark capture as failed.
    pub fn set_capture_failed(&mut self, err: String) {
        self.capture.running = false;
        self.capture.completed = false;
        self.capture.error = Some(err);
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
            DetailTab::Volumes => self.build_volumes_lines().len(),
            DetailTab::Shell => {
                if self.shell_view.is_some() {
                    0 // VT100 handles its own screen
                } else {
                    3 // placeholder lines
                }
            }
            DetailTab::Capture => self.build_capture_lines().len(),
        }
    }

    fn build_content_lines(&self) -> Vec<Line<'_>> {
        match self.active_tab {
            DetailTab::Logs => self.build_logs_lines(),
            DetailTab::Describe => self.build_describe_lines(),
            DetailTab::Timeline => self.build_timeline_lines(),
            DetailTab::Volumes => self.build_volumes_lines(),
            DetailTab::Shell => self.build_shell_lines(),
            DetailTab::Capture => self.build_capture_lines(),
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

    fn build_volumes_lines(&self) -> Vec<Line<'_>> {
        // Filter volumes for the current pod
        let pod_volumes: Vec<&PvcInfo> = self
            .volume_entries
            .iter()
            .filter(|v| v.pods.iter().any(|p| p == &self.pod_name))
            .collect();

        if pod_volumes.is_empty() {
            return vec![Line::from(Span::styled(
                "  No volumes attached to this pod",
                self.styles.muted_text,
            ))];
        }

        let mut lines: Vec<Line> = Vec::new();

        for entry in &pod_volumes {
            let ns_label = if entry.namespace == "default" {
                String::new()
            } else {
                format!(" ({})", entry.namespace)
            };

            // PVC name header
            lines.push(Line::from(Span::styled(
                format!("  \u{1f4e6} {}{}", entry.name, ns_label),
                self.styles.title,
            )));

            // Storage class and phase
            if !entry.storage_class.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("    Storage class: ", self.styles.muted_text),
                    Span::styled(&entry.storage_class, self.styles.normal_text),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("    Phase: ", self.styles.muted_text),
                Span::styled(&entry.phase, self.styles.normal_text),
            ]));

            // Usage bar
            match entry.used_bytes {
                Some(used) if entry.capacity_bytes > 0 => {
                    let percent =
                        (used as f64 / entry.capacity_bytes as f64 * 100.0).min(100.0) as u8;
                    let bar_width = 20;
                    let filled = ((percent as f64 / 100.0) * bar_width as f64).round() as usize;
                    let empty = bar_width - filled;

                    let bar_filled = "\u{2588}".repeat(filled);
                    let bar_empty = "\u{2591}".repeat(empty);

                    let usage_style = if percent >= 90 {
                        self.styles.error_text
                    } else if percent >= 80 {
                        self.styles.warning_text
                    } else {
                        self.styles.success_text
                    };

                    lines.push(Line::from(vec![
                        Span::styled("    Usage: ", self.styles.muted_text),
                        Span::styled(bar_filled, usage_style),
                        Span::styled(bar_empty, self.styles.muted_text),
                        Span::styled(
                            format!(
                                " {} / {} ({}%)",
                                format_bytes(used),
                                format_bytes(entry.capacity_bytes),
                                percent,
                            ),
                            usage_style,
                        ),
                    ]));
                }
                Some(used) => {
                    lines.push(Line::from(vec![
                        Span::styled("    Used: ", self.styles.muted_text),
                        Span::styled(format_bytes(used), self.styles.normal_text),
                    ]));
                }
                None if entry.capacity_bytes > 0 => {
                    lines.push(Line::from(vec![
                        Span::styled("    Capacity: ", self.styles.muted_text),
                        Span::styled(format_bytes(entry.capacity_bytes), self.styles.normal_text),
                    ]));
                }
                None => {
                    lines.push(Line::from(Span::styled(
                        "    Not provisioned yet",
                        self.styles.muted_text,
                    )));
                }
            }

            lines.push(Line::from(""));
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

    fn build_capture_lines(&self) -> Vec<Line<'_>> {
        let cap = &self.capture;
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        let status_text = if let Some(err) = &cap.error {
            format!("Failed: {}", err)
        } else if cap.running {
            format!(
                "Capturing… {} ({})",
                format_bytes(cap.bytes),
                cap.packets
                    .map(|p| format!("{} packets", p))
                    .unwrap_or_else(|| "packets: --".to_string())
            )
        } else if cap.completed {
            format!(
                "Done — {} ({})",
                format_bytes(cap.bytes),
                cap.packets
                    .map(|p| format!("{} packets", p))
                    .unwrap_or_else(|| "packets: --".to_string())
            )
        } else {
            "Idle — press 's' to start".to_string()
        };
        let status_style = if cap.error.is_some() {
            self.styles.error_text
        } else if cap.running {
            self.styles.info_text
        } else if cap.completed {
            self.styles.success_text
        } else {
            self.styles.muted_text
        };
        lines.push(Line::from(vec![
            Span::styled("  Status: ", self.styles.muted_text),
            Span::styled(status_text, status_style),
        ]));

        if let Some(path) = &cap.output_path {
            lines.push(Line::from(vec![
                Span::styled("  Output: ", self.styles.muted_text),
                Span::styled(path.display().to_string(), self.styles.normal_text),
            ]));
        }

        let actions = if cap.running {
            "  [x] stop   [Esc] cancel".to_string()
        } else if cap.completed {
            "  [s] new   [o] open in Wireshark".to_string()
        } else {
            "  [s] start".to_string()
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(actions, self.styles.title)));

        if !cap.status_lines.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  tcpdump:",
                self.styles.muted_text,
            )));
            for line in &cap.status_lines {
                lines.push(Line::from(Span::styled(
                    format!("    {}", line),
                    self.styles.normal_text,
                )));
            }
        }

        lines
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
        let capture_label = if self.capture.running {
            "Capture(x)"
        } else if self.capture.completed {
            "Capture(o)"
        } else {
            "Capture(c)"
        };
        let tab_titles: Vec<Line> = vec![
            Line::from("Logs(l)"),
            Line::from("Describe(d)"),
            Line::from("Timeline(t)"),
            Line::from("Volumes(v)"),
            Line::from(shell_label),
            Line::from(capture_label),
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

/// Format bytes as human-readable (Ki/Mi/Gi)
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}Gi", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}Mi", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0}Ki", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
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
