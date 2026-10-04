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
    /// A hook holds a pty: keys go to it and its pending line is shown
    interactive: bool,
    /// Text a child wrote to its tty that is not yet newline-terminated
    tty_line: String,
    /// Parser state, kept across reads so a split sequence is still stripped
    ansi: AnsiState,
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
            interactive: false,
            tty_line: String::new(),
            ansi: AnsiState::default(),
        }
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll_position = 0;
        self.tty_line.clear();
        self.ansi = AnsiState::default();
    }

    /// Whether a hook is currently attached to the popup through a pty.
    /// Leaving interactive mode commits whatever the hook left unterminated.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.interactive = interactive;
        if !interactive && !self.tty_line.is_empty() {
            self.commit_tty_line();
        }
    }

    /// Append raw tty output from a child. Complete lines become output
    /// lines; the trailing partial line (typically a prompt) stays pending.
    pub fn push_tty(&mut self, text: &str) {
        for c in text.chars() {
            match self.ansi.feed(c) {
                Visible::Nothing => {}
                Visible::Newline => self.commit_tty_line(),
                Visible::Tab => self.tty_line.push_str(TAB),
                Visible::Backspace => {
                    self.tty_line.pop();
                }
                Visible::Char(c) => self.tty_line.push(c),
            }
        }
        self.scroll_to_bottom();
    }

    fn commit_tty_line(&mut self) {
        let content = std::mem::take(&mut self.tty_line);
        self.lines.push(OutputLine::info(content));
    }

    /// Lines to render: committed output plus the pending tty line
    fn total_lines(&self) -> usize {
        self.lines.len() + usize::from(self.shows_tty_line())
    }

    fn shows_tty_line(&self) -> bool {
        self.interactive || !self.tty_line.is_empty()
    }

    pub fn add_line(&mut self, mut line: OutputLine) {
        line.content = sanitize(&line.content);
        self.lines.push(line);
        self.scroll_to_bottom_if_at_end();
    }

    pub fn scroll_up(&mut self) {
        if self.scroll_position > 0 {
            self.scroll_position -= 1;
        }
    }

    pub fn scroll_down(&mut self, visible_lines: usize) {
        let max_scroll = self.total_lines().saturating_sub(visible_lines);
        if self.scroll_position < max_scroll {
            self.scroll_position += 1;
        }
    }

    fn scroll_to_bottom_if_at_end(&mut self) {
        // Auto-scroll only if we're already near the bottom
        self.scroll_position = self.total_lines();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_position = self.total_lines();
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
        let total = self.total_lines();

        // Adjust scroll position
        let scroll_pos = self
            .scroll_position
            .min(total.saturating_sub(visible_lines));

        let end = (scroll_pos + visible_lines).min(total);

        // Build lines for rendering
        let mut text_lines: Vec<Line> = self.lines[scroll_pos..end.min(self.lines.len())]
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

        // Pending tty line with a cursor, so a prompt reads as awaiting input
        if self.shows_tty_line() && end == total {
            text_lines.push(Line::from(vec![
                Span::styled("         > ", self.styles.muted_text),
                Span::styled(&self.tty_line, self.styles.warning_text),
                Span::styled("▌", self.styles.warning_text),
            ]));
        }

        let paragraph = Paragraph::new(text_lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, content_area);

        // Scroll indicator if there's more content below
        if total > visible_lines && scroll_pos + visible_lines < total {
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
        let hint_text = if self.interactive {
            "Typing and paste go to the hook  [↑/↓] Scroll  [Esc] Hide"
        } else {
            "[↑/k] Up  [↓/j] Down  [Esc/Enter] Close"
        };
        let hint =
            Paragraph::new(Line::from(Span::styled(hint_text, self.styles.muted_text))).centered();
        frame.render_widget(hint, hint_area);
    }
}

/// A tab is shown as spaces: the real thing would be expanded by the terminal
/// itself, moving text past the popup border.
const TAB: &str = "    ";

/// Strip terminal control sequences from a complete line of program output.
fn sanitize(text: &str) -> String {
    let mut state = AnsiState::default();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match state.feed(c) {
            Visible::Nothing => {}
            Visible::Char(c) => out.push(c),
            Visible::Tab => out.push_str(TAB),
            Visible::Backspace => {
                out.pop();
            }
            // A line holds no newline of its own; a stray one becomes a space
            Visible::Newline => out.push(' '),
        }
    }
    out
}

/// Parser state for stripping terminal control sequences.
///
/// Command output and tty prompts carry escape sequences — colors, cursor
/// moves, the bracketed-paste toggles a password prompt emits. ratatui drops
/// control characters but keeps the rest, so an unparsed sequence shows up as
/// visible junk like `[?2004h` in the middle of a prompt. Parsing is
/// incremental because a sequence can be split across two reads from the pty.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    #[default]
    Text,
    /// Saw ESC; the next character says what kind of sequence this is
    Escape,
    /// Inside a control sequence, ended by a final character
    Csi,
    /// Inside a string sequence (window title and friends)
    Str,
    /// Saw ESC inside a string sequence, which may be its terminator
    StrEscape,
}

/// What a character contributes to the visible text.
enum Visible {
    Nothing,
    Char(char),
    Tab,
    Backspace,
    Newline,
}

impl AnsiState {
    fn feed(&mut self, c: char) -> Visible {
        match self {
            Self::Text => match c {
                '\x1b' => {
                    *self = Self::Escape;
                    Visible::Nothing
                }
                '\n' => Visible::Newline,
                '\t' => Visible::Tab,
                '\x08' | '\x7f' => Visible::Backspace,
                c if c.is_control() => Visible::Nothing,
                c => Visible::Char(c),
            },
            Self::Escape => {
                match c {
                    '[' => *self = Self::Csi,
                    // Sequences that run until a string terminator
                    ']' | 'P' | 'X' | '^' | '_' => *self = Self::Str,
                    // Intermediate of a two-character escape, e.g. ESC ( B
                    '\x20'..='\x2f' => {}
                    // Anything else is the final character
                    _ => *self = Self::Text,
                }
                Visible::Nothing
            }
            Self::Csi => {
                // Parameters and intermediates, then a final character
                if ('\x40'..='\x7e').contains(&c) {
                    *self = Self::Text;
                }
                Visible::Nothing
            }
            Self::Str => {
                match c {
                    '\x07' => *self = Self::Text,
                    '\x1b' => *self = Self::StrEscape,
                    _ => {}
                }
                Visible::Nothing
            }
            Self::StrEscape => {
                // ESC \ ends the string; anything else is still inside it
                *self = if c == '\\' { Self::Text } else { Self::Str };
                Visible::Nothing
            }
        }
    }
}

impl Default for OutputPopup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tty_tests {
    use super::*;

    fn contents(popup: &OutputPopup) -> Vec<String> {
        popup.lines.iter().map(|l| l.content.clone()).collect()
    }

    #[test]
    fn prompt_without_newline_stays_on_the_pending_line() {
        let mut popup = OutputPopup::new();
        popup.push_tty("Enter MFA code: ");
        assert_eq!(popup.tty_line, "Enter MFA code: ");
        assert!(contents(&popup).is_empty());
    }

    #[test]
    fn newline_commits_the_pending_line_as_output() {
        let mut popup = OutputPopup::new();
        popup.push_tty("Enter MFA code: ");
        popup.push_tty("\r\nLogged in\r\n");
        assert_eq!(contents(&popup), vec!["Enter MFA code: ", "Logged in"]);
        assert_eq!(popup.tty_line, "");
    }

    #[test]
    fn backspace_and_escape_sequences_do_not_reach_the_line() {
        let mut popup = OutputPopup::new();
        popup.push_tty("abc\x08\x1b[31md\x1b[0m\x07");
        assert_eq!(popup.tty_line, "abd");
    }

    #[test]
    fn leaving_interactive_mode_flushes_the_pending_line() {
        let mut popup = OutputPopup::new();
        popup.set_interactive(true);
        popup.push_tty("partial");
        popup.set_interactive(false);
        assert_eq!(contents(&popup), vec!["partial"]);
        assert_eq!(popup.tty_line, "");
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(popup: &OutputPopup) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| popup.render(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn control_characters_never_reach_the_rendered_cells() {
        let mut popup = OutputPopup::new();
        popup.add_line(OutputLine::info("aws\rsts\x07 assume-role\tfailed"));
        popup.add_line(OutputLine::error("\x1b[31mdenied\x1b[0m"));
        popup.push_tty("Enter MFA code for arn:aws:iam::1234:mfa/user: ");

        let buffer = render(&popup);
        let offenders: Vec<&str> = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .filter(|symbol| symbol.chars().any(char::is_control))
            .collect();
        assert!(
            offenders.is_empty(),
            "control chars in cells: {offenders:?}"
        );
    }

    #[test]
    fn escape_sequences_are_stripped_rather_than_shown_as_text() {
        let mut popup = OutputPopup::new();
        popup.add_line(OutputLine::info("\x1b[31mdenied\x1b[0m"));

        let rendered: String = render(&popup)
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("denied"));
        assert!(
            !rendered.contains("31m"),
            "escape params leaked: {rendered}"
        );
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[test]
    fn an_escape_sequence_split_across_reads_is_still_stripped() {
        let mut popup = OutputPopup::new();
        popup.push_tty("code: \x1b[1");
        popup.push_tty("31mred");
        assert_eq!(popup.tty_line, "code: red");
    }

    #[test]
    fn operating_system_commands_do_not_leak_their_payload() {
        let mut popup = OutputPopup::new();
        popup.push_tty("\x1b]0;window title\x07done");
        assert_eq!(popup.tty_line, "done");
    }

    #[test]
    fn a_charset_selector_does_not_swallow_the_text_after_it() {
        let mut popup = OutputPopup::new();
        popup.push_tty("\x1b(Bplain");
        assert_eq!(popup.tty_line, "plain");
    }

    #[test]
    fn carriage_returns_and_tabs_in_command_output_are_replaced() {
        let mut popup = OutputPopup::new();
        popup.add_line(OutputLine::info("a\rb\tc"));
        assert_eq!(popup.lines[0].content, "ab    c");
    }
}
