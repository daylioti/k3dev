//! Output popup component for displaying command output
//!
//! Renders a centered modal popup showing command output with scrolling support.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

use super::output::{OutputLine, OutputType};

/// A centered popup for displaying command output
pub struct OutputPopup {
    title: String,
    lines: Vec<OutputLine>,
    scroll_position: usize,
    styles: Styles,
}

impl OutputPopup {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            title: "Output".to_string(),
            lines: Vec::new(),
            scroll_position: 0,
            styles: Styles::from_theme(theme),
        }
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll_position = 0;
    }

    pub fn add_line(&mut self, line: OutputLine) {
        self.lines.push(line);
        self.scroll_to_bottom_if_at_end();
    }

    pub fn scroll_up(&mut self) {
        if self.scroll_position > 0 {
            self.scroll_position -= 1;
        }
    }

    pub fn scroll_down(&mut self, visible_lines: usize) {
        let max_scroll = self.lines.len().saturating_sub(visible_lines);
        if self.scroll_position < max_scroll {
            self.scroll_position += 1;
        }
    }

    fn scroll_to_bottom_if_at_end(&mut self) {
        // Auto-scroll only if we're already near the bottom
        self.scroll_position = self.lines.len();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_position = self.lines.len();
    }

    /// Check if there are any lines to display
    #[allow(dead_code)]
    pub fn has_content(&self) -> bool {
        !self.lines.is_empty()
    }

    /// Create a centered rectangle for the popup
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

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Create centered popup area (70% width, 60% height)
        let popup_area = Self::centered_rect(70, 60, area);

        // Clear the background
        frame.render_widget(Clear, popup_area);

        // Create the popup block
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(format!(" {} ", self.title));

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        // Reserve space for hint at bottom
        let content_height = inner.height.saturating_sub(2);
        let content_area = Rect::new(inner.x, inner.y, inner.width, content_height);
        let hint_area = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );

        let visible_lines = content_area.height as usize;

        // Adjust scroll position
        let scroll_pos = if self.scroll_position > self.lines.len().saturating_sub(visible_lines) {
            self.lines.len().saturating_sub(visible_lines)
        } else {
            self.scroll_position
        };

        let end = (scroll_pos + visible_lines).min(self.lines.len());

        // Build lines for rendering
        let text_lines: Vec<Line> = self.lines[scroll_pos..end]
            .iter()
            .map(|line| {
                let style = match line.output_type {
                    OutputType::Info => self.styles.normal_text,
                    OutputType::Success => self.styles.success_text,
                    OutputType::Error => self.styles.error_text,
                    OutputType::Warning => self.styles.warning_text,
                };
                let timestamp = line.timestamp.format("[%H:%M:%S]").to_string();
                Line::from(vec![
                    Span::styled(format!("{} ", timestamp), self.styles.muted_text),
                    Span::styled(&line.content, style),
                ])
            })
            .collect();

        let paragraph = Paragraph::new(text_lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, content_area);

        // Scroll indicator if there's more content below
        if self.lines.len() > visible_lines && scroll_pos + visible_lines < self.lines.len() {
            let indicator_area = Rect::new(
                content_area.x,
                content_area.y + content_area.height.saturating_sub(1),
                content_area.width,
                1,
            );
            let indicator =
                Paragraph::new(Line::from(Span::styled("↓ more ↓", self.styles.muted_text)))
                    .centered();
            frame.render_widget(indicator, indicator_area);
        }

        // Render hint
        let hint = Paragraph::new(Line::from(Span::styled(
            "[↑/k] Up  [↓/j] Down  [Esc/Enter] Close",
            self.styles.muted_text,
        )))
        .centered();
        frame.render_widget(hint, hint_area);
    }

    /// Get visible lines count for scroll calculations
    #[allow(dead_code)]
    pub fn get_visible_lines(&self, area: Rect) -> usize {
        let popup_area = Self::centered_rect(70, 60, area);
        // Account for borders (2) and hint line (2)
        popup_area.height.saturating_sub(4) as usize
    }
}

impl Default for OutputPopup {
    fn default() -> Self {
        Self::new()
    }
}
