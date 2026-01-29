use ratatui::style::{Modifier, Style};

use super::theme::{ColorPalette, Theme};

/// Pre-computed styles for the UI
#[derive(Clone)]
pub struct Styles {
    #[allow(dead_code)]
    pub palette: ColorPalette,

    // Text styles
    pub normal_text: Style,
    pub selected: Style,
    pub group_header: Style,
    pub error_text: Style,
    pub success_text: Style,
    pub warning_text: Style,
    pub muted_text: Style,
    pub title: Style,
    pub primary: Style,

    // Border styles
    pub border_focused: Style,
    pub border_unfocused: Style,
    #[allow(dead_code)]
    pub border_secondary: Style,

    // Panel title styles
    pub panel_title_focused: Style,
    pub panel_title_unfocused: Style,

    // Action bar
    pub action_normal: Style,
    pub action_selected: Style,
    pub action_disabled: Style,

    // Status indicators
    pub status_connected: Style,
    pub status_disconnected: Style,
    pub status_unknown: Style,
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
            muted_text: Style::default().fg(palette.muted),
            title: Style::default()
                .fg(palette.primary)
                .add_modifier(Modifier::BOLD),
            primary: Style::default().fg(palette.primary),

            border_focused: Style::default().fg(palette.primary),
            border_unfocused: Style::default().fg(palette.border),
            border_secondary: Style::default().fg(palette.muted),

            panel_title_focused: Style::default()
                .fg(palette.primary)
                .add_modifier(Modifier::BOLD),
            panel_title_unfocused: Style::default().fg(palette.text),

            action_normal: Style::default().fg(palette.text),
            action_selected: Style::default()
                .fg(palette.highlight)
                .add_modifier(Modifier::BOLD)
                .bg(palette.selection_bg),
            action_disabled: Style::default().fg(palette.muted),

            status_connected: Style::default().fg(palette.success),
            status_disconnected: Style::default().fg(palette.error),
            status_unknown: Style::default().fg(palette.warning),

            palette,
        }
    }
}

impl Default for Styles {
    fn default() -> Self {
        Self::from_theme(Theme::default())
    }
}
