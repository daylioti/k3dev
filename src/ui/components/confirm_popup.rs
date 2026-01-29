//! Confirmation popup component for destructive actions

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Confirmation popup for destructive actions
pub struct ConfirmPopup {
    styles: Styles,
    title: String,
    message: String,
}

impl ConfirmPopup {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            styles: Styles::from_theme(theme),
            title: "Confirm".to_string(),
            message: "Are you sure?".to_string(),
        }
    }

    /// Set the title and message for the confirmation
    pub fn set_content(&mut self, title: &str, message: &str) {
        self.title = title.to_string();
        self.message = message.to_string();
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Create a centered popup
        let popup_area = centered_rect(50, 20, area);

        // Clear background
        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.error_text)
            .title(format!(" {} ", self.title));

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        // Split into message and buttons
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // Message
                Constraint::Length(2), // Buttons
            ])
            .split(inner);

        // Render message
        let message = Paragraph::new(Line::from(Span::styled(
            &self.message,
            self.styles.warning_text,
        )))
        .centered();
        frame.render_widget(message, chunks[0]);

        // Render button hints
        let buttons = Line::from(vec![
            Span::styled("[", self.styles.muted_text),
            Span::styled("y", self.styles.error_text),
            Span::styled("] Yes  ", self.styles.muted_text),
            Span::styled("[", self.styles.muted_text),
            Span::styled("n/Esc", self.styles.success_text),
            Span::styled("] No", self.styles.muted_text),
        ]);
        let buttons_para = Paragraph::new(buttons).centered();
        frame.render_widget(buttons_para, chunks[1]);
    }
}

impl Default for ConfirmPopup {
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
