use std::path::PathBuf;

use ratatui::{
    layout::{Alignment, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Cluster action definition
#[derive(Debug, Clone)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub enabled: bool,
    pub shortcut: Option<char>, // Shortcut key (will be underlined in label)
}

/// Cluster action type
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClusterAction {
    Start,
    Stop,
    Restart,
    Destroy,
    Info,
    DeleteSnapshots,
    Diagnostics,
    PreflightCheck,
}

impl ClusterAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterAction::Start => "start",
            ClusterAction::Stop => "stop",
            ClusterAction::Restart => "restart",
            ClusterAction::Destroy => "destroy",
            ClusterAction::Info => "info",
            ClusterAction::DeleteSnapshots => "delete-snapshots",
            ClusterAction::Diagnostics => "diagnostics",
            ClusterAction::PreflightCheck => "preflight-check",
        }
    }
}

/// Action bar component for cluster operations
pub struct ActionBar {
    actions: Vec<Action>,
    selected_index: usize,
    styles: Styles,
    cluster_name: Option<String>,
    config_path: Option<PathBuf>,
}

impl ActionBar {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            actions: vec![
                Action {
                    id: "start".to_string(),
                    label: "Start".to_string(),
                    icon: "▶".to_string(),
                    enabled: true,
                    shortcut: Some('S'),
                },
                Action {
                    id: "stop".to_string(),
                    label: "Stop".to_string(),
                    icon: "⏹".to_string(),
                    enabled: true,
                    shortcut: Some('T'),
                },
                Action {
                    id: "restart".to_string(),
                    label: "Restart".to_string(),
                    icon: "↻".to_string(),
                    enabled: true,
                    shortcut: Some('R'),
                },
                Action {
                    id: "destroy".to_string(),
                    label: "Destroy".to_string(),
                    icon: "✕".to_string(),
                    enabled: true,
                    shortcut: Some('D'),
                },
                Action {
                    id: "info".to_string(),
                    label: "Info".to_string(),
                    icon: "ℹ".to_string(),
                    enabled: true,
                    shortcut: Some('I'),
                },
                Action {
                    id: "preflight".to_string(),
                    label: "Preflight".to_string(),
                    icon: "⚑".to_string(),
                    enabled: true,
                    shortcut: Some('P'),
                },
                Action {
                    id: "diagnostics".to_string(),
                    label: "Diagnostics".to_string(),
                    icon: "✚".to_string(),
                    enabled: false,
                    shortcut: Some('G'),
                },
            ],
            selected_index: 0,
            styles: Styles::from_theme(theme),
            cluster_name: None,
            config_path: None,
        }
    }

    /// Set the cluster name to display
    pub fn set_cluster_name(&mut self, name: Option<String>) {
        self.cluster_name = name;
    }

    /// Set the config file path to display
    pub fn set_config_path(&mut self, path: Option<PathBuf>) {
        self.config_path = path;
    }

    pub fn move_left(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        } else {
            self.selected_index = self.actions.len() - 1;
        }
        self.skip_disabled_left();
    }

    pub fn move_right(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.actions.len();
        self.skip_disabled_right();
    }

    pub fn move_up(&mut self) {
        self.move_left();
    }

    pub fn move_down(&mut self) {
        self.move_right();
    }

    fn skip_disabled_left(&mut self) {
        let start = self.selected_index;
        while !self.actions[self.selected_index].enabled {
            if self.selected_index == 0 {
                self.selected_index = self.actions.len() - 1;
            } else {
                self.selected_index -= 1;
            }
            if self.selected_index == start {
                break;
            }
        }
    }

    fn skip_disabled_right(&mut self) {
        let start = self.selected_index;
        while !self.actions[self.selected_index].enabled {
            self.selected_index = (self.selected_index + 1) % self.actions.len();
            if self.selected_index == start {
                break;
            }
        }
    }

    pub fn set_action_enabled(&mut self, id: &str, enabled: bool) {
        if let Some(action) = self.actions.iter_mut().find(|a| a.id == id) {
            action.enabled = enabled;
        }
        // If the currently selected action just became disabled, advance to the
        // next enabled one so navigation/Enter behavior stays sensible.
        if self
            .actions
            .get(self.selected_index)
            .map(|a| !a.enabled)
            .unwrap_or(false)
        {
            self.skip_disabled_right();
        }
    }

    /// Get the currently selected action
    pub fn selected_action(&self) -> Option<ClusterAction> {
        let action = self.actions.get(self.selected_index)?;
        if !action.enabled {
            return None;
        }
        match action.id.as_str() {
            "start" => Some(ClusterAction::Start),
            "stop" => Some(ClusterAction::Stop),
            "restart" => Some(ClusterAction::Restart),
            "destroy" => Some(ClusterAction::Destroy),
            "info" => Some(ClusterAction::Info),
            "diagnostics" => Some(ClusterAction::Diagnostics),
            "preflight" => Some(ClusterAction::PreflightCheck),
            _ => None,
        }
    }

    /// Get action index at x position (for mouse click handling)
    /// Returns the action index if click is within an action button
    pub fn get_action_at_x(&self, x: usize) -> Option<usize> {
        // Each action is: icon (1-2 chars) + space + label + separator " │ " (3 chars)
        // Approximate: "▶ Start │ " = ~10 chars per action
        let mut pos = 3; // Start after focus-stripe prefix ("▌ " or "  ")
        for (i, action) in self.actions.iter().enumerate() {
            if !action.enabled {
                continue;
            }
            let action_width = action.icon.chars().count() + 1 + action.label.len();
            if x >= pos && x < pos + action_width {
                return Some(i);
            }
            pos += action_width + 3; // +3 for " │ " separator
        }
        None
    }

    /// Select action by index
    pub fn select_index(&mut self, index: usize) {
        if index < self.actions.len() {
            self.selected_index = index;
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        // Build action spans - compact 1-line style (no borders)
        let mut spans: Vec<Span> = Vec::new();

        // Focus stripe on the left when this panel is active
        if focused {
            spans.push(Span::styled("▌ ", self.styles.border_focused));
        } else {
            spans.push(Span::raw("  "));
        }

        // Add cluster name badge if available
        if let Some(name) = &self.cluster_name {
            let badge_style = if focused {
                self.styles.title
            } else {
                self.styles.muted_text
            };
            spans.push(Span::styled(format!("[{}] ", name), badge_style));
        }

        let last_visible = self
            .actions
            .iter()
            .rposition(|a| a.enabled)
            .unwrap_or(0);

        for (i, action) in self.actions.iter().enumerate() {
            if !action.enabled {
                continue;
            }
            let is_selected = i == self.selected_index && focused;

            let base_style = if is_selected {
                self.styles.action_selected
            } else {
                self.styles.action_normal
            };

            // Add icon
            spans.push(Span::styled(format!("{} ", action.icon), base_style));

            // Add label with shortcut character underlined
            if let Some(shortcut) = action.shortcut {
                let shortcut_lower = shortcut.to_ascii_lowercase();
                let mut found = false;
                for c in action.label.chars() {
                    if !found && c.to_ascii_lowercase() == shortcut_lower {
                        // Underline the shortcut character
                        spans.push(Span::styled(
                            c.to_string(),
                            base_style.add_modifier(Modifier::UNDERLINED),
                        ));
                        found = true;
                    } else {
                        spans.push(Span::styled(c.to_string(), base_style));
                    }
                }
            } else {
                spans.push(Span::styled(&action.label, base_style));
            }

            // Add separator between visible items
            if i < last_visible {
                spans.push(Span::styled(" │ ", self.styles.muted_text));
            }
        }

        let line = Line::from(spans);
        let paragraph = Paragraph::new(line);
        frame.render_widget(paragraph, area);

        // Render config path right-aligned
        if let Some(path) = &self.config_path {
            let path_display = path.to_string_lossy();
            let home = dirs::home_dir();
            let short_path = match &home {
                Some(home_dir) => path_display
                    .strip_prefix(home_dir.to_string_lossy().as_ref())
                    .map(|rest| format!("~{}", rest))
                    .unwrap_or_else(|| path_display.to_string()),
                None => path_display.to_string(),
            };
            let config_line = Line::from(Span::styled(short_path, self.styles.muted_text));
            let config_paragraph = Paragraph::new(config_line).alignment(Alignment::Right);
            frame.render_widget(config_paragraph, area);
        }
    }

    /// Render actions as a vertical list (for stopped screen)
    pub fn render_vertical(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let border_style = if focused {
            self.styles.border_focused
        } else {
            self.styles.border_unfocused
        };
        let border_type = if focused {
            ratatui::widgets::BorderType::Thick
        } else {
            ratatui::widgets::BorderType::Rounded
        };
        let title_style = if focused {
            self.styles.title.add_modifier(Modifier::BOLD)
        } else {
            self.styles.normal_text
        };
        let title = if focused {
            " ▶ Actions ◀ "
        } else {
            " Actions "
        };

        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(border_type)
            .border_style(border_style)
            .title(Span::styled(title, title_style));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Align to top
        let start_y = 0;

        let mut y = 0;
        for (i, action) in self.actions.iter().enumerate() {
            if !action.enabled {
                continue;
            }
            if y >= inner.height as usize {
                break;
            }

            let is_selected = i == self.selected_index && focused;
            let base_style = if is_selected {
                self.styles.action_selected
            } else {
                self.styles.action_normal
            };

            let mut spans: Vec<Span> = Vec::new();
            spans.push(Span::styled("  ", base_style));
            spans.push(Span::styled(format!("{} ", action.icon), base_style));

            // Label with underlined shortcut
            if let Some(shortcut) = action.shortcut {
                let shortcut_lower = shortcut.to_ascii_lowercase();
                let mut found = false;
                for c in action.label.chars() {
                    if !found && c.to_ascii_lowercase() == shortcut_lower {
                        spans.push(Span::styled(
                            c.to_string(),
                            base_style.add_modifier(Modifier::UNDERLINED),
                        ));
                        found = true;
                    } else {
                        spans.push(Span::styled(c.to_string(), base_style));
                    }
                }
            } else {
                spans.push(Span::styled(&action.label, base_style));
            }

            // Right-aligned shortcut hint
            if let Some(shortcut) = action.shortcut {
                let label_len = action.icon.chars().count() + 1 + action.label.len() + 2;
                let padding = (inner.width as usize).saturating_sub(label_len + 5);
                spans.push(Span::styled(" ".repeat(padding), base_style));
                spans.push(Span::styled(
                    format!("[{}]", shortcut),
                    self.styles.muted_text,
                ));
            }

            let line = Line::from(spans);
            let row = Rect::new(inner.x, inner.y + start_y as u16 + y as u16, inner.width, 1);
            frame.render_widget(Paragraph::new(line), row);
            y += 1;
        }
    }
}

impl Default for ActionBar {
    fn default() -> Self {
        Self::new()
    }
}
