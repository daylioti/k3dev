use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::config::UiConfig;

/// Calculated layout regions for the app
#[derive(Clone)]
pub struct AppLayout {
    pub action_bar: Rect,
    pub menu: Rect,
    pub pod_stats: Rect,
    pub status_bar: Rect,
}

impl AppLayout {
    /// Calculate layout from terminal area with default config
    #[allow(dead_code)]
    pub fn calculate(area: Rect) -> Self {
        Self::calculate_with_config(area, &UiConfig::default(), 0, 0)
    }

    /// Calculate layout from terminal area with UI config and menu width offset
    pub fn calculate_with_config(
        area: Rect,
        ui_config: &UiConfig,
        longest_menu_item: u16,
        menu_width_offset: i16,
    ) -> Self {
        // Vertical split: action bar | content | status bar
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Action bar (compact, no borders)
                Constraint::Min(10),   // Content area
                Constraint::Length(1), // Status bar
            ])
            .split(area);

        let action_bar = vertical[0];
        let content_area = vertical[1];
        let status_bar = vertical[2];

        // Calculate column widths
        let total_width = content_area.width;

        // Calculate base menu width from config
        let base_menu_width = ui_config
            .menu_width
            .calculate(total_width, longest_menu_item);

        // Apply menu width offset from user adjustments
        let menu_width = (base_menu_width as i32 + menu_width_offset as i32)
            .max(20)
            .min((total_width * 60 / 100) as i32) as u16;

        // Horizontal split for content: menu | pod_stats (full right panel)
        let horizontal = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(menu_width),
                Constraint::Min(0), // Right panel gets remaining space
            ])
            .split(content_area);

        Self {
            action_bar,
            menu: horizontal[0],
            pod_stats: horizontal[1],
            status_bar,
        }
    }
}

/// Helper to create a centered rect for modals
#[allow(dead_code)]
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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
