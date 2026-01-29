//! Pod context menu component for pod management actions

use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Actions available for pods
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PodAction {
    ViewLogs,
    ExecShell,
    Describe,
    Delete,
    Restart,
}

impl PodAction {
    pub fn label(&self) -> &'static str {
        match self {
            PodAction::ViewLogs => "View Logs",
            PodAction::ExecShell => "Exec Shell",
            PodAction::Describe => "Describe",
            PodAction::Delete => "Delete",
            PodAction::Restart => "Restart",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            PodAction::ViewLogs => "📋",
            PodAction::ExecShell => "💻",
            PodAction::Describe => "📄",
            PodAction::Delete => "🗑️",
            PodAction::Restart => "🔄",
        }
    }

    pub fn shortcut(&self) -> char {
        match self {
            PodAction::ViewLogs => 'l',
            PodAction::ExecShell => 'e',
            PodAction::Describe => 'd',
            PodAction::Delete => 'x',
            PodAction::Restart => 'r',
        }
    }

    pub fn all() -> Vec<PodAction> {
        vec![
            PodAction::ViewLogs,
            PodAction::ExecShell,
            PodAction::Describe,
            PodAction::Delete,
            PodAction::Restart,
        ]
    }
}

/// Pod context menu popup
pub struct PodContextMenu {
    styles: Styles,
    actions: Vec<PodAction>,
    selected_index: usize,
    pod_name: String,
    pod_namespace: String,
}

impl PodContextMenu {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            styles: Styles::from_theme(theme),
            actions: PodAction::all(),
            selected_index: 0,
            pod_name: String::new(),
            pod_namespace: String::new(),
        }
    }

    /// Set the target pod for this menu
    pub fn set_pod(&mut self, name: String, namespace: String) {
        self.pod_name = name;
        self.pod_namespace = namespace;
        self.selected_index = 0;
    }

    /// Get the pod name
    pub fn pod_name(&self) -> &str {
        &self.pod_name
    }

    /// Get the pod namespace
    pub fn pod_namespace(&self) -> &str {
        &self.pod_namespace
    }

    /// Move selection up
    pub fn move_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        } else {
            self.selected_index = self.actions.len() - 1;
        }
    }

    /// Move selection down
    pub fn move_down(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.actions.len();
    }

    /// Get the currently selected action
    pub fn selected_action(&self) -> Option<PodAction> {
        self.actions.get(self.selected_index).copied()
    }

    /// Select action by shortcut key
    pub fn select_by_shortcut(&mut self, c: char) -> Option<PodAction> {
        let c_lower = c.to_ascii_lowercase();
        for (idx, action) in self.actions.iter().enumerate() {
            if action.shortcut() == c_lower {
                self.selected_index = idx;
                return Some(*action);
            }
        }
        None
    }

    /// Reset the menu state
    pub fn reset(&mut self) {
        self.selected_index = 0;
        self.pod_name.clear();
        self.pod_namespace.clear();
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Create a centered popup
        let popup_width = 35;
        let popup_height = (self.actions.len() + 4) as u16; // +4 for borders, title, pod name
        let popup_area = centered_rect_fixed(popup_width, popup_height, area);

        // Clear background
        frame.render_widget(Clear, popup_area);

        // Truncate pod name if too long
        let max_pod_len = popup_width as usize - 6; // Leave room for borders and padding
        let display_name = if self.pod_name.len() > max_pod_len {
            format!("{}...", &self.pod_name[..max_pod_len - 3])
        } else {
            self.pod_name.clone()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(format!(" {} ", display_name));

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        // Build action lines
        let mut lines: Vec<Line> = Vec::new();

        // Namespace info
        lines.push(Line::from(Span::styled(
            format!("  ns: {}", self.pod_namespace),
            self.styles.muted_text,
        )));
        lines.push(Line::from(""));

        // Action items
        for (idx, action) in self.actions.iter().enumerate() {
            let is_selected = idx == self.selected_index;

            let cursor = if is_selected { "▸ " } else { "  " };
            let shortcut_hint = format!("({})", action.shortcut());

            let style = if is_selected {
                self.styles.selected
            } else {
                self.styles.normal_text
            };

            let shortcut_style = if is_selected {
                self.styles.selected
            } else {
                self.styles.muted_text
            };

            lines.push(Line::from(vec![
                Span::styled(
                    cursor,
                    if is_selected {
                        self.styles.warning_text
                    } else {
                        self.styles.muted_text
                    },
                ),
                Span::styled(format!("{} ", action.icon()), style),
                Span::styled(format!("{:<12}", action.label()), style),
                Span::styled(shortcut_hint, shortcut_style),
            ]));
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}

impl Default for PodContextMenu {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to create a centered rect with fixed dimensions
fn centered_rect_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}
