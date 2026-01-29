use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::keybindings::KeybindingResolver;
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Help section with title and key bindings
struct HelpSection {
    title: String,
    bindings: Vec<(String, String)>,
}

/// Help overlay component showing all keybindings
pub struct HelpOverlay {
    styles: Styles,
    scroll_offset: usize,
    sections: Vec<HelpSection>,
    total_lines: usize, // Cache total line count
}

impl HelpOverlay {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        let sections = Self::default_sections();
        let total_lines = Self::count_lines(&sections);
        Self {
            styles: Styles::from_theme(theme),
            scroll_offset: 0,
            sections,
            total_lines,
        }
    }

    /// Count total lines in sections
    fn count_lines(sections: &[HelpSection]) -> usize {
        sections.iter().map(|s| s.bindings.len() + 3).sum() // +3 for title, empty line before and after
    }

    /// Update sections based on keybinding configuration
    pub fn update_from_resolver(&mut self, resolver: &KeybindingResolver) {
        use crate::keybindings::KeyAction;

        // Helper to get binding or default
        let get_binding = |action: &KeyAction, default: &str| -> String {
            resolver
                .get_binding_display(action)
                .map(|s| s.to_string())
                .unwrap_or_else(|| default.to_string())
        };

        self.sections = vec![
            HelpSection {
                title: "Navigation".to_string(),
                bindings: vec![
                    (
                        get_binding(&KeyAction::MoveDown, "j / ↓"),
                        "Move down".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::MoveUp, "k / ↑"),
                        "Move up".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::MoveLeft, "h / ←"),
                        "Collapse / Move left".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::MoveRight, "l / →"),
                        "Expand / Move right".to_string(),
                    ),
                    ("[N]j/k".to_string(), "Move N times (e.g., 3j)".to_string()),
                    (
                        get_binding(&KeyAction::ToggleFocus, "Tab"),
                        "Switch focus (menu/actions)".to_string(),
                    ),
                    (
                        "1 / 2 / 3".to_string(),
                        "Focus Commands / Pods / Actions".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::Execute, "Enter"),
                        "Execute / Toggle".to_string(),
                    ),
                ],
            },
            HelpSection {
                title: "Cluster Actions".to_string(),
                bindings: vec![
                    ("Tab → ←/→".to_string(), "Select cluster action".to_string()),
                    ("Enter".to_string(), "Execute selected action".to_string()),
                ],
            },
            HelpSection {
                title: "Pod Actions".to_string(),
                bindings: vec![
                    ("Enter".to_string(), "Open pod context menu".to_string()),
                    (
                        "l / e / d".to_string(),
                        "Logs / Exec / Describe".to_string(),
                    ),
                    ("x / r".to_string(), "Delete / Restart pod".to_string()),
                ],
            },
            HelpSection {
                title: "Commands".to_string(),
                bindings: vec![
                    (
                        get_binding(&KeyAction::Refresh, "r"),
                        "Refresh all data".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::UpdateHosts, "H"),
                        "Update /etc/hosts".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::CommandPalette, ":"),
                        "Open command palette".to_string(),
                    ),
                    ("/".to_string(), "Search/filter menu".to_string()),
                    (
                        get_binding(&KeyAction::Help, "?"),
                        "Toggle this help".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::Quit, "q"),
                        "Quit application".to_string(),
                    ),
                    (
                        get_binding(&KeyAction::Cancel, "Ctrl+C"),
                        "Cancel running command".to_string(),
                    ),
                ],
            },
            HelpSection {
                title: "Panel Resize".to_string(),
                bindings: vec![
                    ("+ / =".to_string(), "Increase menu width".to_string()),
                    ("- / _".to_string(), "Decrease menu width".to_string()),
                ],
            },
            HelpSection {
                title: "Mouse".to_string(),
                bindings: vec![("Click".to_string(), "Select and execute".to_string())],
            },
        ];

        self.total_lines = Self::count_lines(&self.sections);
    }

    fn default_sections() -> Vec<HelpSection> {
        vec![
            HelpSection {
                title: "Navigation".to_string(),
                bindings: vec![
                    ("j / ↓".to_string(), "Move down".to_string()),
                    ("k / ↑".to_string(), "Move up".to_string()),
                    ("h / ←".to_string(), "Collapse / Move left".to_string()),
                    ("l / →".to_string(), "Expand / Move right".to_string()),
                    ("[N]j/k".to_string(), "Move N times (e.g., 3j)".to_string()),
                    ("Tab".to_string(), "Switch focus (menu/actions)".to_string()),
                    ("Enter".to_string(), "Execute / Toggle".to_string()),
                ],
            },
            HelpSection {
                title: "Cluster Actions".to_string(),
                bindings: vec![
                    ("Tab → ←/→".to_string(), "Select cluster action".to_string()),
                    ("Enter".to_string(), "Execute selected action".to_string()),
                ],
            },
            HelpSection {
                title: "Commands".to_string(),
                bindings: vec![
                    ("r".to_string(), "Refresh all data".to_string()),
                    ("H".to_string(), "Update /etc/hosts".to_string()),
                    (":".to_string(), "Open command palette".to_string()),
                    ("?".to_string(), "Toggle this help".to_string()),
                    ("q".to_string(), "Quit application".to_string()),
                    ("Ctrl+C".to_string(), "Cancel running command".to_string()),
                ],
            },
            HelpSection {
                title: "Mouse".to_string(),
                bindings: vec![("Click".to_string(), "Select and execute".to_string())],
            },
        ]
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    /// Scroll up one page
    pub fn page_up(&mut self, visible_height: usize) {
        self.scroll_offset = self
            .scroll_offset
            .saturating_sub(visible_height.saturating_sub(2));
    }

    /// Scroll down one page
    pub fn page_down(&mut self, visible_height: usize) {
        let max_scroll = self.total_lines.saturating_sub(visible_height);
        self.scroll_offset =
            (self.scroll_offset + visible_height.saturating_sub(2)).min(max_scroll);
    }

    /// Reset scroll to top
    pub fn reset_scroll(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Create a centered popup
        let popup_area = centered_rect(60, 70, area);

        // Clear background
        frame.render_widget(Clear, popup_area);

        // Calculate scroll indicator
        let scroll_indicator = if self.total_lines > 0 {
            let current_line = self.scroll_offset + 1;
            format!(" [{}/{}] ", current_line, self.total_lines)
        } else {
            String::new()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(" Help - Press ? or Esc to close ")
            .title_bottom(
                Line::from(Span::styled(
                    format!("{}PgUp/PgDn to scroll ", scroll_indicator),
                    self.styles.muted_text,
                ))
                .right_aligned(),
            );

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        // Build help content
        let mut lines: Vec<Line> = Vec::new();

        for section in &self.sections {
            // Section title
            lines.push(Line::from(Span::styled(
                format!("━━ {} ━━", section.title),
                self.styles.title,
            )));
            lines.push(Line::from(""));

            // Key bindings
            for (key, desc) in &section.bindings {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:12}", key),
                        self.styles.warning_text.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(desc.as_str(), self.styles.normal_text),
                ]));
            }

            lines.push(Line::from(""));
        }

        // Handle scrolling
        let visible_height = inner.height as usize;
        let max_scroll = lines.len().saturating_sub(visible_height);
        let scroll = self.scroll_offset.min(max_scroll);

        let visible_lines: Vec<Line> = lines
            .into_iter()
            .skip(scroll)
            .take(visible_height)
            .collect();

        let paragraph = Paragraph::new(visible_lines);
        frame.render_widget(paragraph, inner);
    }
}

impl Default for HelpOverlay {
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
