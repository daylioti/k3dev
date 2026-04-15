use chrono::Local;

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
}

impl Output {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(_theme: Theme) -> Self {
        Self {
            title: "Output".to_string(),
            lines: Vec::new(),
            scroll_position: 0,
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
        self.scroll_position = self.lines.len();
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
}

impl Default for Output {
    fn default() -> Self {
        Self::new()
    }
}
