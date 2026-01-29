use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use std::collections::HashMap;

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// A single input field
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct InputField {
    pub name: String,
    pub prompt: String,
    pub value: String,
    pub cursor_position: usize,
}

impl InputField {
    pub fn new(name: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            prompt: prompt.into(),
            value: String::new(),
            cursor_position: 0,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.value.insert(self.cursor_position, c);
        self.cursor_position += 1;
    }

    pub fn delete_char(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
            self.value.remove(self.cursor_position);
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_position < self.value.len() {
            self.cursor_position += 1;
        }
    }
}

/// Input form modal for collecting command parameters
pub struct InputForm {
    title: String,
    fields: Vec<InputField>,
    focused_field: usize,
    submit_focused: bool,
    styles: Styles,
}

impl InputForm {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            title: "Input Required".to_string(),
            fields: Vec::new(),
            focused_field: 0,
            submit_focused: false,
            styles: Styles::from_theme(theme),
        }
    }

    /// Setup the form with fields from input prompts
    pub fn setup(&mut self, title: impl Into<String>, inputs: &HashMap<String, String>) {
        self.title = title.into();
        self.fields = inputs
            .iter()
            .map(|(name, prompt)| InputField::new(name.clone(), prompt.clone()))
            .collect();
        self.focused_field = 0;
        self.submit_focused = false;
    }

    pub fn clear(&mut self) {
        self.fields.clear();
        self.focused_field = 0;
        self.submit_focused = false;
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Get all field values as a map
    pub fn get_values(&self) -> HashMap<String, String> {
        self.fields
            .iter()
            .map(|f| (f.name.clone(), f.value.clone()))
            .collect()
    }

    pub fn handle_char(&mut self, c: char) {
        if !self.submit_focused {
            if let Some(field) = self.fields.get_mut(self.focused_field) {
                field.insert_char(c);
            }
        }
    }

    pub fn handle_backspace(&mut self) {
        if !self.submit_focused {
            if let Some(field) = self.fields.get_mut(self.focused_field) {
                field.delete_char();
            }
        }
    }

    pub fn move_cursor_left(&mut self) {
        if !self.submit_focused {
            if let Some(field) = self.fields.get_mut(self.focused_field) {
                field.move_cursor_left();
            }
        }
    }

    pub fn move_cursor_right(&mut self) {
        if !self.submit_focused {
            if let Some(field) = self.fields.get_mut(self.focused_field) {
                field.move_cursor_right();
            }
        }
    }

    pub fn focus_next(&mut self) {
        if self.submit_focused {
            self.submit_focused = false;
            self.focused_field = 0;
        } else if self.focused_field < self.fields.len().saturating_sub(1) {
            self.focused_field += 1;
        } else {
            self.submit_focused = true;
        }
    }

    pub fn focus_prev(&mut self) {
        if self.submit_focused {
            self.submit_focused = false;
            self.focused_field = self.fields.len().saturating_sub(1);
        } else if self.focused_field > 0 {
            self.focused_field -= 1;
        }
    }

    pub fn is_submit_focused(&self) -> bool {
        self.submit_focused
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
        // Calculate popup size based on number of fields
        let height_percent =
            ((self.fields.len() * 2 + 4) * 100 / area.height as usize).clamp(30, 60) as u16;
        let popup_area = Self::centered_rect(50, height_percent, area);

        // Clear the background
        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(format!(" {} ", self.title));

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        if self.fields.is_empty() {
            return;
        }

        // Calculate layout: each field takes 2 lines (prompt + input) + submit button
        let field_count = self.fields.len();
        let constraints: Vec<Constraint> = self
            .fields
            .iter()
            .flat_map(|_| vec![Constraint::Length(1), Constraint::Length(1)])
            .chain(std::iter::once(Constraint::Length(2))) // Submit button
            .chain(std::iter::once(Constraint::Min(0))) // Remaining space
            .collect();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        // Render each field
        for (i, field) in self.fields.iter().enumerate() {
            let prompt_area = chunks[i * 2];
            let input_area = chunks[i * 2 + 1];

            let is_focused = !self.submit_focused && i == self.focused_field;

            // Prompt
            let prompt_style = if is_focused {
                self.styles.title
            } else {
                self.styles.muted_text
            };
            let prompt = Paragraph::new(Line::from(Span::styled(&field.prompt, prompt_style)));
            frame.render_widget(prompt, prompt_area);

            // Input field
            let input_style = if is_focused {
                self.styles.normal_text.add_modifier(Modifier::UNDERLINED)
            } else {
                self.styles.normal_text
            };

            let display_value = if field.value.is_empty() && !is_focused {
                "(empty)".to_string()
            } else {
                field.value.clone()
            };

            let input = Paragraph::new(Line::from(Span::styled(display_value, input_style)));
            frame.render_widget(input, input_area);

            // Show cursor position if focused
            if is_focused {
                frame.set_cursor_position((
                    input_area.x + field.cursor_position as u16,
                    input_area.y,
                ));
            }
        }

        // Submit button
        let submit_area = chunks[field_count * 2];
        let submit_style = if self.submit_focused {
            self.styles.action_selected
        } else {
            self.styles.action_normal
        };
        let submit =
            Paragraph::new(Line::from(Span::styled("[ Submit ]", submit_style))).centered();
        frame.render_widget(submit, submit_area);

        // Render hint at bottom of popup
        let hint_area = Rect::new(
            inner.x,
            popup_area.y + popup_area.height.saturating_sub(2),
            inner.width,
            1,
        );
        let hint = Paragraph::new(Line::from(Span::styled(
            "[Tab] Next  [Enter] Submit  [Esc] Cancel",
            self.styles.muted_text,
        )))
        .centered();
        frame.render_widget(hint, hint_area);
    }
}

impl Default for InputForm {
    fn default() -> Self {
        Self::new()
    }
}
