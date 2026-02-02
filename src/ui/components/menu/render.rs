//! Menu rendering
//!
//! This module contains the render logic for the Menu component.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
    Frame,
};

use super::Menu;
use crate::cluster::IngressHealthStatus;

impl Menu {
    /// Get health style for status
    pub(super) fn health_style(&self, status: IngressHealthStatus) -> Style {
        match status {
            IngressHealthStatus::Healthy => self.styles.success_text,
            IngressHealthStatus::Warning => self.styles.warning_text,
            IngressHealthStatus::Error => self.styles.error_text,
            IngressHealthStatus::Unknown => self.styles.muted_text,
        }
    }

    /// Get breadcrumb path for current selection
    pub(super) fn get_breadcrumb_path(&self) -> Vec<String> {
        if let Some(item) = self.flat_items.get(self.selected_index) {
            let mut path = Vec::new();

            // Build path from item_path indices
            if !item.item_path.is_empty() {
                // First element is group index
                if let Some(group_idx) = item.item_path.first() {
                    if let Some(group) = self.items.get(*group_idx) {
                        path.push(group.name.clone());
                    }
                }

                // Add item name if it's not a group header
                if !item.is_group {
                    path.push(item.name.clone());
                }
            }

            path
        } else {
            Vec::new()
        }
    }

    /// Get the longest visible menu item width (for auto-width calculation)
    pub fn longest_item_width(&self) -> u16 {
        let mut max_width: u16 = 0;
        for item in &self.flat_items {
            // Calculate item width: cursor + indent + expand icon + icon + name
            let cursor_width: u16 = 2; // "▸ " or "  "
            let indent = (item.level * 2) as u16;
            let expand_icon: u16 = 2; // "▼ ", "▶ ", or "  "
            let icon_width = if !item.icon.is_empty() {
                item.icon.chars().count() as u16 + 1
            } else {
                0
            };
            let name_width = item.name.chars().count() as u16;
            let total = cursor_width + indent + expand_icon + icon_width + name_width;
            if total > max_width {
                max_width = total;
            }
        }
        max_width
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

        let title = if focused {
            " ● Commands "
        } else {
            "   Commands "
        };

        let title_style = if focused {
            self.styles.panel_title_focused
        } else {
            self.styles.panel_title_unfocused
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(border_style)
            .title(Span::styled(title, title_style));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible_height = inner.height as usize;

        // Build breadcrumb line or search input
        let mut lines: Vec<Line> = Vec::new();

        if self.search_mode {
            // Show search input
            let search_query_clone = self.search_query.clone();
            let match_text = if self.search_query.is_empty() {
                String::new()
            } else if self.filtered_indices.is_empty() {
                " (no matches)".to_string()
            } else {
                format!(" ({} matches)", self.filtered_indices.len())
            };
            lines.push(Line::from(vec![
                Span::styled("🔍 /", self.styles.warning_text),
                Span::styled(
                    search_query_clone,
                    self.styles.normal_text.add_modifier(Modifier::UNDERLINED),
                ),
                Span::styled(match_text, self.styles.muted_text),
            ]));
        } else {
            let breadcrumb_path = self.get_breadcrumb_path();
            if !breadcrumb_path.is_empty() {
                // Build breadcrumb as owned strings
                let mut breadcrumb_text = String::from("📍 ");
                for (i, segment) in breadcrumb_path.iter().enumerate() {
                    if i > 0 {
                        breadcrumb_text.push_str(" > ");
                    }
                    breadcrumb_text.push_str(segment);
                }
                lines.push(Line::from(Span::styled(breadcrumb_text, self.styles.title)));
            } else {
                // Empty breadcrumb line for consistent layout
                lines.push(Line::from(Span::styled(
                    "📍 Commands",
                    self.styles.muted_text,
                )));
            }
        }

        // Calculate how many lines the ingress section will take (help message is separate at bottom)
        let ingress_lines = if self.ingress_entries.is_empty() {
            0
        } else if self.ingress_expanded {
            // Separator + Header + (host + paths for each entry)
            let total_paths: usize = self
                .ingress_entries
                .iter()
                .map(|e| 1 + e.paths.len()) // 1 for host line + paths
                .sum();
            2 + total_paths
        } else {
            // Separator + header
            2
        };

        // Calculate how many lines the forwarded ports section will take
        let has_static_ports = !self.forwarded_ports.is_empty();
        let has_active_ports = !self.active_port_forwards.is_empty();
        let ports_lines = if !has_static_ports && !has_active_ports {
            0
        } else {
            // Separator + Header + static ports + (subheader + active ports if any)
            let mut count = 2; // separator + header
            if has_static_ports {
                count += 1 + self.forwarded_ports.len(); // subheader + ports
            }
            if has_active_ports {
                count += 1 + self.active_port_forwards.len(); // subheader + ports
            }
            count
        };

        // Available height for commands (leave room for breadcrumb, ingress and ports at bottom)
        let commands_height = visible_height
            .saturating_sub(ingress_lines)
            .saturating_sub(ports_lines)
            .saturating_sub(1);

        // Build command lines
        let command_lines: Vec<Line> = self
            .flat_items
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(commands_height)
            .map(|(idx, item)| {
                let is_selected = idx == self.selected_index;
                let is_match = self.search_query.is_empty() || self.filtered_indices.contains(&idx);

                // Selection cursor prefix
                let cursor = if is_selected { "▸ " } else { "  " };

                // Build indentation
                let indent = "  ".repeat(item.level);

                // Expansion indicator
                let expand_icon = if item.has_children {
                    if item.is_expanded {
                        "▼ "
                    } else {
                        "▶ "
                    }
                } else {
                    "  "
                };

                // Item icon (only for groups)
                let icon = if !item.icon.is_empty() {
                    format!("{} ", item.icon)
                } else {
                    String::new()
                };

                let text = format!("{}{}{}{}{}", cursor, indent, expand_icon, icon, item.name);

                // Dim non-matching items when filtering
                let style = if !is_match {
                    self.styles.muted_text
                } else if is_selected {
                    self.styles.selected
                } else if item.is_group {
                    self.styles.group_header
                } else {
                    self.styles.normal_text
                };

                Line::from(Span::styled(text, style))
            })
            .collect();

        // Add command lines to main lines
        lines.extend(command_lines);

        // Render ingress section
        self.render_ingress_section(&mut lines, &inner);

        // Render forwarded ports section
        self.render_ports_section(&mut lines, &inner);

        // Render missing hosts help message
        self.render_missing_hosts_help(&mut lines, visible_height);

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);

        // Render scrollbar if there are more items than visible
        if self.flat_items.len() > commands_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut scrollbar_state =
                ScrollbarState::new(self.flat_items.len()).position(self.scroll_offset);
            frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
        }
    }

    /// Render ingress section at the bottom
    fn render_ingress_section(&self, lines: &mut Vec<Line>, inner: &Rect) {
        if self.ingress_entries.is_empty() {
            return;
        }

        // Add separator line
        lines.push(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            self.styles.muted_text,
        )));

        // Add ingress header
        let expand_icon = if self.ingress_expanded { "▼" } else { "▶" };
        lines.push(Line::from(Span::styled(
            format!("{} 🌐 Ingress", expand_icon),
            self.styles.group_header,
        )));

        // Add ingress entries with paths as tree
        if self.ingress_expanded {
            for (entry_idx, entry) in self.ingress_entries.iter().enumerate() {
                // Check if this host is missing from /etc/hosts
                let is_missing = self.missing_hosts.contains(&entry.host);

                // Add host line with blinking (H) if missing from /etc/hosts
                if is_missing {
                    let h_indicator = if self.blink_visible {
                        Span::styled(
                            " (H)",
                            self.styles.warning_text.add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Span::styled("    ", self.styles.warning_text) // Same width, invisible
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  📍 {}", entry.host), self.styles.group_header),
                        h_indicator,
                    ]));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("  📍 {}", entry.host),
                        self.styles.group_header,
                    )));
                }

                // Add paths under host
                let path_count = entry.paths.len();
                for (path_idx, path) in entry.paths.iter().enumerate() {
                    let key = format!("{}|{}", entry.host, path);
                    let health = self
                        .ingress_health
                        .get(&key)
                        .copied()
                        .unwrap_or(IngressHealthStatus::Unknown);
                    let health_style = self.health_style(health);

                    // Tree branch character
                    let branch = if path_idx == path_count - 1 {
                        "└"
                    } else {
                        "├"
                    };

                    // Check if this path is selected
                    let is_path_selected = self.ingress_selected
                        && self.selected_ingress_entry == entry_idx
                        && self.selected_ingress_path == path_idx;

                    let cursor = if is_path_selected { "▸" } else { " " };
                    let path_style = if is_path_selected {
                        self.styles.selected
                    } else {
                        self.styles.normal_text
                    };

                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {}{} ", cursor, branch),
                            if is_path_selected {
                                self.styles.warning_text
                            } else {
                                self.styles.muted_text
                            },
                        ),
                        Span::styled(health.dot(), health_style),
                        Span::styled(format!(" {}", path), path_style),
                    ]));
                }
            }
        }
    }

    /// Render forwarded ports section
    fn render_ports_section(&self, lines: &mut Vec<Line>, inner: &Rect) {
        if self.forwarded_ports.is_empty() && self.active_port_forwards.is_empty() {
            return;
        }

        // Add separator line
        lines.push(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            self.styles.muted_text,
        )));

        // Add ports header
        lines.push(Line::from(Span::styled(
            "🔌 Forwarded Ports",
            self.styles.group_header,
        )));

        // Add static port entries (from k3s container)
        if !self.forwarded_ports.is_empty() {
            lines.push(Line::from(Span::styled(
                "  📦 Container",
                self.styles.muted_text,
            )));
            for (host_port, container_port) in &self.forwarded_ports {
                let port_text = format!("    {} → k3s:{}", host_port, container_port);
                lines.push(Line::from(Span::styled(port_text, self.styles.normal_text)));
            }
        }

        // Add active port forwards (from kubectl port-forward, etc.)
        if !self.active_port_forwards.is_empty() {
            lines.push(Line::from(Span::styled(
                "  🔀 Active",
                self.styles.muted_text,
            )));
            for pf in &self.active_port_forwards {
                let port_text =
                    format!("    {} → {} ({})", pf.local_port, pf.remote_port, pf.target);
                lines.push(Line::from(Span::styled(
                    port_text,
                    self.styles.success_text,
                )));
            }
        }
    }

    /// Render missing hosts help message at the bottom
    fn render_missing_hosts_help(&self, lines: &mut Vec<Line>, visible_height: usize) {
        if self.missing_hosts.is_empty() {
            return;
        }

        let count = self.missing_hosts.len();
        // Calculate remaining space and add empty lines to push help to bottom
        let current_lines = lines.len();
        let help_lines_needed = 1;
        if current_lines + help_lines_needed < visible_height {
            let empty_lines = visible_height - current_lines - help_lines_needed;
            for _ in 0..empty_lines {
                lines.push(Line::from(""));
            }
        }

        let hosts_text = if count == 1 { "host" } else { "hosts" };
        lines.push(Line::from(vec![
            Span::styled("  ⚠ ", self.styles.warning_text),
            Span::styled(
                format!("{} {} missing ", count, hosts_text),
                self.styles.warning_text,
            ),
            Span::styled("│ Press ", self.styles.muted_text),
            Span::styled("H", self.styles.warning_text.add_modifier(Modifier::BOLD)),
            Span::styled(" to add", self.styles.muted_text),
        ]));
    }
}
