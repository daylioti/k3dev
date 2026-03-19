//! Pod stats component showing running pods with Docker-based metrics

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
    Frame,
};

use crate::cluster::PullPhase;
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

// === Resource Display Constants ===

/// Threshold above which we consider memory "unlimited" (8GB)
/// When no K8s limit is set, Docker reports host's total RAM
const MEMORY_UNLIMITED_THRESHOLD_MB: f64 = 8192.0;

/// Threshold above which we consider CPU "unlimited" (32 cores = 32000m)
/// When no K8s limit is set, cgroups reports "max" which we convert to 0
const CPU_UNLIMITED_THRESHOLD_MILLICORES: f64 = 32000.0;

/// Maximum displayable CPU in millicores (64 cores) - sanity cap
const CPU_MAX_DISPLAY_MILLICORES: f64 = 64000.0;

/// How much over limit to allow before capping display (2x)
const CPU_OVER_LIMIT_FACTOR: f64 = 2.0;

/// Millicores per core
const MILLICORES_PER_CORE: f64 = 1000.0;

/// Conversion factor from cpu_percent to millicores (100% = 1000m)
const CPU_PERCENT_TO_MILLICORES: f64 = 10.0;

/// Pull progress for a single container within a pod
#[derive(Debug, Clone)]
pub struct ContainerPullInfo {
    #[allow(dead_code)]
    pub container_name: String,
    #[allow(dead_code)]
    pub image: String,
    /// Progress percentage (0-100), None if unknown
    pub progress_percent: Option<f64>,
    /// Total bytes to download
    pub total_bytes: u64,
    /// Bytes downloaded so far
    pub downloaded_bytes: u64,
    /// Current phase of the pull operation
    pub phase: PullPhase,
    /// Number of layers completed (downloaded + cached)
    pub layers_done: u16,
    /// Total number of layers
    pub layers_total: u16,
}

impl ContainerPullInfo {
    pub fn new(container_name: &str, image: &str) -> Self {
        Self {
            container_name: container_name.to_string(),
            image: image.to_string(),
            progress_percent: None,
            total_bytes: 0,
            downloaded_bytes: 0,
            phase: PullPhase::Downloading,
            layers_done: 0,
            layers_total: 0,
        }
    }

    pub fn with_progress(mut self, downloaded: u64, total: u64) -> Self {
        self.downloaded_bytes = downloaded;
        self.total_bytes = total;
        self.progress_percent = if total > 0 {
            Some((downloaded as f64 / total as f64 * 100.0).min(100.0))
        } else {
            None
        };
        self
    }
}

/// Pod display state - determines how the pod is rendered in the UI
#[derive(Debug, Clone, Default)]
pub enum PodState {
    /// Pod is running normally, show CPU/RAM bars
    #[default]
    Running,
    /// Pod is waiting for image pull - one or more containers pulling
    Pulling {
        containers: Vec<ContainerPullInfo>,
        started_at: Option<DateTime<Utc>>,
    },
    /// Pod is waiting for other reasons
    Waiting { reason: String },
    /// Pod has failed (e.g., ImagePullBackOff)
    Failed { reason: String },
}

/// Stats for a single pod/container
#[derive(Debug, Clone)]
pub struct PodStat {
    pub name: String,
    pub namespace: String,
    /// Pod display state - determines rendering behavior
    pub state: PodState,
    // For Running state:
    pub cpu_percent: f64,
    pub cpu_limit_millicores: f64,
    pub memory_used_mb: f64,
    pub memory_limit_mb: f64,
    #[allow(dead_code)]
    pub status: String,
    /// True if the pod's image architecture doesn't match the host
    pub arch_mismatch: bool,
}

impl PodStat {
    pub fn memory_percent(&self) -> f64 {
        if self.memory_limit_mb > 0.0 {
            (self.memory_used_mb / self.memory_limit_mb) * 100.0
        } else {
            0.0
        }
    }

    /// Check if pod has a meaningful memory limit set
    /// Returns false if limit is very high (likely host RAM = no K8s limit)
    pub fn has_memory_limit(&self) -> bool {
        self.memory_limit_mb > 0.0 && self.memory_limit_mb < MEMORY_UNLIMITED_THRESHOLD_MB
    }

    /// Check if pod has a meaningful CPU limit set
    pub fn has_cpu_limit(&self) -> bool {
        self.cpu_limit_millicores > 0.0
            && self.cpu_limit_millicores < CPU_UNLIMITED_THRESHOLD_MILLICORES
    }

    /// Get CPU usage as percentage of limit (if limit exists)
    pub fn cpu_percent_of_limit(&self) -> f64 {
        if self.cpu_limit_millicores > 0.0 {
            let used_millicores = self.cpu_percent * CPU_PERCENT_TO_MILLICORES;
            (used_millicores / self.cpu_limit_millicores) * 100.0
        } else {
            0.0
        }
    }
}

/// Pod stats panel component
pub struct PodStats {
    pods: Vec<PodStat>,
    scroll_offset: usize,
    selected_index: usize,
    styles: Styles,
    /// Animation tick (currently unused, kept for API compat)
    #[allow(dead_code)]
    animation_tick: u8,
    /// Pod names that should be highlighted (e.g., targets of selected menu command)
    highlighted_pods: HashSet<String>,
}

impl PodStats {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            pods: Vec::new(),
            scroll_offset: 0,
            selected_index: 0,
            styles: Styles::from_theme(theme),
            animation_tick: 0,
            highlighted_pods: HashSet::new(),
        }
    }

    /// Advance the animation tick
    pub fn tick_animation(&mut self) {
        self.animation_tick = (self.animation_tick + 1) % 8;
    }

    /// Check if any pods are in a pulling/waiting state (need animation)
    #[allow(dead_code)]
    pub fn has_pending_pods(&self) -> bool {
        self.pods
            .iter()
            .any(|p| matches!(p.state, PodState::Pulling { .. } | PodState::Waiting { .. }))
    }

    pub fn set_pods(&mut self, pods: Vec<PodStat>) {
        self.pods = pods;
        // Reset scroll and selection if pods changed significantly
        if self.scroll_offset > self.pods.len() {
            self.scroll_offset = 0;
        }
        if self.selected_index >= self.pods.len() {
            self.selected_index = self.pods.len().saturating_sub(1);
        }
    }

    /// Get the currently selected pod (if any)
    pub fn selected_pod(&self) -> Option<&PodStat> {
        self.pods.get(self.selected_index)
    }

    /// Get all pods
    pub fn pods(&self) -> &[PodStat] {
        &self.pods
    }

    /// Set which pod names should be highlighted
    pub fn set_highlighted_pods(&mut self, pods: HashSet<String>) {
        self.highlighted_pods = pods;
    }

    /// Select a pod by index and adjust scroll
    pub fn select_index(&mut self, index: usize) {
        if index < self.pods.len() {
            self.selected_index = index;
            let pod_line = self.line_index_of_pod(self.selected_index);
            if pod_line < self.scroll_offset {
                self.scroll_offset = pod_line;
            }
        }
    }

    /// How many visual lines a pod occupies.
    /// Pulling pods with N containers get N lines (min 1); all others get 1.
    fn pod_line_count(pod: &PodStat) -> usize {
        match &pod.state {
            PodState::Pulling { containers, .. } => containers.len().max(1),
            _ => 1,
        }
    }

    /// Compute the visual line offset of the pod at `target_idx`,
    /// accounting for namespace headers, multi-line pods, and separators.
    fn line_index_of_pod(&self, target_idx: usize) -> usize {
        let mut line = 0;
        let mut current_ns = String::new();
        for (i, pod) in self.pods.iter().enumerate() {
            if pod.namespace != current_ns {
                current_ns = pod.namespace.clone();
                line += 1; // namespace header
            }
            if i == target_idx {
                return line;
            }
            line += Self::pod_line_count(pod);
            // Separator line between pods
            if i + 1 < self.pods.len() {
                line += 1;
            }
        }
        line
    }

    /// Total number of visual lines (namespace headers + pod lines + separators).
    fn total_visual_lines(&self) -> usize {
        let mut total = 0;
        let mut current_ns = String::new();
        for (i, pod) in self.pods.iter().enumerate() {
            if pod.namespace != current_ns {
                current_ns = pod.namespace.clone();
                total += 1; // namespace header
            }
            total += Self::pod_line_count(pod);
            // Separator line between pods
            if i + 1 < self.pods.len() {
                total += 1;
            }
        }
        total
    }

    pub fn scroll_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            // Adjust scroll so the pod's first line is visible
            let pod_line = self.line_index_of_pod(self.selected_index);
            if pod_line < self.scroll_offset {
                self.scroll_offset = pod_line;
            }
        }
    }

    pub fn scroll_down(&mut self, visible_lines: usize) {
        if self.selected_index < self.pods.len().saturating_sub(1) {
            self.selected_index += 1;
            // Adjust scroll so the pod's last line is visible
            let pod_line = self.line_index_of_pod(self.selected_index);
            let pod_lines = Self::pod_line_count(&self.pods[self.selected_index]);
            let pod_bottom = pod_line + pod_lines; // one past last line
            if pod_bottom > self.scroll_offset + visible_lines {
                self.scroll_offset = pod_bottom.saturating_sub(visible_lines);
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let border_style = if focused {
            self.styles.border_focused
        } else {
            self.styles.border_unfocused
        };

        let border_type = if focused {
            BorderType::Double
        } else {
            BorderType::Rounded
        };

        let title = if focused { " ● Pods " } else { "   Pods " };

        let title_style = if focused {
            self.styles.title.add_modifier(Modifier::BOLD)
        } else {
            self.styles.normal_text
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(border_style)
            .title(Span::styled(title, title_style));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        self.render_inner(frame, inner, focused);
    }

    /// Render pod list content into the given area (no outer border).
    /// Used when the outer block is drawn by the parent layout.
    pub fn render_inner(&self, frame: &mut Frame, inner: Rect, focused: bool) {
        if self.pods.is_empty() {
            let empty_msg = Paragraph::new(Line::from(Span::styled(
                "No pods running",
                self.styles.muted_text,
            )))
            .centered();
            frame.render_widget(empty_msg, inner);
            return;
        }

        let visible_lines = inner.height as usize;
        let available_width = inner.width as usize;

        // Calculate dynamic widths based on available space
        // Fixed elements: cursor(2) + " C"(2) + " M"(2) + scrollbar(1) = 7
        // Bar widths now include the label text (e.g., "500/1000m" = ~10 chars)
        let fixed_width = 7;
        let remaining = available_width.saturating_sub(fixed_width);

        // Distribute remaining space: 40% to name, 30% each to progress bars with labels
        let name_width = (remaining * 40 / 100).max(8);
        let bar_width = (remaining * 30 / 100).max(10);

        // Group pods by namespace
        let mut namespaces: Vec<String> = self
            .pods
            .iter()
            .map(|p| p.namespace.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        namespaces.sort();

        // Build lines with namespace grouping
        let mut text_lines: Vec<Line> = Vec::new();
        let mut current_ns = String::new();
        let mut line_idx = 0;

        for (idx, pod) in self.pods.iter().enumerate() {
            // Skip until we reach scroll offset
            if line_idx < self.scroll_offset {
                if pod.namespace != current_ns {
                    current_ns = pod.namespace.clone();
                    line_idx += 1; // namespace header
                }
                line_idx += Self::pod_line_count(pod);
                // Separator between pods
                if idx + 1 < self.pods.len() {
                    line_idx += 1;
                }
                continue;
            }

            // Stop if we've reached visible limit
            if text_lines.len() >= visible_lines {
                break;
            }

            // Add namespace header if namespace changed
            if pod.namespace != current_ns {
                current_ns = pod.namespace.clone();
                // Only add header if we have room
                if text_lines.len() < visible_lines {
                    let header_text = format!("── {} ──", current_ns);
                    text_lines.push(Line::from(Span::styled(
                        header_text,
                        self.styles.group_header,
                    )));
                }
                if text_lines.len() >= visible_lines {
                    break;
                }
            }

            let is_selected = idx == self.selected_index && focused;

            // Selection cursor
            let cursor = if is_selected { "▸ " } else { "  " };

            // Extract short name from k8s container name
            let short_name = self.extract_pod_name(&pod.name);

            let is_highlighted = self.highlighted_pods.contains(&pod.name);

            // Name style - color based on pod state, highlight if selected
            let name_style = match &pod.state {
                PodState::Failed { .. } => {
                    let mut s = self.styles.error_text;
                    if is_selected {
                        s = s.add_modifier(Modifier::BOLD);
                    }
                    s
                }
                PodState::Pulling { .. } => {
                    let mut s = self.styles.warning_text;
                    s = s.add_modifier(Modifier::SLOW_BLINK);
                    if is_selected {
                        s = s.add_modifier(Modifier::BOLD);
                    }
                    s
                }
                _ => {
                    if is_selected {
                        self.styles.selected
                    } else if is_highlighted {
                        Style::default()
                            .fg(self.styles.palette.highlight)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        self.styles.normal_text
                    }
                }
            };

            // Build lines based on pod state (may produce multiple lines for Pulling)
            let lines: Vec<Line> = match &pod.state {
                PodState::Running => {
                    // Running state: show CPU/RAM bars (existing behavior)
                    let has_cpu_limit = pod.has_cpu_limit();
                    let cpu_bar_percent = if has_cpu_limit {
                        pod.cpu_percent_of_limit()
                    } else {
                        pod.cpu_percent.min(100.0)
                    };
                    let cpu_base_style = if cpu_bar_percent > 80.0 {
                        self.styles.error_text
                    } else if cpu_bar_percent > 50.0 {
                        self.styles.warning_text
                    } else {
                        self.styles.success_text
                    };

                    let has_mem_limit = pod.has_memory_limit();
                    let mem_percent = pod.memory_percent();
                    let mem_base_style = if mem_percent > 80.0 {
                        self.styles.error_text
                    } else if mem_percent > 50.0 {
                        self.styles.warning_text
                    } else {
                        self.styles.success_text
                    };

                    let cpu_label = format_cpu_display(
                        pod.cpu_percent,
                        pod.cpu_limit_millicores,
                        has_cpu_limit,
                    );
                    let cpu_filled_style = Style::default()
                        .fg(self.styles.palette.background)
                        .bg(cpu_base_style.fg.unwrap_or(ratatui::style::Color::Green));
                    let cpu_empty_style = self.styles.muted_text;

                    let mem_label = format_memory_display(
                        pod.memory_used_mb,
                        pod.memory_limit_mb,
                        has_mem_limit,
                    );
                    let mem_filled_style = Style::default()
                        .fg(self.styles.palette.background)
                        .bg(mem_base_style.fg.unwrap_or(ratatui::style::Color::Green));
                    let mem_empty_style = self.styles.muted_text;

                    // Show arch mismatch warning icon before pod name
                    let (arch_prefix, name_w) = if pod.arch_mismatch {
                        ("\u{26a0} ", name_width.saturating_sub(2))
                    } else {
                        ("", name_width)
                    };

                    let mut line_spans = vec![Span::styled(
                        cursor,
                        if is_selected {
                            self.styles.warning_text
                        } else {
                            self.styles.muted_text
                        },
                    )];

                    if pod.arch_mismatch {
                        line_spans.push(Span::styled(arch_prefix, self.styles.warning_text));
                    }

                    line_spans.push(Span::styled(
                        format!(
                            "{:<width$}",
                            truncate_string(&short_name, name_w),
                            width = name_w
                        ),
                        name_style,
                    ));
                    line_spans.push(Span::styled(" C", self.styles.muted_text));

                    line_spans.extend(progress_bar_with_label(
                        cpu_bar_percent,
                        &cpu_label,
                        bar_width,
                        cpu_filled_style,
                        cpu_empty_style,
                    ));

                    line_spans.push(Span::styled(" M", self.styles.muted_text));

                    line_spans.extend(progress_bar_with_label(
                        mem_percent,
                        &mem_label,
                        bar_width,
                        mem_filled_style,
                        mem_empty_style,
                    ));

                    vec![Line::from(line_spans)]
                }
                PodState::Pulling {
                    containers,
                    started_at,
                } => {
                    let elapsed = started_at
                        .map(|t| {
                            let duration = Utc::now().signed_duration_since(t);
                            format_elapsed(duration)
                        })
                        .unwrap_or_else(|| "...".to_string());

                    let elapsed_display = format!(" {}", elapsed);

                    // Prefix: cursor(2) + name(name_width) + " "(1)
                    let prefix_width = name_width + 3;
                    let full_bar_width = available_width
                        .saturating_sub(prefix_width)
                        .saturating_sub(1); // scrollbar

                    let filled_style = Style::default().fg(self.styles.palette.background).bg(self
                        .styles
                        .info_text
                        .fg
                        .unwrap_or(ratatui::style::Color::Cyan));

                    let cursor_span = Span::styled(
                        cursor,
                        if is_selected {
                            self.styles.warning_text
                        } else {
                            self.styles.muted_text
                        },
                    );
                    let name_span = Span::styled(
                        format!(
                            "{:<width$}",
                            truncate_string(&short_name, name_width),
                            width = name_width
                        ),
                        name_style,
                    );

                    if containers.is_empty() {
                        let bar_w = full_bar_width.saturating_sub(elapsed_display.len());
                        let mut spans = vec![
                            cursor_span,
                            name_span,
                            Span::styled(" ", self.styles.muted_text),
                        ];
                        spans.extend(progress_bar_with_label(
                            0.0,
                            "pulling...",
                            bar_w,
                            filled_style,
                            self.styles.muted_text,
                        ));
                        spans.push(Span::styled(elapsed_display, self.styles.muted_text));
                        vec![Line::from(spans)]
                    } else {
                        let mut result_lines: Vec<Line> = Vec::new();
                        for (i, container) in containers.iter().enumerate() {
                            let is_first = i == 0;
                            let is_last = i == containers.len() - 1;

                            let mut spans: Vec<Span> = if is_first {
                                vec![
                                    cursor_span.clone(),
                                    name_span.clone(),
                                    Span::styled(" ", self.styles.muted_text),
                                ]
                            } else {
                                vec![Span::styled(
                                    " ".repeat(prefix_width),
                                    self.styles.muted_text,
                                )]
                            };

                            let bar_w = if is_first {
                                full_bar_width.saturating_sub(elapsed_display.len())
                            } else {
                                full_bar_width
                            };

                            // Use yellow bar fill for extracting phase
                            let bar_fill = if container.phase == PullPhase::Extracting {
                                Style::default().fg(self.styles.palette.background).bg(self
                                    .styles
                                    .warning_text
                                    .fg
                                    .unwrap_or(ratatui::style::Color::Yellow))
                            } else {
                                filled_style
                            };

                            // Three-way label: bytes progress, layer count, or fallback
                            if let Some(percent) = container.progress_percent {
                                let label = if container.total_bytes > 0 {
                                    format_bytes_progress(
                                        container.downloaded_bytes,
                                        container.total_bytes,
                                    )
                                } else {
                                    format!("{:.0}%", percent)
                                };
                                spans.extend(progress_bar_with_label(
                                    percent,
                                    &label,
                                    bar_w,
                                    bar_fill,
                                    self.styles.muted_text,
                                ));
                            } else if container.layers_total > 0 {
                                let label = format!(
                                    "{}/{} layers",
                                    container.layers_done, container.layers_total
                                );
                                spans.extend(progress_bar_with_label(
                                    0.0,
                                    &label,
                                    bar_w,
                                    bar_fill,
                                    self.styles.muted_text,
                                ));
                            } else {
                                spans.extend(progress_bar_with_label(
                                    0.0,
                                    "pulling...",
                                    bar_w,
                                    bar_fill,
                                    self.styles.muted_text,
                                ));
                            }

                            if is_first {
                                spans.push(Span::styled(
                                    elapsed_display.clone(),
                                    self.styles.muted_text,
                                ));
                            }

                            result_lines.push(Line::from(spans));

                            if text_lines.len() + result_lines.len() >= visible_lines && !is_last {
                                break;
                            }
                        }
                        result_lines
                    }
                }
                PodState::Waiting { reason } => {
                    // Waiting state: show reason in muted text
                    let status_width = bar_width * 2 + 4;
                    let status_text = format!("⏳ {}", reason);
                    let truncated_status = truncate_string(&status_text, status_width);

                    vec![Line::from(vec![
                        Span::styled(
                            cursor,
                            if is_selected {
                                self.styles.warning_text
                            } else {
                                self.styles.muted_text
                            },
                        ),
                        Span::styled(
                            format!(
                                "{:<width$}",
                                truncate_string(&short_name, name_width),
                                width = name_width
                            ),
                            name_style,
                        ),
                        Span::styled(" ", self.styles.muted_text),
                        Span::styled(
                            format!("{:<width$}", truncated_status, width = status_width),
                            self.styles.warning_text,
                        ),
                    ])]
                }
                PodState::Failed { reason } => {
                    // Failed state: show reason in error style
                    let status_width = bar_width * 2 + 4;
                    let status_text = format!("✗ {}", reason);
                    let truncated_status = truncate_string(&status_text, status_width);

                    vec![Line::from(vec![
                        Span::styled(
                            cursor,
                            if is_selected {
                                self.styles.warning_text
                            } else {
                                self.styles.muted_text
                            },
                        ),
                        Span::styled(
                            format!(
                                "{:<width$}",
                                truncate_string(&short_name, name_width),
                                width = name_width
                            ),
                            name_style,
                        ),
                        Span::styled(" ", self.styles.muted_text),
                        Span::styled(
                            format!("{:<width$}", truncated_status, width = status_width),
                            self.styles.error_text,
                        ),
                    ])]
                }
            };

            line_idx += lines.len();
            text_lines.extend(lines);

            // Add separator between pods
            if idx + 1 < self.pods.len() && text_lines.len() < visible_lines {
                let sep_width = available_width.saturating_sub(1); // -1 for scrollbar
                text_lines.push(Line::from(Span::styled(
                    "─".repeat(sep_width),
                    self.styles.border_unfocused,
                )));
                line_idx += 1;
            }
        }

        let paragraph = Paragraph::new(text_lines);
        frame.render_widget(paragraph, inner);

        // Render scrollbar if there are more lines than visible
        let total_lines = self.total_visual_lines();
        if total_lines > visible_lines {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut scrollbar_state = ScrollbarState::new(total_lines).position(self.scroll_offset);
            frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
        }
    }

    /// Extract a short readable pod name
    /// Input is already a pod name like "varnish-6d54999ccd-nzsf5" or "k3s-server"
    fn extract_pod_name(&self, pod_name: &str) -> String {
        // For k8s pods managed by a ReplicaSet, strip the generated suffixes:
        // Format: <deployment>-<replicaset-hash>-<pod-hash>
        // e.g., "varnish-6d54999ccd-nzsf5" -> "varnish"
        // e.g., "coredns-64fd4b4794-9h97w" -> "coredns"
        //
        // But keep meaningful name parts:
        // e.g., "pull-test-multi" -> "pull-test-multi" (no hash suffixes)
        // e.g., "k3s-server" -> "k3s-server"
        //
        // K8s hashes: pod-hash is 5 alphanumeric chars, replicaset-hash is 8-10 alphanumeric
        if let Some((prefix, pod_hash)) = pod_name.rsplit_once('-') {
            if is_k8s_hash(pod_hash) {
                if let Some((base, rs_hash)) = prefix.rsplit_once('-') {
                    if is_k8s_hash(rs_hash) {
                        return base.to_string();
                    }
                }
            }
        }
        pod_name.to_string()
    }
}

impl Default for PodStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a string looks like a K8s-generated hash suffix
/// ReplicaSet hashes: 8-10 lowercase alphanumeric (e.g., "6d54999ccd")
/// Pod hashes: 5 lowercase alphanumeric (e.g., "nzsf5")
fn is_k8s_hash(s: &str) -> bool {
    let len = s.len();
    (len == 5 || (8..=10).contains(&len))
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Truncate a string to a maximum length, adding ellipsis if needed
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..max_len - 3])
    } else {
        s[..max_len].to_string()
    }
}

/// Create a progress bar with label overlaid on it
/// Returns a vector of styled spans for the bar with text on top
/// The bar has a fixed width, with the label right-aligned on it
fn progress_bar_with_label(
    percent: f64,
    label: &str,
    width: usize,
    filled_style: ratatui::style::Style,
    empty_style: ratatui::style::Style,
) -> Vec<Span<'static>> {
    let clamped = percent.clamp(0.0, 100.0);
    let filled_count = ((clamped / 100.0) * width as f64).round() as usize;

    // Truncate label if longer than width, or pad if shorter (right-align)
    let display_label: String = if label.chars().count() > width {
        // Truncate from the left to keep the important right part (the limit)
        label.chars().skip(label.chars().count() - width).collect()
    } else {
        // Pad with spaces on the left
        format!("{:>width$}", label, width = width)
    };

    let label_chars: Vec<char> = display_label.chars().collect();

    let mut spans = Vec::new();

    for (i, ch) in label_chars.iter().enumerate().take(width) {
        let style = if i < filled_count {
            filled_style
        } else {
            empty_style
        };
        spans.push(Span::styled(ch.to_string(), style));
    }

    spans
}

/// Format bytes progress display (e.g., "12/48M")
fn format_bytes_progress(downloaded: u64, total: u64) -> String {
    let (dl_val, dl_unit) = format_bytes_value(downloaded);
    let (total_val, total_unit) = format_bytes_value(total);

    // If units match, show compact format
    if dl_unit == total_unit {
        format!("{:.0}/{:.0}{}", dl_val, total_val, total_unit)
    } else {
        format!("{:.0}{}/{:.0}{}", dl_val, dl_unit, total_val, total_unit)
    }
}

/// Convert bytes to appropriate unit (K, M, G)
fn format_bytes_value(bytes: u64) -> (f64, &'static str) {
    let bytes_f = bytes as f64;
    if bytes_f >= 1024.0 * 1024.0 * 1024.0 {
        (bytes_f / (1024.0 * 1024.0 * 1024.0), "G")
    } else if bytes_f >= 1024.0 * 1024.0 {
        (bytes_f / (1024.0 * 1024.0), "M")
    } else if bytes_f >= 1024.0 {
        (bytes_f / 1024.0, "K")
    } else {
        (bytes_f, "B")
    }
}

/// Format memory display based on whether limit exists
fn format_memory_display(used_mb: f64, limit_mb: f64, has_limit: bool) -> String {
    if has_limit {
        // Show used/limit format: "256/512M"
        let (used_val, used_unit) = format_memory_value(used_mb);
        let (limit_val, limit_unit) = format_memory_value(limit_mb);

        // If units match, show compact format
        if used_unit == limit_unit {
            format!("{:>3.0}/{:.0}{}", used_val, limit_val, limit_unit)
        } else {
            format!(
                "{:>3.0}{}/{:.0}{}",
                used_val, used_unit, limit_val, limit_unit
            )
        }
    } else {
        // Show usage with infinity symbol: "256M∞"
        let (val, unit) = format_memory_value(used_mb);
        format!("{:>4.0}{}∞", val, unit)
    }
}

/// Format CPU display based on whether limit exists
/// cpu_percent is 100% = 1 core usage (can be >100% for multi-core)
/// cpu_limit_millicores is in millicores (1000 = 1 core)
fn format_cpu_display(cpu_percent: f64, cpu_limit_millicores: f64, has_limit: bool) -> String {
    // Convert cpu_percent to millicores, with sanity cap
    let used_millicores = (cpu_percent * CPU_PERCENT_TO_MILLICORES).min(CPU_MAX_DISPLAY_MILLICORES);

    if has_limit {
        // Cap display at 2x limit (higher values are measurement errors)
        let display_used = used_millicores.min(cpu_limit_millicores * CPU_OVER_LIMIT_FACTOR);
        format!(
            "{}/{}",
            format_cpu_value(display_used),
            format_cpu_value(cpu_limit_millicores)
        )
    } else {
        // No limit - show usage with infinity symbol
        format!("{}∞", format_cpu_value(used_millicores))
    }
}

/// Format a CPU value in millicores to a display string
/// Only uses cores (e.g., "1c", "2c") for exact whole numbers
/// Otherwise always uses millicores (e.g., "1050m", "500m")
fn format_cpu_value(millicores: f64) -> String {
    // Check if it's an exact whole number of cores
    let cores = millicores / MILLICORES_PER_CORE;
    let is_whole_cores = cores >= 1.0 && (cores - cores.round()).abs() < 0.001;

    if is_whole_cores {
        format!("{:.0}c", cores.round())
    } else {
        format!("{:.0}m", millicores)
    }
}

/// Convert MB to appropriate unit (M or G)
fn format_memory_value(mb: f64) -> (f64, &'static str) {
    if mb >= 1024.0 {
        (mb / 1024.0, "G")
    } else {
        (mb, "M")
    }
}

/// Format elapsed time in a human-readable way
fn format_elapsed(duration: chrono::Duration) -> String {
    let secs = duration.num_seconds();
    if secs < 0 {
        return "0s".to_string();
    }
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Extract a short image name from a full image reference
/// e.g., "docker.io/library/nginx:1.21" -> "nginx:1.21"
#[allow(dead_code)]
fn extract_short_image(image: &str) -> String {
    // Remove registry prefix (everything before the last '/')
    let short = image.rsplit('/').next().unwrap_or(image);

    // If still too long, truncate the tag
    if short.len() > 20 {
        if let Some(colon_pos) = short.find(':') {
            let name = &short[..colon_pos];
            let tag = &short[colon_pos + 1..];
            if tag.len() > 8 {
                return format!("{}:{}...", name, &tag[..5]);
            }
        }
    }

    short.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_progress_same_unit() {
        // Both values in MB range
        let result = format_bytes_progress(50 * 1024 * 1024, 100 * 1024 * 1024);
        assert_eq!(result, "50/100M");
    }

    #[test]
    fn test_format_bytes_progress_different_units() {
        // Downloaded in KB, total in MB
        let result = format_bytes_progress(512 * 1024, 2 * 1024 * 1024);
        assert_eq!(result, "512K/2M");
    }

    #[test]
    fn test_format_bytes_value_bytes() {
        let (val, unit) = format_bytes_value(500);
        assert!((val - 500.0).abs() < 0.01);
        assert_eq!(unit, "B");
    }

    #[test]
    fn test_format_bytes_value_kilobytes() {
        let (val, unit) = format_bytes_value(2048);
        assert!((val - 2.0).abs() < 0.01);
        assert_eq!(unit, "K");
    }

    #[test]
    fn test_format_bytes_value_megabytes() {
        let (val, unit) = format_bytes_value(5 * 1024 * 1024);
        assert!((val - 5.0).abs() < 0.01);
        assert_eq!(unit, "M");
    }

    #[test]
    fn test_format_bytes_value_gigabytes() {
        let (val, unit) = format_bytes_value(2 * 1024 * 1024 * 1024);
        assert!((val - 2.0).abs() < 0.01);
        assert_eq!(unit, "G");
    }

    #[test]
    fn test_is_k8s_hash_pod_hash() {
        // 5-char lowercase alphanumeric = pod hash
        assert!(is_k8s_hash("nzsf5"));
        assert!(is_k8s_hash("9h97w"));
    }

    #[test]
    fn test_is_k8s_hash_replicaset_hash() {
        // 8-10 char lowercase alphanumeric = replicaset hash
        assert!(is_k8s_hash("6d54999ccd"));
        assert!(is_k8s_hash("64fd4b47"));
    }

    #[test]
    fn test_is_k8s_hash_not_hash() {
        // Normal name parts should not be detected as hashes
        assert!(!is_k8s_hash("server"));
        assert!(!is_k8s_hash("app"));
        assert!(!is_k8s_hash("test-pod"));
        // Too short
        assert!(!is_k8s_hash("ab"));
        // Contains uppercase
        assert!(!is_k8s_hash("ABCDE"));
    }

    #[test]
    fn test_is_k8s_hash_edge_cases() {
        // Exactly 5 chars but with special chars
        assert!(!is_k8s_hash("abc-d"));
        assert!(!is_k8s_hash("abc_d"));
        // 6 or 7 chars (not valid k8s hash length)
        assert!(!is_k8s_hash("abcdef"));
        assert!(!is_k8s_hash("abcdefg"));
        // Empty string
        assert!(!is_k8s_hash(""));
    }
}
