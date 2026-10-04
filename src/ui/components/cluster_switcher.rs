use std::path::PathBuf;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::styles::Styles;
use crate::ui::theme::Theme;

/// One selectable cluster in the switcher
#[derive(Debug, Clone)]
pub struct SwitcherEntry {
    pub name: String,
    pub domain: String,
    pub api_port: u16,
    /// Config file that defines this cluster. `None` means the cluster exists in
    /// Docker but was never opened in the TUI, so there is nothing to switch to.
    pub config_path: Option<PathBuf>,
    pub running: bool,
    /// The cluster this App instance is currently showing
    pub current: bool,
}

/// Overlay for switching the whole app to a different cluster's config
pub struct ClusterSwitcher {
    styles: Styles,
    query: String,
    entries: Vec<SwitcherEntry>,
    filtered: Vec<usize>,
    selected_index: usize,
    /// Why the last Enter did nothing, shown in place. The App's `output` pane
    /// is not rendered on the running screen, so feedback has to live here.
    message: Option<String>,
}

impl ClusterSwitcher {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            styles: Styles::from_theme(theme),
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected_index: 0,
            message: None,
        }
    }

    /// Replace the entry list, ordered by the caller, and reset the query.
    /// Selection starts on the first entry that is not the current cluster, so
    /// the common case is open-and-Enter.
    pub fn set_entries(&mut self, entries: Vec<SwitcherEntry>) {
        self.entries = entries;
        self.query.clear();
        self.message = None;
        self.filtered = (0..self.entries.len()).collect();
        self.selected_index = self
            .entries
            .iter()
            .position(|e| !e.current && e.config_path.is_some())
            .unwrap_or(0);
    }

    /// Apply the running-cluster set once Docker has answered.
    ///
    /// A running cluster with no recorded config still gets a row — it is not
    /// switchable, but silently hiding a cluster that is demonstrably up would
    /// be more confusing than showing why it cannot be opened.
    pub fn apply_running(&mut self, running: &[String]) {
        for entry in &mut self.entries {
            entry.running = running.contains(&entry.name);
        }

        for name in running {
            if self.entries.iter().any(|e| &e.name == name) {
                continue;
            }
            self.entries.push(SwitcherEntry {
                name: name.clone(),
                domain: String::new(),
                api_port: 0,
                config_path: None,
                running: true,
                current: false,
            });
        }

        self.filter();
    }

    pub fn reset(&mut self) {
        self.query.clear();
        self.entries.clear();
        self.filtered.clear();
        self.selected_index = 0;
        self.message = None;
    }

    /// Explain, in place, why the selection could not be opened
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    pub fn handle_char(&mut self, c: char) {
        self.message = None;
        self.query.push(c);
        self.filter();
    }

    pub fn handle_backspace(&mut self) {
        self.message = None;
        self.query.pop();
        self.filter();
    }

    pub fn move_up(&mut self) {
        self.message = None;
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    pub fn move_down(&mut self) {
        self.message = None;
        if self.selected_index + 1 < self.filtered.len() {
            self.selected_index += 1;
        }
    }

    pub fn selected(&self) -> Option<&SwitcherEntry> {
        self.filtered
            .get(self.selected_index)
            .and_then(|&i| self.entries.get(i))
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                query.is_empty()
                    || e.name.to_lowercase().contains(&query)
                    || e.domain.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect();

        if self.selected_index >= self.filtered.len() {
            self.selected_index = 0;
        }
    }

    /// Widest name column across visible entries, for alignment
    fn name_width(&self) -> usize {
        self.filtered
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .map(|e| e.name.chars().count())
            .max()
            .unwrap_or(0)
            .max(4)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Height follows the list: 2 border rows + 2 query rows + one per entry,
        // plus a row for the message when one is showing
        let rows = self.filtered.len().max(1) as u16 + 4 + u16::from(self.message.is_some());
        let popup_area = centered_rect(52, rows, area);
        frame.render_widget(Clear, popup_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(self.styles.border_focused)
            .title(" Switch Cluster ")
            .title_bottom(" \u{2191}\u{2193} select  \u{23ce} switch  esc cancel ");

        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),                                 // Query line
                Constraint::Min(0),                                    // Entries
                Constraint::Length(u16::from(self.message.is_some())), // Message
            ])
            .split(inner);

        if let Some(message) = &self.message {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {}", message),
                    self.styles.warning_text,
                ))),
                chunks[2],
            );
        }

        let input = Paragraph::new(Line::from(vec![
            Span::styled("> ", self.styles.warning_text),
            Span::styled(
                &self.query,
                self.styles.normal_text.add_modifier(Modifier::UNDERLINED),
            ),
        ]));
        frame.render_widget(input, chunks[0]);
        frame.set_cursor_position((
            chunks[0].x + 2 + self.query.chars().count() as u16,
            chunks[0].y,
        ));

        if self.filtered.is_empty() {
            let empty = if self.entries.is_empty() {
                "  No clusters recorded yet \u{2014} open one with `k3dev -c <config>`"
            } else {
                "  No matching clusters"
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(empty, self.styles.muted_text))),
                chunks[1],
            );
            return;
        }

        let name_width = self.name_width();
        let visible_height = chunks[1].height as usize;
        let lines: Vec<Line> = self
            .filtered
            .iter()
            .take(visible_height)
            .enumerate()
            .map(|(row, &idx)| {
                self.render_entry(&self.entries[idx], row == self.selected_index, name_width)
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), chunks[1]);
    }

    fn render_entry(
        &self,
        entry: &SwitcherEntry,
        is_selected: bool,
        name_width: usize,
    ) -> Line<'_> {
        let mut spans = Vec::new();

        spans.push(if is_selected {
            Span::styled("\u{25b6} ", self.styles.warning_text)
        } else {
            Span::raw("  ")
        });

        // Status dot: filled when the cluster container is up
        if entry.config_path.is_none() {
            spans.push(Span::styled("\u{25cc} ", self.styles.muted_text));
        } else if entry.running {
            spans.push(Span::styled("\u{25cf} ", self.styles.success_text));
        } else {
            spans.push(Span::styled("\u{25cb} ", self.styles.muted_text));
        }

        let name_style = if is_selected {
            self.styles.selected
        } else if entry.current {
            self.styles.title
        } else {
            self.styles.normal_text
        };
        spans.push(Span::styled(
            format!("{:<width$}  ", entry.name, width = name_width),
            name_style,
        ));

        let domain = if entry.domain.is_empty() {
            "\u{2014}".to_string()
        } else {
            entry.domain.clone()
        };
        spans.push(Span::styled(
            format!("{:<14}", domain),
            self.styles.muted_text,
        ));

        let port = if entry.api_port == 0 {
            "\u{2014}".to_string()
        } else {
            format!(":{}", entry.api_port)
        };
        spans.push(Span::styled(format!("{:<7}", port), self.styles.muted_text));

        if entry.config_path.is_none() {
            spans.push(Span::styled("no config", self.styles.warning_text));
        } else if entry.current {
            spans.push(Span::styled("current", self.styles.muted_text));
        }

        Line::from(spans)
    }
}

impl Default for ClusterSwitcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Centered rect: `percent_x` of the width, `rows` tall (clamped to the area)
fn centered_rect(percent_x: u16, rows: u16, area: Rect) -> Rect {
    // Never narrower than the name/domain/port columns plus a status message
    let width = (area.width * percent_x / 100).max(64).clamp(1, area.width);
    let height = rows.clamp(1, area.height);

    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, current: bool, has_config: bool) -> SwitcherEntry {
        SwitcherEntry {
            name: name.to_string(),
            domain: format!("{}.dev", name),
            api_port: 6443,
            config_path: has_config.then(|| PathBuf::from("/tmp/x.yml")),
            running: false,
            current,
        }
    }

    #[test]
    fn selection_starts_on_a_switchable_cluster() {
        let mut switcher = ClusterSwitcher::new();
        switcher.set_entries(vec![
            entry("alpha", true, true),
            entry("beta", false, false),
            entry("two", false, true),
        ]);

        assert_eq!(switcher.selected().map(|e| e.name.as_str()), Some("two"));
    }

    #[test]
    fn filter_matches_name_and_domain() {
        let mut switcher = ClusterSwitcher::new();
        switcher.set_entries(vec![entry("alpha", true, true), entry("two", false, true)]);

        switcher.handle_char('t');
        switcher.handle_char('w');
        assert_eq!(switcher.filtered.len(), 1);
        assert_eq!(switcher.selected().map(|e| e.name.as_str()), Some("two"));

        switcher.handle_backspace();
        switcher.handle_backspace();
        assert_eq!(switcher.filtered.len(), 2);
    }

    #[test]
    fn running_flags_are_applied_by_name() {
        let mut switcher = ClusterSwitcher::new();
        switcher.set_entries(vec![entry("alpha", true, true), entry("two", false, true)]);
        switcher.apply_running(&["two".to_string()]);

        assert!(!switcher.entries[0].running);
        assert!(switcher.entries[1].running);
    }

    #[test]
    fn running_cluster_without_a_config_is_listed_but_not_switchable() {
        let mut switcher = ClusterSwitcher::new();
        switcher.set_entries(vec![entry("alpha", true, true)]);
        switcher.apply_running(&["alpha".to_string(), "ghost".to_string()]);

        let ghost = switcher
            .entries
            .iter()
            .find(|e| e.name == "ghost")
            .expect("unrecorded running cluster should be listed");
        assert!(ghost.running);
        assert!(ghost.config_path.is_none());
        assert_eq!(switcher.filtered.len(), 2);
    }
}
