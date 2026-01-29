use std::collections::VecDeque;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::commands::PaletteCommandId;
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

const MAX_RECENT_COMMANDS: usize = 5;

/// A command entry in the palette
#[derive(Debug, Clone)]
pub struct PaletteCommand {
    pub id: PaletteCommandId,
    pub name: String,
    pub shortcut: Option<String>,
    pub category: CommandCategory,
    pub description: Option<String>,
}

/// Command categories
#[derive(Debug, Clone, PartialEq)]
pub enum CommandCategory {
    Cluster,
    Navigation,
    Application,
    Custom(String),
}

impl CommandCategory {
    fn as_str(&self) -> &str {
        match self {
            CommandCategory::Cluster => "Cluster",
            CommandCategory::Navigation => "Navigation",
            CommandCategory::Application => "App",
            CommandCategory::Custom(name) => name.as_str(),
        }
    }

    /// Get the appropriate style for this category's badge
    fn badge_style(&self, styles: &Styles) -> ratatui::style::Style {
        match self {
            CommandCategory::Cluster => styles.warning_text, // amber/orange
            CommandCategory::Navigation => styles.success_text, // green
            CommandCategory::Application => styles.primary,  // primary color
            CommandCategory::Custom(_) => styles.muted_text, // muted
        }
    }
}

/// Command palette for fuzzy searching and executing commands
pub struct CommandPalette {
    styles: Styles,
    query: String,
    cursor_pos: usize,
    commands: Vec<PaletteCommand>,
    filtered: Vec<usize>, // Indices into commands
    selected_index: usize,
    recent_commands: VecDeque<usize>, // Indices into commands, most recent first
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        let commands = vec![
            // Cluster actions
            PaletteCommand {
                id: PaletteCommandId::ClusterStart,
                name: "Start Cluster".to_string(),
                shortcut: None,
                category: CommandCategory::Cluster,
                description: Some("Start the k3s cluster container".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::ClusterStop,
                name: "Stop Cluster".to_string(),
                shortcut: None,
                category: CommandCategory::Cluster,
                description: Some("Stop the k3s cluster container".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::ClusterRestart,
                name: "Restart Cluster".to_string(),
                shortcut: None,
                category: CommandCategory::Cluster,
                description: Some("Stop and start the cluster".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::ClusterDestroy,
                name: "Destroy Cluster".to_string(),
                shortcut: None,
                category: CommandCategory::Cluster,
                description: Some("Permanently delete the cluster and all data".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::ClusterInfo,
                name: "Cluster Info".to_string(),
                shortcut: None,
                category: CommandCategory::Cluster,
                description: Some("Show cluster status and configuration".to_string()),
            },
            // Application commands
            PaletteCommand {
                id: PaletteCommandId::AppRefresh,
                name: "Refresh All".to_string(),
                shortcut: Some("r".to_string()),
                category: CommandCategory::Application,
                description: Some("Refresh ingress, hosts, and port forwards".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::AppUpdateHosts,
                name: "Update /etc/hosts".to_string(),
                shortcut: Some("H".to_string()),
                category: CommandCategory::Application,
                description: Some("Add missing ingress hosts to /etc/hosts".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::AppHelp,
                name: "Show Help".to_string(),
                shortcut: Some("?".to_string()),
                category: CommandCategory::Application,
                description: Some("Display keyboard shortcuts and help".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::AppQuit,
                name: "Quit Application".to_string(),
                shortcut: Some("q".to_string()),
                category: CommandCategory::Application,
                description: Some("Exit the application".to_string()),
            },
            // Navigation commands
            PaletteCommand {
                id: PaletteCommandId::NavFocusMenu,
                name: "Focus Menu".to_string(),
                shortcut: Some("Tab".to_string()),
                category: CommandCategory::Navigation,
                description: Some("Switch focus to the command menu".to_string()),
            },
            PaletteCommand {
                id: PaletteCommandId::NavFocusActions,
                name: "Focus Action Bar".to_string(),
                shortcut: Some("Tab".to_string()),
                category: CommandCategory::Navigation,
                description: Some("Switch focus to the action bar".to_string()),
            },
        ];

        let filtered: Vec<usize> = (0..commands.len()).collect();

        Self {
            styles: Styles::from_theme(theme),
            query: String::new(),
            cursor_pos: 0,
            commands,
            filtered,
            selected_index: 0,
            recent_commands: VecDeque::with_capacity(MAX_RECENT_COMMANDS),
        }
    }

    /// Load custom commands from config command groups
    pub fn load_custom_commands(&mut self, command_groups: &[crate::config::CommandGroup]) {
        for group in command_groups {
            self.add_commands_from_group(&group.name, &group.commands);
        }
        // Update filtered to include new commands
        self.filtered = (0..self.commands.len()).collect();
    }

    /// Recursively add commands from a group/subgroup
    fn add_commands_from_group(
        &mut self,
        parent_path: &str,
        entries: &[crate::config::CommandEntry],
    ) {
        for entry in entries {
            let path = format!("{}/{}", parent_path, entry.name);

            if entry.exec.is_some() {
                // This is an executable command
                self.commands.push(PaletteCommand {
                    id: PaletteCommandId::Custom(path.clone()),
                    name: entry.name.clone(),
                    shortcut: None,
                    category: CommandCategory::Custom(parent_path.to_string()),
                    description: entry.description.clone(),
                });
            }

            // Recursively add nested commands
            if !entry.commands.is_empty() {
                self.add_commands_from_group(&path, &entry.commands);
            }
        }
    }

    /// Reset the palette state
    pub fn reset(&mut self) {
        self.query.clear();
        self.cursor_pos = 0;
        self.filtered = (0..self.commands.len()).collect();
        self.selected_index = 0;
    }

    /// Handle character input
    pub fn handle_char(&mut self, c: char) {
        self.query.insert(self.cursor_pos, c);
        self.cursor_pos += 1;
        self.filter();
    }

    /// Handle backspace
    pub fn handle_backspace(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            self.query.remove(self.cursor_pos);
            self.filter();
        }
    }

    /// Move selection up
    pub fn move_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    /// Move selection down
    pub fn move_down(&mut self) {
        let max_index = self.total_selectable_items().saturating_sub(1);
        if self.selected_index < max_index {
            self.selected_index += 1;
        }
    }

    /// Get total number of selectable items
    fn total_selectable_items(&self) -> usize {
        if self.query.is_empty() && !self.recent_commands.is_empty() {
            // Recent commands + all filtered commands
            self.recent_commands.len() + self.filtered.len()
        } else {
            self.filtered.len()
        }
    }

    /// Get the selected command
    pub fn selected_command(&self) -> Option<&PaletteCommand> {
        // When showing recent commands section
        if self.query.is_empty() && !self.recent_commands.is_empty() {
            if self.selected_index < self.recent_commands.len() {
                // Selected from recent commands
                self.recent_commands
                    .get(self.selected_index)
                    .and_then(|&idx| self.commands.get(idx))
            } else {
                // Selected from all commands (offset by recent commands count)
                let adjusted_index = self.selected_index - self.recent_commands.len();
                self.filtered
                    .get(adjusted_index)
                    .and_then(|&idx| self.commands.get(idx))
            }
        } else {
            self.filtered
                .get(self.selected_index)
                .and_then(|&idx| self.commands.get(idx))
        }
    }

    /// Record a command execution to the recent list
    pub fn record_execution(&mut self, cmd_id: &PaletteCommandId) {
        // Find the command index
        if let Some(idx) = self.commands.iter().position(|c| &c.id == cmd_id) {
            // Remove if already in recent list (to move to front)
            self.recent_commands.retain(|&i| i != idx);
            // Add to front
            self.recent_commands.push_front(idx);
            // Trim to max size
            while self.recent_commands.len() > MAX_RECENT_COMMANDS {
                self.recent_commands.pop_back();
            }
        }
    }

    /// Filter commands based on query
    fn filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.commands.len()).collect();
        } else {
            let query_lower = self.query.to_lowercase();
            self.filtered = self
                .commands
                .iter()
                .enumerate()
                .filter(|(_, cmd)| {
                    cmd.name.to_lowercase().contains(&query_lower)
                        || cmd.id.as_str().to_lowercase().contains(&query_lower)
                })
                .map(|(i, _)| i)
                .collect();
        }

        // Reset selection if out of bounds
        let max_items = self.total_selectable_items();
        if self.selected_index >= max_items {
            self.selected_index = 0;
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Create a centered popup
        let popup_area = centered_rect(50, 50, area);

        // Clear background
        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(" Command Palette ");

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        // Split into input, results, and description
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Input line
                Constraint::Min(0),    // Results
                Constraint::Length(2), // Description line
            ])
            .split(inner);

        // Render input line
        let input_line = Line::from(vec![
            Span::styled("> ", self.styles.warning_text),
            Span::styled(
                &self.query,
                self.styles.normal_text.add_modifier(Modifier::UNDERLINED),
            ),
        ]);
        let input = Paragraph::new(input_line);
        frame.render_widget(input, chunks[0]);

        // Show cursor
        frame.set_cursor_position((chunks[0].x + 2 + self.cursor_pos as u16, chunks[0].y));

        // Build results list
        let visible_height = chunks[1].height as usize;
        let mut results: Vec<Line> = Vec::new();
        let mut display_index = 0;

        // Show recent commands section when query is empty and we have recent commands
        if self.query.is_empty() && !self.recent_commands.is_empty() {
            // Section header
            results.push(Line::from(Span::styled(
                "  ── Recent ──",
                self.styles.muted_text,
            )));

            for &cmd_idx in &self.recent_commands {
                if results.len() >= visible_height {
                    break;
                }
                let line = self.render_command_line(cmd_idx, display_index == self.selected_index);
                results.push(line);
                display_index += 1;
            }

            // Add separator before all commands
            if results.len() < visible_height {
                results.push(Line::from(Span::styled(
                    "  ── All Commands ──",
                    self.styles.muted_text,
                )));
            }
        }

        // Render filtered results
        for &cmd_idx in &self.filtered {
            if results.len() >= visible_height {
                break;
            }
            let line = self.render_command_line(cmd_idx, display_index == self.selected_index);
            results.push(line);
            display_index += 1;
        }

        if results.is_empty() {
            let no_results = Paragraph::new(Line::from(Span::styled(
                "  No matching commands",
                self.styles.muted_text,
            )));
            frame.render_widget(no_results, chunks[1]);
        } else {
            let results_para = Paragraph::new(results);
            frame.render_widget(results_para, chunks[1]);
        }

        // Render description for selected command
        if let Some(cmd) = self.selected_command() {
            if let Some(desc) = &cmd.description {
                let desc_line =
                    Line::from(Span::styled(format!("  {}", desc), self.styles.muted_text));
                let desc_para = Paragraph::new(desc_line);
                frame.render_widget(desc_para, chunks[2]);
            }
        }
    }

    /// Render a single command line
    fn render_command_line(&self, cmd_idx: usize, is_selected: bool) -> Line<'_> {
        let cmd = &self.commands[cmd_idx];
        let mut spans = vec![];

        // Selection indicator
        if is_selected {
            spans.push(Span::styled("▶ ", self.styles.warning_text));
        } else {
            spans.push(Span::styled("  ", self.styles.muted_text));
        }

        // Category badge with color-coded style
        spans.push(Span::styled(
            format!("[{}] ", cmd.category.as_str()),
            cmd.category.badge_style(&self.styles),
        ));

        // Command name
        let name_style = if is_selected {
            self.styles.selected
        } else {
            self.styles.normal_text
        };
        spans.push(Span::styled(&cmd.name, name_style));

        // Shortcut hint
        if let Some(shortcut) = &cmd.shortcut {
            spans.push(Span::styled(
                format!("  ({})", shortcut),
                self.styles.muted_text,
            ));
        }

        Line::from(spans)
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to create a centered rect
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
