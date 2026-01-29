//! Event handling for keyboard and mouse input
//!
//! This module contains all event handling logic for the App.

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::config::RefreshTask;
use crate::keybindings::KeyAction;

use super::{App, AppMode, FocusArea};

impl App {
    pub(super) fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Use keybinding resolver to determine action
        let action = self.keybinding_resolver.resolve(code, modifiers);

        // Handle Quit action first - should always work regardless of mode
        if matches!(action, KeyAction::Quit) && self.mode == AppMode::Normal {
            self.should_quit = true;
            return;
        }

        // Handle Cancel action (Ctrl+C) - cancels execution or quits
        if matches!(action, KeyAction::Cancel) {
            if let Some(token) = self.cancel_token.take() {
                token.cancel();
                self.output.add_warning("Cancelling...");
                self.is_executing = false;
                self.status_bar.set_executing(false);
            } else {
                self.should_quit = true;
            }
            return;
        }

        // Handle input mode separately (modal - doesn't use resolver)
        if self.mode == AppMode::Input {
            self.handle_input_key(code, modifiers);
            return;
        }

        // Handle sudo password input mode (modal)
        if self.mode == AppMode::SudoPassword {
            self.handle_sudo_password_key(code);
            return;
        }

        // Handle help mode (modal)
        if self.mode == AppMode::Help {
            let visible_height = self
                .current_layout
                .as_ref()
                .map(|l| (l.pod_stats.height as usize).saturating_sub(4))
                .unwrap_or(20);

            match code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.mode = AppMode::Normal;
                    self.help_overlay.reset_scroll();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_overlay.scroll_up();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_overlay.scroll_down();
                }
                KeyCode::PageUp => {
                    self.help_overlay.page_up(visible_height);
                }
                KeyCode::PageDown => {
                    self.help_overlay.page_down(visible_height);
                }
                KeyCode::Home => {
                    self.help_overlay.reset_scroll();
                }
                _ => {}
            }
            return;
        }

        // Handle output popup mode (modal)
        if self.mode == AppMode::OutputPopup {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.mode = AppMode::Normal;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.output_popup.scroll_up();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    // Get visible lines for scroll calculation
                    let visible = self
                        .current_layout
                        .as_ref()
                        .map(|_| 20) // approximate visible lines
                        .unwrap_or(20);
                    self.output_popup.scroll_down(visible);
                }
                _ => {}
            }
            return;
        }

        // Handle command palette mode (modal)
        if self.mode == AppMode::CommandPalette {
            match code {
                KeyCode::Esc => {
                    self.mode = AppMode::Normal;
                    self.command_palette.reset();
                }
                KeyCode::Enter => {
                    if let Some(cmd) = self.command_palette.selected_command() {
                        let cmd_id = cmd.id.clone();
                        // Record execution for recent commands before resetting
                        self.command_palette.record_execution(&cmd_id);
                        self.mode = AppMode::Normal;
                        self.command_palette.reset();
                        self.execute_palette_command(cmd_id);
                    }
                }
                KeyCode::Up => self.command_palette.move_up(),
                KeyCode::Down => self.command_palette.move_down(),
                KeyCode::Backspace => self.command_palette.handle_backspace(),
                KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.command_palette.move_down();
                }
                KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.command_palette.move_up();
                }
                KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.command_palette.move_down();
                }
                KeyCode::Char('p') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.command_palette.move_up();
                }
                KeyCode::Char(c) => self.command_palette.handle_char(c),
                _ => {}
            }
            return;
        }

        // Handle confirm destroy mode (modal)
        if self.mode == AppMode::ConfirmDestroy {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_destroy();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.cancel_destroy();
                }
                _ => {}
            }
            return;
        }

        // Handle pod context menu mode (modal)
        if self.mode == AppMode::PodContextMenu {
            match code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.mode = AppMode::Normal;
                    self.pod_context_menu.reset();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.pod_context_menu.move_up();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.pod_context_menu.move_down();
                }
                KeyCode::Enter => {
                    if let Some(action) = self.pod_context_menu.selected_action() {
                        self.execute_pod_context_action(action);
                    }
                }
                KeyCode::Char(c) => {
                    // Try shortcut keys
                    if let Some(action) = self.pod_context_menu.select_by_shortcut(c) {
                        self.execute_pod_context_action(action);
                    }
                }
                _ => {}
            }
            return;
        }

        // Don't handle other keys while executing
        if self.is_executing {
            return;
        }

        // Handle menu search mode
        if self.menu.is_search_mode() && self.focus == FocusArea::Content {
            match code {
                KeyCode::Esc => {
                    self.menu.exit_search_mode();
                }
                KeyCode::Enter => {
                    // Execute selected item and exit search mode
                    self.menu.exit_search_mode();
                    self.handle_enter();
                }
                KeyCode::Up | KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.menu.move_up();
                }
                KeyCode::Down | KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.menu.move_down();
                }
                KeyCode::Up => {
                    self.menu.move_up();
                }
                KeyCode::Down => {
                    self.menu.move_down();
                }
                KeyCode::Backspace => {
                    self.menu.search_handle_backspace();
                    // Exit search mode if query becomes empty
                    if self.menu.search_query().is_empty() {
                        self.menu.exit_search_mode();
                    }
                }
                KeyCode::Char(c) => {
                    self.menu.search_handle_char(c);
                }
                _ => {}
            }
            return;
        }

        // Handle panel resize shortcuts (+/- keys)
        if let KeyCode::Char(c) = code {
            match c {
                '+' | '=' => {
                    // Increase menu width (clamp to reasonable range)
                    self.menu_width_offset = (self.menu_width_offset + 2).min(40);
                    self.status_bar
                        .set_resize_hint(Some(self.menu_width_offset));
                    return;
                }
                '-' | '_' => {
                    // Decrease menu width (clamp to reasonable range)
                    self.menu_width_offset = (self.menu_width_offset - 2).max(-20);
                    self.status_bar
                        .set_resize_hint(Some(self.menu_width_offset));
                    return;
                }
                _ => {}
            }
        }

        // Handle quick focus keys (1/2/3) - only if not building a count prefix
        if let KeyCode::Char(c) = code {
            if self.pending_count.is_empty() {
                match c {
                    '1' => {
                        self.focus = FocusArea::Content;
                        return;
                    }
                    '2' => {
                        self.focus = FocusArea::PodStats;
                        return;
                    }
                    '3' => {
                        self.focus = FocusArea::ActionBar;
                        return;
                    }
                    _ => {}
                }
            }
        }

        // Handle digit keys for vim-style count prefix (4-9, 0)
        // 1-3 are handled above as quick focus when not in count mode
        if let KeyCode::Char(c) = code {
            if c.is_ascii_digit() {
                // If we already have digits, allow any digit
                // Otherwise, only allow 4-9, 0 (1-3 are quick focus)
                if !self.pending_count.is_empty() || !matches!(c, '1' | '2' | '3') {
                    self.pending_count.push(c);
                    self.status_bar
                        .set_pending_count(Some(self.pending_count.clone()));
                    return;
                }
            }
        }

        // Get the movement count (default to 1)
        let count = self.pending_count.parse::<usize>().unwrap_or(1).max(1);
        self.pending_count.clear();
        self.status_bar.set_pending_count(None);

        // Handle actions via keybinding resolver
        match action {
            KeyAction::Quit => {
                self.should_quit = true;
            }
            KeyAction::Help => {
                self.mode = AppMode::Help;
            }
            KeyAction::Refresh => {
                self.spawn_status_check();
                self.spawn_ingress_refresh();
                self.spawn_missing_hosts_check();
                self.spawn_port_forwards_check();
                // Reset scheduler timers for tasks we just triggered
                self.scheduler
                    .mark_run_multiple(&[RefreshTask::IngressRefresh, RefreshTask::HostsCheck]);
            }
            KeyAction::CommandPalette => {
                self.command_palette.reset();
                self.mode = AppMode::CommandPalette;
            }
            KeyAction::UpdateHosts => {
                self.trigger_manual_hosts_update();
            }
            KeyAction::MoveUp => {
                for _ in 0..count {
                    self.handle_up();
                }
            }
            KeyAction::MoveDown => {
                for _ in 0..count {
                    self.handle_down();
                }
            }
            KeyAction::MoveLeft => {
                self.handle_left();
            }
            KeyAction::MoveRight => {
                self.handle_right();
            }
            KeyAction::ToggleFocus => {
                self.cycle_focus();
            }
            KeyAction::Execute => {
                self.handle_enter();
            }
            KeyAction::CustomCommand(path) => {
                self.execute_custom_command(&path);
            }
            KeyAction::Cancel | KeyAction::None => {}
        }

        // Handle '/' to enter search mode in menu (after action handling to avoid conflicts)
        if let KeyCode::Char('/') = code {
            if self.focus == FocusArea::Content {
                self.menu.enter_search_mode();
            }
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Only handle clicks in normal mode
        if self.mode != AppMode::Normal || self.is_executing {
            return;
        }

        let layout = match &self.current_layout {
            Some(l) => l.clone(),
            None => return,
        };

        // Only handle left mouse button down
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }

        let (x, y) = (mouse.column, mouse.row);

        // Check if click is in action bar
        if y >= layout.action_bar.y && y < layout.action_bar.y + layout.action_bar.height {
            self.focus = FocusArea::ActionBar;
            if let Some(action_index) = self
                .action_bar
                .get_action_at_x(x.saturating_sub(layout.action_bar.x) as usize)
            {
                self.action_bar.select_index(action_index);
                if let Some(action) = self.action_bar.selected_action() {
                    self.execute_cluster_action(action);
                }
            }
        }
        // Check if click is in menu
        else if x >= layout.menu.x
            && x < layout.menu.x + layout.menu.width
            && y >= layout.menu.y
            && y < layout.menu.y + layout.menu.height
        {
            self.focus = FocusArea::Content;
            let menu_y = (y - layout.menu.y).saturating_sub(1) as usize;
            if self.menu.select_at_row(menu_y) {
                if let Some(item) = self.menu.selected_item() {
                    if item.has_children {
                        self.menu.toggle();
                    } else if let Some(cmd) = &item.command {
                        self.execute_command(cmd.clone());
                    }
                }
            }
        }
    }

    fn handle_input_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) {
        match code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.input_form.clear();
                self.pending_command = None;
                self.output.add_info("Input cancelled");
            }
            KeyCode::Tab => self.input_form.focus_next(),
            KeyCode::BackTab => self.input_form.focus_prev(),
            KeyCode::Up => self.input_form.focus_prev(),
            KeyCode::Down => self.input_form.focus_next(),
            KeyCode::Left => self.input_form.move_cursor_left(),
            KeyCode::Right => self.input_form.move_cursor_right(),
            KeyCode::Backspace => self.input_form.handle_backspace(),
            KeyCode::Enter => {
                if self.input_form.is_submit_focused() {
                    self.submit_input();
                } else {
                    self.input_form.focus_next();
                }
            }
            KeyCode::Char(c) => self.input_form.handle_char(c),
            _ => {}
        }
    }

    fn handle_sudo_password_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.password_input.clear();
                self.pending_hosts_update = false;
                self.output.add_warning("Operation cancelled");
            }
            KeyCode::Enter => {
                self.sudo_password = self.password_input.clone();
                self.password_input.clear();
                self.mode = AppMode::Normal;

                if self.pending_hosts_update {
                    self.pending_hosts_update = false;
                    self.do_manual_hosts_update();
                }
            }
            KeyCode::Backspace => {
                self.password_input.pop();
            }
            KeyCode::Char(c) => {
                self.password_input.push(c);
            }
            _ => {}
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            FocusArea::ActionBar => FocusArea::Content,
            FocusArea::Content => FocusArea::PodStats,
            FocusArea::PodStats => FocusArea::ActionBar,
        };
    }

    fn handle_up(&mut self) {
        match self.focus {
            FocusArea::Content => self.menu.move_up(),
            FocusArea::PodStats => self.pod_stats.scroll_up(),
            FocusArea::ActionBar => {}
        }
    }

    fn handle_down(&mut self) {
        match self.focus {
            FocusArea::Content => self.menu.move_down(),
            FocusArea::PodStats => {
                // Get visible lines for scroll calculation
                let visible = self
                    .current_layout
                    .as_ref()
                    .map(|l| l.pod_stats.height.saturating_sub(2) as usize)
                    .unwrap_or(20);
                self.pod_stats.scroll_down(visible);
            }
            FocusArea::ActionBar => {}
        }
    }

    fn handle_left(&mut self) {
        match self.focus {
            FocusArea::ActionBar => self.action_bar.move_left(),
            FocusArea::Content => self.menu.collapse(),
            FocusArea::PodStats => {}
        }
    }

    fn handle_right(&mut self) {
        match self.focus {
            FocusArea::ActionBar => self.action_bar.move_right(),
            FocusArea::Content => self.menu.expand(),
            FocusArea::PodStats => {}
        }
    }

    fn handle_enter(&mut self) {
        match self.focus {
            FocusArea::ActionBar => {
                if let Some(action) = self.action_bar.selected_action() {
                    self.execute_cluster_action(action);
                }
            }
            FocusArea::Content => {
                // Check if ingress path is selected
                if let Some(url) = self.menu.selected_ingress_url() {
                    // Open URL in browser
                    self.open_url(&url);
                    return;
                }

                if let Some(item) = self.menu.selected_item() {
                    if item.has_children {
                        self.menu.toggle();
                    } else if let Some(cmd) = &item.command {
                        self.execute_command(cmd.clone());
                    }
                }
            }
            FocusArea::PodStats => {
                // Show pod context menu if a pod is selected
                if let Some(pod) = self.pod_stats.selected_pod() {
                    self.pod_context_menu
                        .set_pod(pod.name.clone(), pod.namespace.clone());
                    self.mode = AppMode::PodContextMenu;
                }
            }
        }
    }

    /// Execute a pod context menu action
    fn execute_pod_context_action(&mut self, action: crate::ui::components::PodAction) {
        let pod_name = self.pod_context_menu.pod_name().to_string();
        let namespace = self.pod_context_menu.pod_namespace().to_string();

        self.mode = AppMode::Normal;
        self.pod_context_menu.reset();

        self.execute_pod_action(action, &pod_name, &namespace);
    }
}
