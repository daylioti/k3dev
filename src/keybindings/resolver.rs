//! Keybinding resolution from key events to actions

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

use super::KeyBinding;
use crate::config::KeybindingsConfig;

/// Actions that can be triggered by keybindings
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyAction {
    // Application actions
    Quit,
    Help,
    Refresh,
    CommandPalette,
    UpdateHosts,
    Cancel,

    // Navigation actions
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    ToggleFocus,
    Execute,

    // Custom command by path (e.g., "Group Name/Command Name")
    CustomCommand(String),

    // No action bound to this key
    None,
}

/// Resolves key events to actions based on configuration
pub struct KeybindingResolver {
    bindings: HashMap<KeyBinding, KeyAction>,
    // Keep track of original binding strings for help display
    binding_display: HashMap<KeyAction, String>,
}

impl KeybindingResolver {
    /// Create a resolver with default keybindings
    pub fn new() -> Self {
        Self::with_defaults()
    }

    /// Create a resolver with default keybindings
    fn with_defaults() -> Self {
        let mut resolver = Self {
            bindings: HashMap::new(),
            binding_display: HashMap::new(),
        };

        // Register default bindings
        resolver.register_default("q", KeyAction::Quit);
        resolver.register_default("Esc", KeyAction::Quit);
        resolver.register_default("?", KeyAction::Help);
        resolver.register_default("r", KeyAction::Refresh);
        resolver.register_default(":", KeyAction::CommandPalette);
        resolver.register_default("H", KeyAction::UpdateHosts);
        resolver.register_default("Ctrl+c", KeyAction::Cancel);
        resolver.register_default("Ctrl+q", KeyAction::Quit);

        // Navigation defaults
        resolver.register_default("k", KeyAction::MoveUp);
        resolver.register_default("Up", KeyAction::MoveUp);
        resolver.register_default("j", KeyAction::MoveDown);
        resolver.register_default("Down", KeyAction::MoveDown);
        resolver.register_default("h", KeyAction::MoveLeft);
        resolver.register_default("Left", KeyAction::MoveLeft);
        resolver.register_default("l", KeyAction::MoveRight);
        resolver.register_default("Right", KeyAction::MoveRight);
        resolver.register_default("Tab", KeyAction::ToggleFocus);
        resolver.register_default("Enter", KeyAction::Execute);

        resolver
    }

    fn register_default(&mut self, key_str: &str, action: KeyAction) {
        if let Ok(binding) = KeyBinding::parse(key_str) {
            self.bindings.insert(binding, action.clone());
            self.binding_display
                .entry(action)
                .or_insert_with(|| key_str.to_string());
        }
    }

    /// Create a resolver from configuration, falling back to defaults
    pub fn from_config(config: Option<&KeybindingsConfig>) -> Self {
        let mut resolver = Self::with_defaults();

        if let Some(keybindings) = config {
            resolver.apply_config(keybindings);
        }

        resolver
    }

    fn apply_config(&mut self, config: &KeybindingsConfig) {
        // Built-in action remaps
        self.remap_action(&config.quit, KeyAction::Quit);
        self.remap_action(&config.help, KeyAction::Help);
        self.remap_action(&config.refresh, KeyAction::Refresh);
        self.remap_action(&config.command_palette, KeyAction::CommandPalette);
        self.remap_action(&config.update_hosts, KeyAction::UpdateHosts);
        self.remap_action(&config.cancel, KeyAction::Cancel);

        // Navigation remaps
        self.remap_action(&config.move_up, KeyAction::MoveUp);
        self.remap_action(&config.move_down, KeyAction::MoveDown);
        self.remap_action(&config.move_left, KeyAction::MoveLeft);
        self.remap_action(&config.move_right, KeyAction::MoveRight);
        self.remap_action(&config.toggle_focus, KeyAction::ToggleFocus);
        self.remap_action(&config.execute, KeyAction::Execute);

        // Custom command bindings
        for (key_str, command_path) in &config.custom {
            if let Ok(binding) = KeyBinding::parse(key_str) {
                self.bindings
                    .insert(binding, KeyAction::CustomCommand(command_path.clone()));
            }
        }
    }

    fn remap_action(&mut self, key_opt: &Option<String>, action: KeyAction) {
        if let Some(key_str) = key_opt {
            // Remove all existing bindings for this action
            self.bindings.retain(|_, v| v != &action);

            // Add new binding
            if let Ok(binding) = KeyBinding::parse(key_str) {
                self.bindings.insert(binding, action.clone());
                self.binding_display.insert(action, key_str.clone());
            }
        }
    }

    /// Resolve a key event to an action
    pub fn resolve(&self, code: KeyCode, modifiers: KeyModifiers) -> KeyAction {
        for (binding, action) in &self.bindings {
            if binding.matches(code, modifiers) {
                return action.clone();
            }
        }
        KeyAction::None
    }

    /// Get the display string for an action's keybinding
    pub fn get_binding_display(&self, action: &KeyAction) -> Option<&str> {
        self.binding_display.get(action).map(|s| s.as_str())
    }

    /// Get all keybindings for display in help
    #[allow(dead_code)]
    pub fn get_all_bindings(&self) -> Vec<(&KeyAction, &str)> {
        self.binding_display
            .iter()
            .map(|(action, display)| (action, display.as_str()))
            .collect()
    }

    /// Check if a specific action has a binding
    #[allow(dead_code)]
    pub fn has_binding(&self, action: &KeyAction) -> bool {
        self.bindings.values().any(|a| a == action)
    }
}

impl Default for KeybindingResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_bindings() {
        let resolver = KeybindingResolver::new();

        assert_eq!(
            resolver.resolve(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyAction::Quit
        );
        assert_eq!(
            resolver.resolve(KeyCode::Char('?'), KeyModifiers::NONE),
            KeyAction::Help
        );
        assert_eq!(
            resolver.resolve(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyAction::MoveDown
        );
        assert_eq!(
            resolver.resolve(KeyCode::Up, KeyModifiers::NONE),
            KeyAction::MoveUp
        );
    }

    #[test]
    fn test_config_remap() {
        let config = KeybindingsConfig {
            quit: Some("Ctrl+q".to_string()),
            help: Some("F1".to_string()),
            ..Default::default()
        };

        let resolver = KeybindingResolver::from_config(Some(&config));

        // Old binding should no longer work
        assert_eq!(
            resolver.resolve(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyAction::None
        );

        // New binding should work
        assert_eq!(
            resolver.resolve(KeyCode::Char('q'), KeyModifiers::CONTROL),
            KeyAction::Quit
        );
        assert_eq!(
            resolver.resolve(KeyCode::F(1), KeyModifiers::NONE),
            KeyAction::Help
        );
    }

    #[test]
    fn test_custom_command() {
        let mut custom = HashMap::new();
        custom.insert("Ctrl+d".to_string(), "Drupal/Clear Cache".to_string());

        let config = KeybindingsConfig {
            custom,
            ..Default::default()
        };

        let resolver = KeybindingResolver::from_config(Some(&config));

        match resolver.resolve(KeyCode::Char('d'), KeyModifiers::CONTROL) {
            KeyAction::CustomCommand(path) => assert_eq!(path, "Drupal/Clear Cache"),
            _ => panic!("expected CustomCommand action"),
        }
    }
}
