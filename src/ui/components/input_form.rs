use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use std::collections::HashMap;

use crate::config::{InputDefinition, InputSpec};
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

const MULTI_SELECT_JOIN: &str = " ";

/// One input field — kind plus shared metadata.
#[derive(Debug, Clone)]
pub struct InputField {
    pub name: String,
    pub prompt: String,
    pub kind: FieldKind,
    pub required: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum FieldKind {
    Text {
        value: String,
        cursor_position: usize,
    },
    Select {
        options: Vec<String>,
        selected: usize,
    },
    MultiSelect {
        options: Vec<String>,
        checked: Vec<bool>,
        cursor: usize,
    },
}

impl InputField {
    fn from_definition(name: String, def: &InputDefinition) -> Self {
        match def {
            InputDefinition::Prompt(prompt) => Self {
                name,
                prompt: prompt.clone(),
                kind: FieldKind::Text {
                    value: String::new(),
                    cursor_position: 0,
                },
                required: false,
                error: None,
            },
            InputDefinition::Detailed(InputSpec::Text {
                prompt,
                default,
                required,
            }) => {
                let value = default.clone();
                let cursor_position = value.chars().count();
                Self {
                    name,
                    prompt: prompt.clone(),
                    kind: FieldKind::Text {
                        value,
                        cursor_position,
                    },
                    required: *required,
                    error: None,
                }
            }
            InputDefinition::Detailed(InputSpec::Select {
                prompt,
                options,
                default,
            }) => {
                let selected = default
                    .as_ref()
                    .and_then(|d| options.iter().position(|o| o == d))
                    .unwrap_or(0);
                Self {
                    name,
                    prompt: prompt.clone(),
                    kind: FieldKind::Select {
                        options: options.clone(),
                        selected,
                    },
                    // A select always has a value (empty options is rejected by validation,
                    // but we still treat select as not-required since there's nothing to enforce).
                    required: false,
                    error: None,
                }
            }
            InputDefinition::Detailed(InputSpec::MultiSelect {
                prompt,
                options,
                default,
                required,
            }) => {
                let checked: Vec<bool> = options.iter().map(|o| default.contains(o)).collect();
                Self {
                    name,
                    prompt: prompt.clone(),
                    kind: FieldKind::MultiSelect {
                        options: options.clone(),
                        checked,
                        cursor: 0,
                    },
                    required: *required,
                    error: None,
                }
            }
        }
    }

    /// Lines this field occupies in the modal (prompt + content + optional error).
    fn line_count(&self) -> usize {
        let body = match &self.kind {
            FieldKind::Text { .. } => 1,
            FieldKind::Select { options, .. } => options.len().max(1),
            FieldKind::MultiSelect { options, .. } => options.len().max(1),
        };
        // prompt + body + error line (always reserved when set)
        1 + body + if self.error.is_some() { 1 } else { 0 }
    }

    /// Collapsed string value for substitution.
    fn collected_value(&self) -> String {
        match &self.kind {
            FieldKind::Text { value, .. } => value.clone(),
            FieldKind::Select { options, selected } => {
                options.get(*selected).cloned().unwrap_or_default()
            }
            FieldKind::MultiSelect {
                options, checked, ..
            } => options
                .iter()
                .zip(checked.iter())
                .filter(|(_, c)| **c)
                .map(|(o, _)| o.as_str())
                .collect::<Vec<_>>()
                .join(MULTI_SELECT_JOIN),
        }
    }

    fn is_empty(&self) -> bool {
        match &self.kind {
            FieldKind::Text { value, .. } => value.is_empty(),
            FieldKind::Select { .. } => false,
            FieldKind::MultiSelect { checked, .. } => !checked.iter().any(|c| *c),
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

    /// Setup the form with fields built from input definitions.
    /// `order` controls field display order — names not in `order` are appended in iteration order.
    pub fn setup(
        &mut self,
        title: impl Into<String>,
        inputs: &HashMap<String, InputDefinition>,
        order: &[String],
    ) {
        self.title = title.into();
        let mut fields: Vec<InputField> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for name in order {
            if let Some(def) = inputs.get(name) {
                fields.push(InputField::from_definition(name.clone(), def));
                seen.insert(name.clone());
            }
        }
        for (name, def) in inputs {
            if !seen.contains(name) {
                fields.push(InputField::from_definition(name.clone(), def));
            }
        }
        self.fields = fields;
        self.focused_field = 0;
        self.submit_focused = false;
    }

    pub fn clear(&mut self) {
        self.fields.clear();
        self.focused_field = 0;
        self.submit_focused = false;
    }

    /// Get all field values as a map (string-collapsed per kind).
    pub fn get_values(&self) -> HashMap<String, String> {
        self.fields
            .iter()
            .map(|f| (f.name.clone(), f.collected_value()))
            .collect()
    }

    fn focused_kind(&self) -> Option<&FieldKind> {
        if self.submit_focused {
            return None;
        }
        self.fields.get(self.focused_field).map(|f| &f.kind)
    }

    /// True when up/down should move *within* the focused field's options
    /// rather than between fields.
    pub fn focused_field_uses_vertical_keys(&self) -> bool {
        matches!(
            self.focused_kind(),
            Some(FieldKind::Select { .. } | FieldKind::MultiSelect { .. })
        )
    }

    pub fn focused_field_is_multi_select(&self) -> bool {
        matches!(self.focused_kind(), Some(FieldKind::MultiSelect { .. }))
    }

    fn clear_errors(&mut self) {
        for f in &mut self.fields {
            f.error = None;
        }
    }

    pub fn handle_char(&mut self, c: char) {
        self.clear_errors();
        if self.submit_focused {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused_field) {
            if let FieldKind::Text {
                value,
                cursor_position,
            } = &mut field.kind
            {
                let byte_idx = char_byte_index(value, *cursor_position);
                value.insert(byte_idx, c);
                *cursor_position += 1;
            }
        }
    }

    pub fn handle_backspace(&mut self) {
        self.clear_errors();
        if self.submit_focused {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused_field) {
            if let FieldKind::Text {
                value,
                cursor_position,
            } = &mut field.kind
            {
                if *cursor_position > 0 {
                    *cursor_position -= 1;
                    let byte_idx = char_byte_index(value, *cursor_position);
                    value.remove(byte_idx);
                }
            }
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.submit_focused {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused_field) {
            if let FieldKind::Text {
                cursor_position, ..
            } = &mut field.kind
            {
                if *cursor_position > 0 {
                    *cursor_position -= 1;
                }
            }
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.submit_focused {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused_field) {
            if let FieldKind::Text {
                value,
                cursor_position,
            } = &mut field.kind
            {
                let max = value.chars().count();
                if *cursor_position < max {
                    *cursor_position += 1;
                }
            }
        }
    }

    /// Move option cursor up inside Select / MultiSelect.
    pub fn move_option_up(&mut self) {
        if self.submit_focused {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused_field) {
            match &mut field.kind {
                FieldKind::Select { selected, .. } => *selected = selected.saturating_sub(1),
                FieldKind::MultiSelect { cursor, .. } => *cursor = cursor.saturating_sub(1),
                _ => {}
            }
        }
    }

    pub fn move_option_down(&mut self) {
        if self.submit_focused {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused_field) {
            match &mut field.kind {
                FieldKind::Select { options, selected } => {
                    *selected = (*selected + 1).min(options.len().saturating_sub(1));
                }
                FieldKind::MultiSelect {
                    options, cursor, ..
                } => {
                    *cursor = (*cursor + 1).min(options.len().saturating_sub(1));
                }
                _ => {}
            }
        }
    }

    /// Toggle current option in a MultiSelect.
    pub fn toggle_multi_select(&mut self) {
        self.clear_errors();
        if self.submit_focused {
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused_field) {
            if let FieldKind::MultiSelect {
                checked, cursor, ..
            } = &mut field.kind
            {
                if let Some(c) = checked.get_mut(*cursor) {
                    *c = !*c;
                }
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

    /// Validate required fields. Returns true if all required fields are filled.
    /// Sets per-field `error` for any required-but-empty field.
    pub fn validate(&mut self) -> bool {
        let mut ok = true;
        for f in &mut self.fields {
            if f.required && f.is_empty() {
                f.error = Some("Required".to_string());
                ok = false;
            } else {
                f.error = None;
            }
        }
        ok
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
        // Total content rows: sum of fields (each field knows its own line count) + submit row + spacer
        let field_rows: usize = self.fields.iter().map(|f| f.line_count() + 1).sum(); // +1 spacer between fields
        let total_rows = field_rows + 2; // submit + hint
        let height_percent =
            ((total_rows + 4) * 100 / area.height.max(1) as usize).clamp(30, 80) as u16;
        let popup_area = Self::centered_rect(50, height_percent, area);

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

        // Build per-row constraints.
        let mut constraints: Vec<Constraint> = Vec::new();
        for f in &self.fields {
            for _ in 0..f.line_count() {
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Length(1)); // spacer
        }
        constraints.push(Constraint::Length(1)); // submit
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        let mut row = 0usize;
        for (i, field) in self.fields.iter().enumerate() {
            let is_focused = !self.submit_focused && i == self.focused_field;

            // Prompt row
            let prompt_style = if is_focused {
                self.styles.title
            } else {
                self.styles.muted_text
            };
            let mut prompt_text = field.prompt.clone();
            if field.required {
                prompt_text.push_str(" *");
            }
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(prompt_text, prompt_style))),
                chunks[row],
            );
            row += 1;

            // Body
            match &field.kind {
                FieldKind::Text {
                    value,
                    cursor_position,
                } => {
                    let input_style = if is_focused {
                        self.styles.normal_text.add_modifier(Modifier::UNDERLINED)
                    } else {
                        self.styles.normal_text
                    };
                    let display_value = if value.is_empty() && !is_focused {
                        "(empty)".to_string()
                    } else {
                        value.clone()
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(display_value, input_style))),
                        chunks[row],
                    );
                    if is_focused {
                        frame.set_cursor_position((
                            chunks[row].x + *cursor_position as u16,
                            chunks[row].y,
                        ));
                    }
                    row += 1;
                }
                FieldKind::Select { options, selected } => {
                    if options.is_empty() {
                        frame.render_widget(
                            Paragraph::new(Line::from(Span::styled(
                                "(no options)",
                                self.styles.muted_text,
                            ))),
                            chunks[row],
                        );
                        row += 1;
                    } else {
                        for (idx, opt) in options.iter().enumerate() {
                            let is_sel = idx == *selected;
                            let marker = if is_sel { "▶ " } else { "  " };
                            let style = if is_sel && is_focused {
                                self.styles.action_selected
                            } else if is_sel {
                                self.styles.title
                            } else {
                                self.styles.normal_text
                            };
                            frame.render_widget(
                                Paragraph::new(Line::from(Span::styled(
                                    format!("{}{}", marker, opt),
                                    style,
                                ))),
                                chunks[row],
                            );
                            row += 1;
                        }
                    }
                }
                FieldKind::MultiSelect {
                    options,
                    checked,
                    cursor,
                } => {
                    if options.is_empty() {
                        frame.render_widget(
                            Paragraph::new(Line::from(Span::styled(
                                "(no options)",
                                self.styles.muted_text,
                            ))),
                            chunks[row],
                        );
                        row += 1;
                    } else {
                        for (idx, opt) in options.iter().enumerate() {
                            let is_cursor = idx == *cursor;
                            let mark = if checked.get(idx).copied().unwrap_or(false) {
                                "[x]"
                            } else {
                                "[ ]"
                            };
                            let arrow = if is_cursor && is_focused {
                                "▶ "
                            } else {
                                "  "
                            };
                            let style = if is_cursor && is_focused {
                                self.styles.action_selected
                            } else {
                                self.styles.normal_text
                            };
                            frame.render_widget(
                                Paragraph::new(Line::from(Span::styled(
                                    format!("{}{} {}", arrow, mark, opt),
                                    style,
                                ))),
                                chunks[row],
                            );
                            row += 1;
                        }
                    }
                }
            }

            // Error row (if present)
            if let Some(err) = &field.error {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!("  ! {}", err),
                        self.styles.error_text,
                    ))),
                    chunks[row],
                );
                row += 1;
            }

            // Spacer
            row += 1;
        }

        // Submit button
        let submit_area = chunks[row];
        let submit_style = if self.submit_focused {
            self.styles.action_selected
        } else {
            self.styles.action_normal
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("[ Submit ]", submit_style))).centered(),
            submit_area,
        );

        // Hint at bottom of popup
        let hint_area = Rect::new(
            inner.x,
            popup_area.y + popup_area.height.saturating_sub(2),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "[Tab] Next  [Space] Toggle  [Enter] Submit  [Esc] Cancel",
                self.styles.muted_text,
            )))
            .centered(),
            hint_area,
        );
    }
}

impl Default for InputForm {
    fn default() -> Self {
        Self::new()
    }
}

fn char_byte_index(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn defs(pairs: Vec<(&str, InputDefinition)>) -> HashMap<String, InputDefinition> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn text_default_pre_fill() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "msg",
            InputDefinition::Detailed(InputSpec::Text {
                prompt: "Msg:".into(),
                default: "hello".into(),
                required: false,
            }),
        )]);
        form.setup("t", &inputs, &["msg".to_string()]);
        let v = form.get_values();
        assert_eq!(v.get("msg"), Some(&"hello".to_string()));
    }

    #[test]
    fn select_default_selection() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "env",
            InputDefinition::Detailed(InputSpec::Select {
                prompt: "Env:".into(),
                options: vec!["dev".into(), "staging".into(), "prod".into()],
                default: Some("staging".into()),
            }),
        )]);
        form.setup("t", &inputs, &["env".to_string()]);
        assert_eq!(form.get_values().get("env"), Some(&"staging".to_string()));
    }

    #[test]
    fn select_default_falls_back_to_first() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "env",
            InputDefinition::Detailed(InputSpec::Select {
                prompt: "Env:".into(),
                options: vec!["a".into(), "b".into()],
                default: None,
            }),
        )]);
        form.setup("t", &inputs, &["env".to_string()]);
        assert_eq!(form.get_values().get("env"), Some(&"a".to_string()));
    }

    #[test]
    fn multi_select_joins_with_space() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "feats",
            InputDefinition::Detailed(InputSpec::MultiSelect {
                prompt: "Features:".into(),
                options: vec!["auth".into(), "logging".into(), "metrics".into()],
                default: vec!["auth".into(), "metrics".into()],
                required: false,
            }),
        )]);
        form.setup("t", &inputs, &["feats".to_string()]);
        assert_eq!(
            form.get_values().get("feats"),
            Some(&"auth metrics".to_string())
        );
    }

    #[test]
    fn validate_rejects_empty_required_text() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "msg",
            InputDefinition::Detailed(InputSpec::Text {
                prompt: "Msg:".into(),
                default: "".into(),
                required: true,
            }),
        )]);
        form.setup("t", &inputs, &["msg".to_string()]);
        assert!(!form.validate());
    }

    #[test]
    fn validate_rejects_empty_required_multi_select() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "f",
            InputDefinition::Detailed(InputSpec::MultiSelect {
                prompt: "F:".into(),
                options: vec!["a".into(), "b".into()],
                default: vec![],
                required: true,
            }),
        )]);
        form.setup("t", &inputs, &["f".to_string()]);
        assert!(!form.validate());
    }

    #[test]
    fn shorthand_string_is_text_input() {
        let mut form = InputForm::new();
        let inputs = defs(vec![("x", InputDefinition::Prompt("Enter x:".into()))]);
        form.setup("t", &inputs, &["x".to_string()]);
        form.handle_char('a');
        form.handle_char('b');
        assert_eq!(form.get_values().get("x"), Some(&"ab".to_string()));
    }

    #[test]
    fn move_option_navigates_select() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "e",
            InputDefinition::Detailed(InputSpec::Select {
                prompt: "E:".into(),
                options: vec!["a".into(), "b".into(), "c".into()],
                default: None,
            }),
        )]);
        form.setup("t", &inputs, &["e".to_string()]);
        form.move_option_down();
        form.move_option_down();
        assert_eq!(form.get_values().get("e"), Some(&"c".to_string()));
        form.move_option_up();
        assert_eq!(form.get_values().get("e"), Some(&"b".to_string()));
    }

    #[test]
    fn toggle_multi_select_works() {
        let mut form = InputForm::new();
        let inputs = defs(vec![(
            "f",
            InputDefinition::Detailed(InputSpec::MultiSelect {
                prompt: "F:".into(),
                options: vec!["a".into(), "b".into(), "c".into()],
                default: vec![],
                required: false,
            }),
        )]);
        form.setup("t", &inputs, &["f".to_string()]);
        form.toggle_multi_select(); // toggle 'a'
        form.move_option_down();
        form.move_option_down();
        form.toggle_multi_select(); // toggle 'c'
        assert_eq!(form.get_values().get("f"), Some(&"a c".to_string()));
    }
}
