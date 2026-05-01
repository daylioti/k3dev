use ratatui::style::{Modifier, Style};

use super::theme::{ColorPalette, Theme};

/// Pre-computed styles for the UI
#[derive(Clone)]
pub struct Styles {
    pub palette: ColorPalette,

    // Text styles
    pub normal_text: Style,
    pub selected: Style,
    pub group_header: Style,
    pub error_text: Style,
    pub success_text: Style,
    pub warning_text: Style,
    pub info_text: Style,
    pub muted_text: Style,
    pub title: Style,
    pub primary: Style,

    // Border styles
    pub border_focused: Style,
    pub border_unfocused: Style,

    // Panel title styles
    pub panel_title_focused: Style,
    pub panel_title_unfocused: Style,

    // Action bar
    pub action_normal: Style,
    pub action_selected: Style,
}

impl Styles {
    /// Create styles from a theme
    pub fn from_theme(theme: Theme) -> Self {
        let palette = theme.palette();
        Self::from_palette(palette)
    }

    /// Create styles from a color palette
    pub fn from_palette(palette: ColorPalette) -> Self {
        Self {
            normal_text: Style::default().fg(palette.text),
            selected: Style::default()
                .fg(palette.highlight)
                .add_modifier(Modifier::BOLD)
                .bg(palette.selection_bg),
            group_header: Style::default()
                .fg(palette.primary)
                .add_modifier(Modifier::BOLD),
            error_text: Style::default().fg(palette.error),
            success_text: Style::default().fg(palette.success),
            warning_text: Style::default().fg(palette.warning),
            info_text: Style::default().fg(palette.primary),
            muted_text: Style::default().fg(palette.muted),
            title: Style::default()
                .fg(palette.primary)
                .add_modifier(Modifier::BOLD),
            primary: Style::default().fg(palette.primary),

            border_focused: Style::default()
                .fg(palette.highlight)
                .add_modifier(Modifier::BOLD),
            border_unfocused: Style::default().fg(palette.border),

            panel_title_focused: Style::default()
                .fg(palette.primary)
                .add_modifier(Modifier::BOLD),
            panel_title_unfocused: Style::default().fg(palette.text),

            action_normal: Style::default().fg(palette.text),
            action_selected: Style::default()
                .fg(palette.highlight)
                .add_modifier(Modifier::BOLD)
                .bg(palette.selection_bg),

            palette,
        }
    }
}

impl Default for Styles {
    fn default() -> Self {
        Self::from_theme(Theme::default())
    }
}
