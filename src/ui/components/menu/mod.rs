//! Hierarchical command menu component
//!
//! This module provides the Menu component for displaying commands,
//! ingress entries, and forwarded ports in a hierarchical tree structure.

mod render;

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
    pub(super) items: Vec<CommandGroup>,
    pub(super) flat_items: Vec<FlatMenuItem>,
    expanded: Vec<bool>, // Track expanded state for groups
    pub(super) selected_index: usize,
    pub(super) scroll_offset: usize,
    pub(super) styles: Styles,
    // Ingress entries with paths and health status
    pub(super) ingress_entries: Vec<IngressEntry>,
    pub(super) ingress_health: HashMap<String, IngressHealthStatus>, // Key: "host|path"
    pub(super) ingress_expanded: bool,
    // Hosts that are missing from /etc/hosts (should blink)
    pub(super) missing_hosts: HashSet<String>,
    // Manual blink state (toggled by app loop)
    pub(super) blink_visible: bool,
    // Forwarded ports from config (host_port, container_port)
    pub(super) forwarded_ports: Vec<(u16, u16)>,
    // Active port forwards from kubectl port-forward
    pub(super) active_port_forwards: Vec<ActivePortForward>,
    // Search/filter state
    pub(super) search_mode: bool,
    pub(super) search_query: String,
    pub(super) filtered_indices: Vec<usize>, // Indices into flat_items that match filter
    // Ingress selection state
    pub(super) ingress_selected: bool,
    pub(super) selected_ingress_entry: usize, // Index into ingress_entries
    pub(super) selected_ingress_path: usize,  // Index into paths within the entry
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

    // === State Setters ===

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

    // === Getters ===

    /// Get ingress entries (for health checking)
    pub fn get_ingress_entries(&self) -> &[IngressEntry] {
        &self.ingress_entries
    }

    // === Search Mode ===

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

    // === Ingress Selection ===

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

    // === Config Building ===

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

    // === Navigation ===

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

    // === Selection ===

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
}

impl Default for Menu {
    fn default() -> Self {
        Self::new()
    }
}
