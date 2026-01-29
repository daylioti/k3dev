//! Keybinding configuration and resolution
//!
//! This module provides user-configurable keybindings with support for
//! custom command shortcuts and action remapping.

mod resolver;

pub use resolver::{KeyAction, KeybindingResolver};

use crossterm::event::{KeyCode, KeyModifiers};

/// A parsed key binding that can be matched against key events
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyBinding {
    /// Create a new keybinding
    #[allow(dead_code)]
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Parse a keybinding string into a KeyBinding
    ///
    /// Supported formats:
    /// - Single character: "q", "j", "k"
    /// - Special keys: "Enter", "Esc", "Tab", "F1"-"F12"
    /// - With modifiers: "Ctrl+c", "Alt+x", "Shift+Tab"
    /// - Multiple modifiers: "Ctrl+Shift+p"
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split('+').collect();

        if parts.is_empty() {
            return Err("empty keybinding".to_string());
        }

        let mut modifiers = KeyModifiers::NONE;

        let key_part: &str = if parts.len() == 1 {
            parts[0]
        } else {
            // Parse modifiers
            for part in &parts[..parts.len() - 1] {
                match part.to_lowercase().as_str() {
                    "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                    "alt" => modifiers |= KeyModifiers::ALT,
                    "shift" => modifiers |= KeyModifiers::SHIFT,
                    _ => return Err(format!("unknown modifier: {}", part)),
                }
            }
            parts[parts.len() - 1]
        };

        let code = Self::parse_key_code(key_part)?;

        Ok(Self { code, modifiers })
    }

    fn parse_key_code(s: &str) -> Result<KeyCode, String> {
        // Single character
        if s.len() == 1 {
            let c = s.chars().next().unwrap();
            return Ok(KeyCode::Char(c.to_ascii_lowercase()));
        }

        // Special keys (case-insensitive)
        match s.to_lowercase().as_str() {
            "enter" | "return" => Ok(KeyCode::Enter),
            "esc" | "escape" => Ok(KeyCode::Esc),
            "tab" => Ok(KeyCode::Tab),
            "backtab" => Ok(KeyCode::BackTab),
            "backspace" => Ok(KeyCode::Backspace),
            "delete" | "del" => Ok(KeyCode::Delete),
            "insert" | "ins" => Ok(KeyCode::Insert),
            "home" => Ok(KeyCode::Home),
            "end" => Ok(KeyCode::End),
            "pageup" | "pgup" => Ok(KeyCode::PageUp),
            "pagedown" | "pgdn" => Ok(KeyCode::PageDown),
            "up" | "arrowup" => Ok(KeyCode::Up),
            "down" | "arrowdown" => Ok(KeyCode::Down),
            "left" | "arrowleft" => Ok(KeyCode::Left),
            "right" | "arrowright" => Ok(KeyCode::Right),
            "space" => Ok(KeyCode::Char(' ')),
            "f1" => Ok(KeyCode::F(1)),
            "f2" => Ok(KeyCode::F(2)),
            "f3" => Ok(KeyCode::F(3)),
            "f4" => Ok(KeyCode::F(4)),
            "f5" => Ok(KeyCode::F(5)),
            "f6" => Ok(KeyCode::F(6)),
            "f7" => Ok(KeyCode::F(7)),
            "f8" => Ok(KeyCode::F(8)),
            "f9" => Ok(KeyCode::F(9)),
            "f10" => Ok(KeyCode::F(10)),
            "f11" => Ok(KeyCode::F(11)),
            "f12" => Ok(KeyCode::F(12)),
            _ => Err(format!("unknown key: {}", s)),
        }
    }

    /// Check if this binding matches the given key event
    pub fn matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // Normalize the code for comparison (handle case-insensitivity)
        let normalized_code = match code {
            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };

        let self_code = match self.code {
            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };

        self_code == normalized_code && self.modifiers == modifiers
    }
}

impl std::fmt::Display for KeyBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();

        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("Alt");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("Shift");
        }

        let key_str = match self.code {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "Enter".to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "BackTab".to_string(),
            KeyCode::Backspace => "Backspace".to_string(),
            KeyCode::Delete => "Delete".to_string(),
            KeyCode::Insert => "Insert".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::PageUp => "PageUp".to_string(),
            KeyCode::PageDown => "PageDown".to_string(),
            KeyCode::Up => "Up".to_string(),
            KeyCode::Down => "Down".to_string(),
            KeyCode::Left => "Left".to_string(),
            KeyCode::Right => "Right".to_string(),
            KeyCode::F(n) => format!("F{}", n),
            _ => "?".to_string(),
        };

        parts.push(&key_str);
        write!(f, "{}", parts.join("+"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_keys() {
        let binding = KeyBinding::parse("q").unwrap();
        assert_eq!(binding.code, KeyCode::Char('q'));
        assert_eq!(binding.modifiers, KeyModifiers::NONE);

        let binding = KeyBinding::parse("Enter").unwrap();
        assert_eq!(binding.code, KeyCode::Enter);

        let binding = KeyBinding::parse("F1").unwrap();
        assert_eq!(binding.code, KeyCode::F(1));
    }

    #[test]
    fn test_parse_with_modifiers() {
        let binding = KeyBinding::parse("Ctrl+c").unwrap();
        assert_eq!(binding.code, KeyCode::Char('c'));
        assert_eq!(binding.modifiers, KeyModifiers::CONTROL);

        let binding = KeyBinding::parse("Ctrl+Shift+p").unwrap();
        assert_eq!(binding.code, KeyCode::Char('p'));
        assert!(binding.modifiers.contains(KeyModifiers::CONTROL));
        assert!(binding.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn test_matches() {
        let binding = KeyBinding::parse("Ctrl+c").unwrap();
        assert!(binding.matches(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(binding.matches(KeyCode::Char('C'), KeyModifiers::CONTROL)); // case-insensitive
        assert!(!binding.matches(KeyCode::Char('c'), KeyModifiers::NONE));
    }

    #[test]
    fn test_display() {
        let binding = KeyBinding::parse("Ctrl+Shift+p").unwrap();
        let display = binding.to_string();
        assert!(display.contains("Ctrl"));
        assert!(display.contains("Shift"));
        assert!(display.contains("p"));
    }
}
