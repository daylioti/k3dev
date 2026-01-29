use chrono::Local;
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Output line type for coloring
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputType {
    Info,
    Success,
    Error,
    Warning,
}

/// A single line of output
#[derive(Debug, Clone)]
pub struct OutputLine {
    pub content: String,
    pub output_type: OutputType,
    pub timestamp: chrono::DateTime<Local>,
}

impl OutputLine {
    pub fn info(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            output_type: OutputType::Info,
            timestamp: Local::now(),
        }
    }

    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            output_type: OutputType::Success,
            timestamp: Local::now(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            output_type: OutputType::Error,
            timestamp: Local::now(),
        }
    }

    pub fn warning(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            output_type: OutputType::Warning,
            timestamp: Local::now(),
        }
    }
}

/// Output panel component (used as internal buffer, rendering done via OutputPopup)
pub struct Output {
    title: String,
    lines: Vec<OutputLine>,
    scroll_position: usize,
    #[allow(dead_code)]
    styles: Styles,
}

impl Output {
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
        self.scroll_to_bottom();
    }

    pub fn add_info(&mut self, content: impl Into<String>) {
        self.add_line(OutputLine::info(content));
    }

    pub fn add_success(&mut self, content: impl Into<String>) {
        self.add_line(OutputLine::success(content));
    }

    pub fn add_error(&mut self, content: impl Into<String>) {
        self.add_line(OutputLine::error(content));
    }

    pub fn add_warning(&mut self, content: impl Into<String>) {
        self.add_line(OutputLine::warning(content));
    }

    #[allow(dead_code)]
    pub fn add_multiline(&mut self, content: &str, output_type: OutputType) {
        for line in content.lines() {
            self.add_line(OutputLine {
                content: line.to_string(),
                output_type,
                timestamp: Local::now(),
            });
        }
    }

    #[allow(dead_code)]
    pub fn scroll_up(&mut self) {
        if self.scroll_position > 0 {
            self.scroll_position -= 1;
        }
    }

    #[allow(dead_code)]
    pub fn scroll_down(&mut self, visible_lines: usize) {
        let max_scroll = self.lines.len().saturating_sub(visible_lines);
        if self.scroll_position < max_scroll {
            self.scroll_position += 1;
        }
    }

    fn scroll_to_bottom(&mut self) {
        // Will be adjusted when rendering based on visible height
        self.scroll_position = self.lines.len();
    }

    #[allow(dead_code)]
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_unfocused)
            .title(format!(" {} ", self.title));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible_lines = inner.height as usize;

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

        frame.render_widget(paragraph, inner);

        // Scroll indicator
        if self.lines.len() > visible_lines && scroll_pos + visible_lines < self.lines.len() {
            let indicator_area = Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(1),
                inner.width,
                1,
            );
            let indicator =
                Paragraph::new(Line::from(Span::styled("↓ more ↓", self.styles.muted_text)))
                    .centered();
            frame.render_widget(indicator, indicator_area);
        }
    }
}

impl Default for Output {
    fn default() -> Self {
        Self::new()
    }
}
