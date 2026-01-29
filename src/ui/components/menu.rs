use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
    Frame,
};
use std::collections::{HashMap, HashSet};

use crate::cluster::{IngressEntry, IngressHealthStatus};
use crate::config::{CommandEntry, CommandGroup, Config};
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// A flattened menu item for display
#[derive(Debug, Clone)]
pub struct FlatMenuItem {
    pub name: String,
    pub icon: String,
    pub level: usize,
    pub is_group: bool,
    pub is_expanded: bool,
    pub has_children: bool,
    pub command: Option<CommandEntry>,
    pub group_index: usize,
    #[allow(dead_code)]
    pub item_path: Vec<usize>,
}

/// Active port forward from kubectl port-forward or similar
#[derive(Debug, Clone)]
pub struct ActivePortForward {
    pub local_port: u16,
    pub remote_port: u16,
    pub target: String, // e.g., "pod/my-pod", "svc/my-service"
}

/// Hierarchical command menu component
pub struct Menu {
    items: Vec<CommandGroup>,
    flat_items: Vec<FlatMenuItem>,
    expanded: Vec<bool>, // Track expanded state for groups
    selected_index: usize,
    scroll_offset: usize,
    styles: Styles,
    // Ingress entries with paths and health status
    ingress_entries: Vec<IngressEntry>,
    ingress_health: HashMap<String, IngressHealthStatus>, // Key: "host|path"
    ingress_expanded: bool,
    // Hosts that are missing from /etc/hosts (should blink)
    missing_hosts: HashSet<String>,
    // Manual blink state (toggled by app loop)
    blink_visible: bool,
    // Forwarded ports from config (host_port, container_port)
    forwarded_ports: Vec<(u16, u16)>,
    // Active port forwards from kubectl port-forward
    active_port_forwards: Vec<ActivePortForward>,
    // Search/filter state
    search_mode: bool,
    search_query: String,
    filtered_indices: Vec<usize>, // Indices into flat_items that match filter
    // Ingress selection state
    ingress_selected: bool,
    selected_ingress_entry: usize, // Index into ingress_entries
    selected_ingress_path: usize,  // Index into paths within the entry
}

impl Menu {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            items: Vec::new(),
            flat_items: Vec::new(),
            expanded: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            styles: Styles::from_theme(theme),
            ingress_entries: Vec::new(),
            ingress_health: HashMap::new(),
            ingress_expanded: true,
            missing_hosts: HashSet::new(),
            blink_visible: true,
            forwarded_ports: Vec::new(),
            active_port_forwards: Vec::new(),
            search_mode: false,
            search_query: String::new(),
            filtered_indices: Vec::new(),
            ingress_selected: false,
            selected_ingress_entry: 0,
            selected_ingress_path: 0,
        }
    }

    /// Toggle blink state (call from app loop ~every 500ms)
    pub fn toggle_blink(&mut self) {
        self.blink_visible = !self.blink_visible;
    }

    /// Update ingress entries list
    pub fn set_ingress_entries(&mut self, entries: Vec<IngressEntry>) {
        self.ingress_entries = entries;
    }

    /// Update ingress health status (key format: "host|path")
    pub fn set_ingress_health(&mut self, health: HashMap<String, IngressHealthStatus>) {
        self.ingress_health = health;
    }

    /// Update missing hosts (hosts not in /etc/hosts)
    pub fn set_missing_hosts(&mut self, missing: HashSet<String>) {
        self.missing_hosts = missing;
    }

    /// Update forwarded ports list
    pub fn set_forwarded_ports(&mut self, ports: Vec<(u16, u16)>) {
        self.forwarded_ports = ports;
    }

    /// Update active port forwards (from kubectl port-forward, etc.)
    pub fn set_active_port_forwards(&mut self, forwards: Vec<ActivePortForward>) {
        self.active_port_forwards = forwards;
    }

    /// Get ingress entries (for health checking)
    pub fn get_ingress_entries(&self) -> &[IngressEntry] {
        &self.ingress_entries
    }

    /// Check if in search mode
    pub fn is_search_mode(&self) -> bool {
        self.search_mode
    }

    /// Get search query
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Enter search mode
    pub fn enter_search_mode(&mut self) {
        self.search_mode = true;
        self.search_query.clear();
        self.update_filter();
    }

    /// Exit search mode and clear filter
    pub fn exit_search_mode(&mut self) {
        self.search_mode = false;
        self.search_query.clear();
        self.filtered_indices.clear();
    }

    /// Handle character input in search mode
    pub fn search_handle_char(&mut self, c: char) {
        self.search_query.push(c);
        self.update_filter();
    }

    /// Handle backspace in search mode
    pub fn search_handle_backspace(&mut self) {
        self.search_query.pop();
        self.update_filter();
    }

    /// Update filtered indices based on search query
    fn update_filter(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_indices.clear();
            return;
        }

        let query_lower = self.search_query.to_lowercase();
        self.filtered_indices = self
            .flat_items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.name.to_lowercase().contains(&query_lower))
            .map(|(i, _)| i)
            .collect();

        // Select first match if any
        if !self.filtered_indices.is_empty()
            && !self.filtered_indices.contains(&self.selected_index)
        {
            self.selected_index = self.filtered_indices[0];
        }
    }

    /// Get the number of matches (for display)
    #[allow(dead_code)]
    pub fn match_count(&self) -> usize {
        if self.search_query.is_empty() {
            self.flat_items.len()
        } else {
            self.filtered_indices.len()
        }
    }

    /// Check if ingress is currently selected
    #[allow(dead_code)]
    pub fn is_ingress_selected(&self) -> bool {
        self.ingress_selected
    }

    /// Select the ingress section (called when scrolling past commands)
    pub fn select_ingress(&mut self) {
        if !self.ingress_entries.is_empty() {
            self.ingress_selected = true;
            self.selected_ingress_entry = 0;
            self.selected_ingress_path = 0;
        }
    }

    /// Deselect ingress and return to commands
    #[allow(dead_code)]
    pub fn deselect_ingress(&mut self) {
        self.ingress_selected = false;
    }

    /// Move up in ingress selection
    pub fn ingress_move_up(&mut self) {
        if self.selected_ingress_path > 0 {
            self.selected_ingress_path -= 1;
        } else if self.selected_ingress_entry > 0 {
            self.selected_ingress_entry -= 1;
            let paths_count = self
                .ingress_entries
                .get(self.selected_ingress_entry)
                .map(|e| e.paths.len())
                .unwrap_or(0);
            self.selected_ingress_path = paths_count.saturating_sub(1);
        } else {
            // Move back to commands
            self.ingress_selected = false;
            self.selected_index = self.flat_items.len().saturating_sub(1);
        }
    }

    /// Move down in ingress selection
    pub fn ingress_move_down(&mut self) {
        let current_paths = self
            .ingress_entries
            .get(self.selected_ingress_entry)
            .map(|e| e.paths.len())
            .unwrap_or(0);

        if self.selected_ingress_path < current_paths.saturating_sub(1) {
            self.selected_ingress_path += 1;
        } else if self.selected_ingress_entry < self.ingress_entries.len().saturating_sub(1) {
            self.selected_ingress_entry += 1;
            self.selected_ingress_path = 0;
        }
        // At the end, stay at last item
    }

    /// Get the currently selected ingress URL (if any)
    pub fn selected_ingress_url(&self) -> Option<String> {
        if !self.ingress_selected {
            return None;
        }
        let entry = self.ingress_entries.get(self.selected_ingress_entry)?;
        let path = entry.paths.get(self.selected_ingress_path)?;
        Some(format!("http://{}{}", entry.host, path))
    }

    /// Get the selected ingress host and path (for status bar display)
    pub fn selected_ingress_info(&self) -> Option<(String, String)> {
        if !self.ingress_selected {
            return None;
        }
        let entry = self.ingress_entries.get(self.selected_ingress_entry)?;
        let path = entry.paths.get(self.selected_ingress_path)?;
        Some((entry.host.clone(), path.clone()))
    }

    /// Get health style for status
    fn health_style(&self, status: IngressHealthStatus) -> Style {
        match status {
            IngressHealthStatus::Healthy => self.styles.success_text,
            IngressHealthStatus::Warning => self.styles.warning_text,
            IngressHealthStatus::Error => self.styles.error_text,
            IngressHealthStatus::Unknown => self.styles.muted_text,
        }
    }

    /// Build menu from config
    pub fn build_from_config(&mut self, config: &Config) {
        self.items = config.commands.clone();

        // Initialize expanded state - all groups start expanded
        self.expanded = vec![true; self.items.len()];

        self.rebuild_flat_items();
    }

    /// Rebuild the flattened item list based on current expansion state
    fn rebuild_flat_items(&mut self) {
        self.flat_items.clear();

        // Collect data first to avoid borrow issues
        let items_data: Vec<(String, String, Vec<CommandEntry>, bool)> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, group)| {
                let is_expanded = self.expanded.get(idx).copied().unwrap_or(true);
                (
                    group.name.clone(),
                    group.icon.clone(),
                    group.commands.clone(),
                    is_expanded,
                )
            })
            .collect();

        for (group_idx, (name, icon, commands, is_expanded)) in items_data.into_iter().enumerate() {
            // Add group header
            self.flat_items.push(FlatMenuItem {
                name,
                icon,
                level: 0,
                is_group: true,
                is_expanded,
                has_children: !commands.is_empty(),
                command: None,
                group_index: group_idx,
                item_path: vec![group_idx],
            });

            // If group is expanded, add its children
            if is_expanded {
                self.flatten_entries(commands, 1, group_idx, vec![group_idx]);
            }
        }

        // Ensure selected index is valid
        if self.selected_index >= self.flat_items.len() {
            self.selected_index = self.flat_items.len().saturating_sub(1);
        }
    }

    fn flatten_entries(
        &mut self,
        entries: Vec<CommandEntry>,
        level: usize,
        group_idx: usize,
        parent_path: Vec<usize>,
    ) {
        for (idx, entry) in entries.into_iter().enumerate() {
            let mut path = parent_path.clone();
            path.push(idx);

            let has_children = !entry.commands.is_empty();
            let is_expanded = has_children; // Nested items always expanded for simplicity
            let children = entry.commands.clone();

            self.flat_items.push(FlatMenuItem {
                name: entry.name.clone(),
                icon: String::new(),
                level,
                is_group: false,
                is_expanded,
                has_children,
                command: if entry.exec.is_some() {
                    Some(entry)
                } else {
                    None
                },
                group_index: group_idx,
                item_path: path.clone(),
            });

            // Recurse if has children
            if has_children && is_expanded {
                self.flatten_entries(children, level + 1, group_idx, path);
            }
        }
    }

    pub fn move_up(&mut self) {
        // If in ingress selection, handle separately
        if self.ingress_selected {
            self.ingress_move_up();
            return;
        }

        if self.search_mode && !self.filtered_indices.is_empty() {
            // Find current position in filtered list and move to previous
            if let Some(pos) = self
                .filtered_indices
                .iter()
                .position(|&i| i == self.selected_index)
            {
                if pos > 0 {
                    self.selected_index = self.filtered_indices[pos - 1];
                }
            } else if !self.filtered_indices.is_empty() {
                self.selected_index = self.filtered_indices[0];
            }
        } else if self.selected_index > 0 {
            self.selected_index -= 1;
        }
        self.adjust_scroll();
    }

    pub fn move_down(&mut self) {
        // If in ingress selection, handle separately
        if self.ingress_selected {
            self.ingress_move_down();
            return;
        }

        if self.search_mode && !self.filtered_indices.is_empty() {
            // Find current position in filtered list and move to next
            if let Some(pos) = self
                .filtered_indices
                .iter()
                .position(|&i| i == self.selected_index)
            {
                if pos < self.filtered_indices.len() - 1 {
                    self.selected_index = self.filtered_indices[pos + 1];
                }
            } else if !self.filtered_indices.is_empty() {
                self.selected_index = self.filtered_indices[0];
            }
        } else if self.selected_index < self.flat_items.len().saturating_sub(1) {
            self.selected_index += 1;
        } else if !self.ingress_entries.is_empty() && !self.search_mode {
            // At end of commands, transition to ingress
            self.select_ingress();
        }
        self.adjust_scroll();
    }

    fn adjust_scroll(&mut self) {
        // Will be called with visible height during render
    }

    #[allow(dead_code)]
    pub fn adjust_scroll_for_height(&mut self, visible_height: usize) {
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }

    /// Toggle expansion of current item
    pub fn toggle(&mut self) {
        if let Some(item) = self.flat_items.get(self.selected_index) {
            if item.is_group && item.has_children {
                let group_idx = item.group_index;
                if let Some(exp) = self.expanded.get_mut(group_idx) {
                    *exp = !*exp;
                }
                self.rebuild_flat_items();
            }
        }
    }

    /// Collapse current item or parent
    pub fn collapse(&mut self) {
        if let Some(item) = self.flat_items.get(self.selected_index) {
            if item.is_group && item.is_expanded {
                self.toggle();
            }
        }
    }

    /// Expand current item
    pub fn expand(&mut self) {
        if let Some(item) = self.flat_items.get(self.selected_index) {
            if item.is_group && !item.is_expanded {
                self.toggle();
            }
        }
    }

    /// Get the currently selected command (if it's an executable command)
    #[allow(dead_code)]
    pub fn selected_command(&self) -> Option<&CommandEntry> {
        self.flat_items
            .get(self.selected_index)
            .and_then(|item| item.command.as_ref())
    }

    /// Get selected item
    pub fn selected_item(&self) -> Option<&FlatMenuItem> {
        self.flat_items.get(self.selected_index)
    }

    /// Select item at the given row (for mouse click handling)
    /// Returns true if a valid item was selected
    pub fn select_at_row(&mut self, row: usize) -> bool {
        // Account for breadcrumb line
        let adjusted_row = row.saturating_sub(1);
        let target_index = self.scroll_offset + adjusted_row;
        if target_index < self.flat_items.len() {
            self.selected_index = target_index;
            true
        } else {
            false
        }
    }

    /// Get breadcrumb path for current selection
    fn get_breadcrumb_path(&self) -> Vec<String> {
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

        // Add ingress section at the bottom if there are ingresses
        if !self.ingress_entries.is_empty() {
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

        // Add forwarded ports section at the bottom if there are any ports
        if !self.forwarded_ports.is_empty() || !self.active_port_forwards.is_empty() {
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

        // Add help message at the very bottom of the block if there are missing hosts
        if !self.missing_hosts.is_empty() {
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
}

impl Default for Menu {
    fn default() -> Self {
        Self::new()
    }
}
