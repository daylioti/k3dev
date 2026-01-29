use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Cluster action definition
#[allow(dead_code)]
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
}

impl ClusterAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterAction::Start => "start",
            ClusterAction::Stop => "stop",
            ClusterAction::Restart => "restart",
            ClusterAction::Destroy => "destroy",
            ClusterAction::Info => "info",
        }
    }
}

/// Action bar component for cluster operations
pub struct ActionBar {
    actions: Vec<Action>,
    selected_index: usize,
    styles: Styles,
    cluster_name: Option<String>,
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
            ],
            selected_index: 0,
            styles: Styles::from_theme(theme),
            cluster_name: None,
        }
    }

    /// Set the cluster name to display
    pub fn set_cluster_name(&mut self, name: Option<String>) {
        self.cluster_name = name;
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

    #[allow(dead_code)]
    pub fn set_action_enabled(&mut self, id: &str, enabled: bool) {
        if let Some(action) = self.actions.iter_mut().find(|a| a.id == id) {
            action.enabled = enabled;
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
            _ => None,
        }
    }

    /// Get action index at x position (for mouse click handling)
    /// Returns the action index if click is within an action button
    pub fn get_action_at_x(&self, x: usize) -> Option<usize> {
        // Each action is: icon (1-2 chars) + space + label + separator " │ " (3 chars)
        // Approximate: "▶ Start │ " = ~10 chars per action
        let mut pos = 1; // Start after border
        for (i, action) in self.actions.iter().enumerate() {
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

        // Add cluster name badge if available
        if let Some(name) = &self.cluster_name {
            let badge_style = if focused {
                self.styles.title
            } else {
                self.styles.muted_text
            };
            spans.push(Span::styled(format!("[{}] ", name), badge_style));
        }

        for (i, action) in self.actions.iter().enumerate() {
            let is_selected = i == self.selected_index && focused;

            let base_style = if is_selected {
                self.styles.action_selected
            } else if !action.enabled {
                self.styles.action_disabled
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

            // Add separator (except for last item)
            if i < self.actions.len() - 1 {
                spans.push(Span::styled(" │ ", self.styles.muted_text));
            }
        }

        let line = Line::from(spans);
        let paragraph = Paragraph::new(line);

        frame.render_widget(paragraph, area);
    }
}

impl Default for ActionBar {
    fn default() -> Self {
        Self::new()
    }
}
