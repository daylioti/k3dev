use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::cluster::diagnostics::{DiagnosticStatus, DiagnosticsReport};
use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// Diagnostics overlay — displays cluster health check results as a checklist
pub struct DiagnosticsOverlay {
    styles: Styles,
    report: DiagnosticsReport,
    scroll_offset: usize,
    total_lines: usize,
}

impl DiagnosticsOverlay {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            styles: Styles::from_theme(theme),
            report: DiagnosticsReport {
                results: Vec::new(),
                finished: false,
            },
            scroll_offset: 0,
            total_lines: 0,
        }
    }

    /// Reset state for a new diagnostics run
    pub fn reset(&mut self) {
        self.report.results.clear();
        self.report.finished = false;
        self.scroll_offset = 0;
        self.total_lines = 0;
    }

    /// Update with new diagnostics state from background task
    pub fn update(&mut self, report: DiagnosticsReport) {
        self.report = report;
        // Recompute line count for scrolling
        self.total_lines = self.count_lines();
    }

    pub fn is_finished(&self) -> bool {
        self.report.finished
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    fn count_lines(&self) -> usize {
        if self.report.results.is_empty() {
            return 0;
        }
        let mut lines = 0;
        let mut current_cat = "";
        for r in &self.report.results {
            if r.category != current_cat {
                if !current_cat.is_empty() {
                    lines += 1; // blank line between categories
                }
                lines += 1; // category header
                current_cat = r.category;
            }
            lines += 1; // test line
        }
        lines
    }

    fn build_lines(&self) -> Vec<Line<'_>> {
        let mut lines: Vec<Line> = Vec::new();
        let mut current_cat = "";

        for result in &self.report.results {
            if result.category != current_cat {
                if !current_cat.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled(
                    format!("  \u{2500}\u{2500} {} \u{2500}\u{2500}", result.category),
                    self.styles.title,
                )));
                current_cat = result.category;
            }

            let (symbol, style) = match &result.status {
                DiagnosticStatus::Pending => ("[ ]", self.styles.muted_text),
                DiagnosticStatus::Running => ("[~]", self.styles.warning_text),
                DiagnosticStatus::Passed => ("[*]", self.styles.success_text),
                DiagnosticStatus::Failed(_) => ("[x]", self.styles.error_text),
                DiagnosticStatus::Skipped(_) => ("[-]", self.styles.muted_text),
            };

            let mut spans = vec![
                Span::styled(format!("  {} ", symbol), style),
                Span::styled(result.name.as_str(), self.styles.normal_text),
            ];

            // Duration for completed tests
            if let Some(dur) = result.duration {
                spans.push(Span::styled(
                    format!("  ({}ms)", dur.as_millis()),
                    self.styles.muted_text,
                ));
            }

            // Failure reason inline
            if let DiagnosticStatus::Failed(reason) = &result.status {
                spans.push(Span::styled(
                    format!(" - {}", reason),
                    self.styles.error_text,
                ));
            }

            // Skip reason inline
            if let DiagnosticStatus::Skipped(reason) = &result.status {
                spans.push(Span::styled(
                    format!(" - {}", reason),
                    self.styles.muted_text,
                ));
            }

            lines.push(Line::from(spans));
        }

        lines
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let popup_area = centered_rect(65, 75, area);

        frame.render_widget(Clear, popup_area);

        // Summary for title
        let passed = self
            .report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Passed))
            .count();
        let total = self.report.results.len();
        let failed = self
            .report
            .results
            .iter()
            .filter(|r| matches!(r.status, DiagnosticStatus::Failed(_)))
            .count();

        let summary = if self.report.finished {
            if failed == 0 {
                format!(" {}/{} passed ", passed, total)
            } else {
                format!(" {}/{} passed, {} failed ", passed, total, failed)
            }
        } else {
            " running... ".to_string()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(" Cluster Diagnostics ")
            .title_bottom(
                Line::from(vec![
                    Span::styled(
                        " Esc close ",
                        self.styles.muted_text,
                    ),
                    Span::styled(
                        " r re-run ",
                        self.styles.muted_text,
                    ),
                    Span::styled(
                        summary,
                        if failed > 0 {
                            self.styles.error_text
                        } else if self.report.finished {
                            self.styles.success_text
                        } else {
                            self.styles.warning_text
                        },
                    ),
                ])
                .right_aligned(),
            );

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        // Build content
        let lines = self.build_lines();

        // Handle scrolling
        let visible_height = inner.height as usize;
        let max_scroll = lines.len().saturating_sub(visible_height);
        let scroll = self.scroll_offset.min(max_scroll);

        let visible_lines: Vec<Line> = lines
            .into_iter()
            .skip(scroll)
            .take(visible_height)
            .collect();

        let paragraph = Paragraph::new(visible_lines);
        frame.render_widget(paragraph, inner);
    }
}

impl Default for DiagnosticsOverlay {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to create a centered rect
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
