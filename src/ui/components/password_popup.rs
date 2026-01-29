//! Password popup component for sudo authentication
//!
//! Renders a centered modal popup for password input.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// A centered popup for password input
pub struct PasswordPopup {
    message: String,
    styles: Styles,
}

impl PasswordPopup {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            message: "Enter your password:".to_string(),
            styles: Styles::from_theme(theme),
        }
    }

    /// Set the message to display above the password field
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    /// Create a centered rectangle for the popup
    fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
        let popup_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length((area.height.saturating_sub(height)) / 2),
                Constraint::Length(height),
                Constraint::Min(0),
            ])
            .split(area);

        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length((area.width.saturating_sub(width)) / 2),
                Constraint::Length(width),
                Constraint::Min(0),
            ])
            .split(popup_layout[1])[1]
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, password: &str) {
        // Create centered popup area
        let popup_width = 50.min(area.width.saturating_sub(4));
        let popup_height = 7;
        let popup_area = Self::centered_rect(popup_width, popup_height, area);

        // Clear the background
        frame.render_widget(Clear, popup_area);

        // Create the popup block
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(" Sudo Password Required ");

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        // Layout for inner content
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Message
                Constraint::Length(1), // Spacing
                Constraint::Length(1), // Password field
                Constraint::Length(1), // Spacing
                Constraint::Length(1), // Hint
            ])
            .split(inner);

        // Render message
        let message = Paragraph::new(Line::from(Span::styled(
            &self.message,
            self.styles.normal_text,
        )));
        frame.render_widget(message, chunks[0]);

        // Render password field (masked)
        let masked_password = "*".repeat(password.len());
        let password_display = format!("Password: {}", masked_password);
        let password_line = Paragraph::new(Line::from(Span::styled(
            password_display,
            self.styles.normal_text.add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(password_line, chunks[2]);

        // Set cursor position after the password
        let cursor_x = chunks[2].x + 10 + password.len() as u16; // "Password: " is 10 chars
        let cursor_y = chunks[2].y;
        if cursor_x < popup_area.x + popup_area.width - 1 {
            frame.set_cursor_position((cursor_x, cursor_y));
        }

        // Render hint
        let hint = Paragraph::new(Line::from(Span::styled(
            "[Enter] Submit  [Esc] Cancel",
            self.styles.muted_text,
        )));
        frame.render_widget(hint, chunks[4]);
    }
}

impl Default for PasswordPopup {
    fn default() -> Self {
        Self::new()
    }
}
