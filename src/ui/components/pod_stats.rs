//! Pod stats component showing running pods with Docker-based metrics

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
    Frame,
};

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

/// Stats for a single pod/container
#[derive(Debug, Clone)]
pub struct PodStat {
    pub name: String,
    pub namespace: String,
    pub cpu_percent: f64,
    pub cpu_limit_millicores: f64,
    pub memory_used_mb: f64,
    pub memory_limit_mb: f64,
    #[allow(dead_code)]
    pub status: String,
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
        }
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

    pub fn scroll_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            // Adjust scroll to keep selection visible
            if self.selected_index < self.scroll_offset {
                self.scroll_offset = self.selected_index;
            }
        }
    }

    pub fn scroll_down(&mut self, visible_lines: usize) {
        if self.selected_index < self.pods.len().saturating_sub(1) {
            self.selected_index += 1;
            // Adjust scroll to keep selection visible
            if self.selected_index >= self.scroll_offset + visible_lines {
                self.scroll_offset = self.selected_index - visible_lines + 1;
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
        let name_width = (remaining * 40 / 100).clamp(8, 35);
        let bar_width = (remaining * 30 / 100).clamp(10, 14);

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
                line_idx += 1; // pod line
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

            // CPU style - use percentage of limit if limit exists, otherwise raw percent
            let has_cpu_limit = pod.has_cpu_limit();
            let cpu_bar_percent = if has_cpu_limit {
                pod.cpu_percent_of_limit()
            } else {
                // For unlimited pods, cap bar display at 100%
                pod.cpu_percent.min(100.0)
            };
            let cpu_base_style = if cpu_bar_percent > 80.0 {
                self.styles.error_text
            } else if cpu_bar_percent > 50.0 {
                self.styles.warning_text
            } else {
                self.styles.success_text
            };

            // Memory style based on percentage
            let has_mem_limit = pod.has_memory_limit();
            let mem_percent = pod.memory_percent();
            let mem_base_style = if mem_percent > 80.0 {
                self.styles.error_text
            } else if mem_percent > 50.0 {
                self.styles.warning_text
            } else {
                self.styles.success_text
            };

            // Name style - highlight if selected
            let name_style = if is_selected {
                self.styles.selected
            } else {
                self.styles.normal_text
            };

            // Create CPU label text
            let cpu_label =
                format_cpu_display(pod.cpu_percent, pod.cpu_limit_millicores, has_cpu_limit);
            // Create filled/empty styles for CPU bar (filled = dark text on colored bg, empty = dimmed)
            let cpu_filled_style = Style::default()
                .fg(self.styles.palette.background)
                .bg(cpu_base_style.fg.unwrap_or(ratatui::style::Color::Green));
            let cpu_empty_style = self.styles.muted_text;

            // Create memory label text
            let mem_label =
                format_memory_display(pod.memory_used_mb, pod.memory_limit_mb, has_mem_limit);
            // Create filled/empty styles for memory bar
            let mem_filled_style = Style::default()
                .fg(self.styles.palette.background)
                .bg(mem_base_style.fg.unwrap_or(ratatui::style::Color::Green));
            let mem_empty_style = self.styles.muted_text;

            // Build the line with progress bars that have labels overlaid
            let mut line_spans = vec![
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
                Span::styled(" C", self.styles.muted_text),
            ];

            // Add CPU bar with label
            line_spans.extend(progress_bar_with_label(
                cpu_bar_percent,
                &cpu_label,
                bar_width,
                cpu_filled_style,
                cpu_empty_style,
            ));

            line_spans.push(Span::styled(" M", self.styles.muted_text));

            // Add memory bar with label
            line_spans.extend(progress_bar_with_label(
                mem_percent,
                &mem_label,
                bar_width,
                mem_filled_style,
                mem_empty_style,
            ));

            text_lines.push(Line::from(line_spans));

            line_idx += 1;
        }

        let paragraph = Paragraph::new(text_lines);
        frame.render_widget(paragraph, inner);

        // Render scrollbar if there are more pods than visible
        if self.pods.len() > visible_lines {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut scrollbar_state =
                ScrollbarState::new(self.pods.len()).position(self.scroll_offset);
            frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
        }
    }

    /// Extract a short readable pod name
    /// Input is already a pod name like "varnish-6d54999ccd-nzsf5" or "k3s-server"
    fn extract_pod_name(&self, pod_name: &str) -> String {
        // For k8s pods, try to extract the base name without replica hash
        // Format: <deployment>-<replicaset-hash>-<pod-hash>
        // e.g., "varnish-6d54999ccd-nzsf5" -> "varnish"
        // e.g., "coredns-64fd4b4794-9h97w" -> "coredns"
        if let Some((prefix, _)) = pod_name.rsplit_once('-') {
            if let Some((base, _)) = prefix.rsplit_once('-') {
                return base.to_string();
            }
        }
        // Return as-is for non-standard names (like k3s-server)
        pod_name.to_string()
    }
}

impl Default for PodStats {
    fn default() -> Self {
        Self::new()
    }
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
