//! Configuration validation with comprehensive error and warning detection
//!
//! This module provides detailed validation of the configuration file,
//! detecting issues that range from hard errors to soft warnings.

mod checks;

use super::types::Config;

/// Result of configuration validation
#[derive(Debug, Default)]
pub struct ValidationResult {
    #[allow(dead_code)]
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationWarning>,
}

impl ValidationResult {
    #[allow(dead_code)]
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    #[allow(dead_code)]
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    #[allow(dead_code)]
    pub fn add_error(&mut self, error: ValidationError) {
        self.errors.push(error);
    }

    pub fn add_warning(&mut self, warning: ValidationWarning) {
        self.warnings.push(warning);
    }
}

/// Hard validation errors that should prevent loading
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ValidationError {
    MissingRequiredField {
        path: String,
        field: String,
    },
    InvalidValue {
        path: String,
        field: String,
        reason: String,
    },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::MissingRequiredField { path, field } => {
                write!(f, "{}: missing required field '{}'", path, field)
            }
            ValidationError::InvalidValue {
                path,
                field,
                reason,
            } => {
                write!(f, "{}: invalid value for '{}': {}", path, field, reason)
            }
        }
    }
}

/// Soft validation warnings shown on startup
#[derive(Debug, Clone)]
pub enum ValidationWarning {
    UnusedPlaceholder {
        name: String,
    },
    DuplicateCommandName {
        group: String,
        name: String,
    },
    SuspiciousPort {
        port: u16,
        reason: String,
    },
    EmptyCommandGroup {
        name: String,
    },
    UnresolvedPlaceholder {
        path: String,
        placeholder: String,
    },
    DuplicateKeybinding {
        key: String,
        actions: Vec<String>,
    },
    InvalidKeybindingSyntax {
        key: String,
        reason: String,
    },
    PortConflict {
        ports: Vec<u16>,
        description: String,
    },
}

impl std::fmt::Display for ValidationWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationWarning::UnusedPlaceholder { name } => {
                write!(f, "Placeholder '@{}' is defined but never used", name)
            }
            ValidationWarning::DuplicateCommandName { group, name } => {
                write!(f, "Duplicate command name '{}' in group '{}'", name, group)
            }
            ValidationWarning::SuspiciousPort { port, reason } => {
                write!(f, "Port {}: {}", port, reason)
            }
            ValidationWarning::EmptyCommandGroup { name } => {
                write!(f, "Command group '{}' has no commands", name)
            }
            ValidationWarning::UnresolvedPlaceholder { path, placeholder } => {
                write!(f, "{}: unresolved placeholder '@{}'", path, placeholder)
            }
            ValidationWarning::DuplicateKeybinding { key, actions } => {
                write!(
                    f,
                    "Key '{}' bound to multiple actions: {}",
                    key,
                    actions.join(", ")
                )
            }
            ValidationWarning::InvalidKeybindingSyntax { key, reason } => {
                write!(f, "Invalid keybinding '{}': {}", key, reason)
            }
            ValidationWarning::PortConflict { ports, description } => {
                write!(f, "Port conflict {:?}: {}", ports, description)
            }
        }
    }
}

/// Configuration validator
pub struct ConfigValidator<'a> {
    pub(super) config: &'a Config,
    pub(super) result: ValidationResult,
}

impl<'a> ConfigValidator<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            result: ValidationResult::default(),
        }
    }

    /// Run all validation checks
    pub fn validate(mut self) -> ValidationResult {
        self.check_port_conflicts();
        self.check_duplicate_command_names();
        self.check_unused_placeholders();
        self.check_unresolved_placeholders();
        self.check_empty_command_groups();
        self.check_suspicious_ports();
        self.check_keybinding_conflicts();
        self.result
    }
}

/// Validate keybinding syntax
pub(super) fn validate_key_syntax(key: &str) -> Result<(), String> {
    let parts: Vec<&str> = key.split('+').collect();

    if parts.is_empty() {
        return Err("empty keybinding".to_string());
    }

    let valid_modifiers = ["ctrl", "alt", "shift"];
    let valid_special_keys = [
        "enter",
        "esc",
        "escape",
        "tab",
        "backspace",
        "delete",
        "insert",
        "home",
        "end",
        "pageup",
        "pagedown",
        "up",
        "down",
        "left",
        "right",
        "f1",
        "f2",
        "f3",
        "f4",
        "f5",
        "f6",
        "f7",
        "f8",
        "f9",
        "f10",
        "f11",
        "f12",
        "space",
    ];

    for (i, part) in parts.iter().enumerate() {
        let lower = part.to_lowercase();
        let is_last = i == parts.len() - 1;

        if is_last {
            // Last part should be the actual key
            if lower.len() == 1 {
                // Single character is valid
                continue;
            }
            if valid_special_keys.contains(&lower.as_str()) {
                continue;
            }
            // Also allow modifiers as the key (e.g., just "Ctrl")
            if valid_modifiers.contains(&lower.as_str()) {
                continue;
            }
            return Err(format!("unknown key '{}'", part));
        } else {
            // Non-last parts should be modifiers
            if !valid_modifiers.contains(&lower.as_str()) {
                return Err(format!(
                    "invalid modifier '{}'; expected ctrl, alt, or shift",
                    part
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_key_syntax() {
        assert!(validate_key_syntax("q").is_ok());
        assert!(validate_key_syntax("Enter").is_ok());
        assert!(validate_key_syntax("Ctrl+c").is_ok());
        assert!(validate_key_syntax("Ctrl+Shift+p").is_ok());
        assert!(validate_key_syntax("F1").is_ok());
        assert!(validate_key_syntax("").is_err());
        assert!(validate_key_syntax("Foo+c").is_err());
    }
}
