//! VT100-to-ratatui terminal emulator view for interactive pod shells

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// Connection state for the shell view
enum ShellState {
    Connecting,
    Connected,
    Disconnected,
    Error(String),
}

/// VT100 terminal emulator view
pub struct ShellView {
    parser: vt100::Parser,
    state: ShellState,
}

impl ShellView {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 0),
            state: ShellState::Connecting,
        }
    }

    /// Feed raw output bytes into the VT100 parser
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the VT100 parser screen
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows.max(1), cols.max(1));
    }

    pub fn set_connected(&mut self) {
        self.state = ShellState::Connected;
    }

    pub fn set_error(&mut self, msg: String) {
        self.state = ShellState::Error(msg);
    }

    pub fn set_disconnected(&mut self) {
        self.state = ShellState::Disconnected;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match &self.state {
            ShellState::Connecting => {
                let text = Paragraph::new(Line::from(Span::styled(
                    "  Connecting...",
                    Style::default().fg(Color::Yellow),
                )));
                frame.render_widget(text, area);
            }
            ShellState::Disconnected => {
                let text = Paragraph::new(Line::from(Span::styled(
                    "  Disconnected. Switch tabs to reconnect.",
                    Style::default().fg(Color::DarkGray),
                )));
                frame.render_widget(text, area);
            }
            ShellState::Error(msg) => {
                let lines = vec![
                    Line::from(Span::styled(
                        format!("  Error: {}", msg),
                        Style::default().fg(Color::Red),
                    )),
                    Line::from(Span::styled(
                        "  Switch tabs to reconnect.",
                        Style::default().fg(Color::DarkGray),
                    )),
                ];
                let text = Paragraph::new(lines);
                frame.render_widget(text, area);
            }
            ShellState::Connected => {
                self.render_terminal(frame, area);
            }
        }
    }

    fn render_terminal(&self, frame: &mut Frame, area: Rect) {
        let screen = self.parser.screen();
        let (screen_rows, screen_cols) = screen.size();
        let cursor_pos = screen.cursor_position();

        let render_rows = screen_rows.min(area.height);
        let render_cols = screen_cols.min(area.width);

        let mut lines: Vec<Line> = Vec::with_capacity(render_rows as usize);

        for row in 0..render_rows {
            let mut spans: Vec<Span> = Vec::new();
            let mut current_style = Style::default();
            let mut current_text = String::new();

            for col in 0..render_cols {
                let cell = match screen.cell(row, col) {
                    Some(c) => c,
                    None => continue,
                };

                let mut cell_style = cell_to_style(cell);

                // Render cursor with reverse video
                if row == cursor_pos.0 && col == cursor_pos.1 {
                    cell_style = cell_style.add_modifier(Modifier::REVERSED);
                }

                if cell_style != current_style {
                    if !current_text.is_empty() {
                        spans.push(Span::styled(
                            std::mem::take(&mut current_text),
                            current_style,
                        ));
                    }
                    current_style = cell_style;
                }

                let contents = cell.contents();
                if contents.is_empty() {
                    current_text.push(' ');
                } else {
                    current_text.push_str(&contents);
                }
            }

            if !current_text.is_empty() {
                spans.push(Span::styled(current_text, current_style));
            }

            lines.push(Line::from(spans));
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, area);
    }
}

/// Map vt100 cell attributes to ratatui Style
fn cell_to_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();

    match cell.fgcolor() {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            style = style.fg(Color::Indexed(i));
        }
        vt100::Color::Rgb(r, g, b) => {
            style = style.fg(Color::Rgb(r, g, b));
        }
    }

    match cell.bgcolor() {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            style = style.bg(Color::Indexed(i));
        }
        vt100::Color::Rgb(r, g, b) => {
            style = style.bg(Color::Rgb(r, g, b));
        }
    }

    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }

    style
}

/// Convert a crossterm key event into raw terminal bytes for shell stdin
pub fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    // Ctrl+<letter> → 0x01-0x1A
    if modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = code {
            let byte = (c.to_ascii_lowercase() as u8)
                .wrapping_sub(b'a')
                .wrapping_add(1);
            if byte <= 26 {
                return Some(vec![byte]);
            }
        }
    }

    match code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            Some(s.as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        _ => None,
    }
}
