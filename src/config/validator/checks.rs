//! Validation check implementations
//!
//! This module contains all the individual validation checks for configuration.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};

use super::{ConfigValidator, ValidationWarning};
use crate::config::types::CommandEntry;

/// Lazy-compiled regex for extracting @placeholder names
static PLACEHOLDER_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"@(\w+)").expect("Invalid PLACEHOLDER_REGEX pattern"));

impl<'a> ConfigValidator<'a> {
    /// Check for port conflicts between http/https/api ports
    pub(super) fn check_port_conflicts(&mut self) {
        let infra = &self.config.infrastructure;
        let mut port_usage: HashMap<u16, Vec<&str>> = HashMap::new();

        port_usage
            .entry(infra.http_port)
            .or_default()
            .push("http_port");
        port_usage
            .entry(infra.https_port)
            .or_default()
            .push("https_port");
        port_usage
            .entry(infra.api_port)
            .or_default()
            .push("api_port");

        for (port, usages) in port_usage {
            if usages.len() > 1 {
                self.result.add_warning(ValidationWarning::PortConflict {
                    ports: vec![port],
                    description: format!(
                        "Port {} is used by multiple services: {}",
                        port,
                        usages.join(", ")
                    ),
                });
            }
        }
    }

    /// Check for duplicate command names within the same group
    pub(super) fn check_duplicate_command_names(&mut self) {
        for group in &self.config.commands {
            let mut seen_names: HashSet<String> = HashSet::new();
            self.check_duplicate_names_in_entries(&group.commands, &group.name, &mut seen_names);
        }
    }

    fn check_duplicate_names_in_entries(
        &mut self,
        entries: &[CommandEntry],
        group_name: &str,
        seen: &mut HashSet<String>,
    ) {
        for entry in entries {
            let lowercase_name = entry.name.to_lowercase();
            if seen.contains(&lowercase_name) {
                self.result
                    .add_warning(ValidationWarning::DuplicateCommandName {
                        group: group_name.to_string(),
                        name: entry.name.clone(),
                    });
            } else {
                seen.insert(lowercase_name);
            }

            // Check nested commands with separate namespace
            if !entry.commands.is_empty() {
                let mut nested_seen: HashSet<String> = HashSet::new();
                self.check_duplicate_names_in_entries(
                    &entry.commands,
                    &format!("{}/{}", group_name, entry.name),
                    &mut nested_seen,
                );
            }
        }
    }

    /// Check for placeholders defined but never used
    pub(super) fn check_unused_placeholders(&mut self) {
        if self.config.placeholders.is_empty() {
            return;
        }

        let mut used_placeholders: HashSet<String> = HashSet::new();

        // Scan all command entries for placeholder usage
        for group in &self.config.commands {
            self.collect_placeholder_usage(&group.commands, &mut used_placeholders);
        }

        // Report unused placeholders
        for name in self.config.placeholders.keys() {
            if !used_placeholders.contains(name) {
                self.result
                    .add_warning(ValidationWarning::UnusedPlaceholder { name: name.clone() });
            }
        }
    }

    fn collect_placeholder_usage(&self, entries: &[CommandEntry], used: &mut HashSet<String>) {
        for entry in entries {
            // Check entry name
            self.extract_placeholders(&entry.name, used);

            // Check exec config fields
            if let Some(exec) = &entry.exec {
                self.extract_placeholders(&exec.target.namespace, used);
                self.extract_placeholders(&exec.target.selector, used);
                self.extract_placeholders(&exec.target.pod_name, used);
                self.extract_placeholders(&exec.target.container, used);
                self.extract_placeholders(&exec.workdir, used);
                self.extract_placeholders(&exec.cmd, used);
            }

            // Recurse into nested commands
            self.collect_placeholder_usage(&entry.commands, used);
        }
    }

    fn extract_placeholders(&self, s: &str, used: &mut HashSet<String>) {
        for cap in PLACEHOLDER_REGEX.captures_iter(s) {
            if let Some(name) = cap.get(1) {
                used.insert(name.as_str().to_string());
            }
        }
    }

    /// Check for unresolved @placeholder patterns that don't match any definition
    pub(super) fn check_unresolved_placeholders(&mut self) {
        let defined: HashSet<&String> = self.config.placeholders.keys().collect();

        for group in &self.config.commands {
            self.check_unresolved_in_entries(&group.commands, &group.name, &defined);
        }
    }

    fn check_unresolved_in_entries(
        &mut self,
        entries: &[CommandEntry],
        path: &str,
        defined: &HashSet<&String>,
    ) {
        for entry in entries {
            let entry_path = format!("{}/{}", path, entry.name);

            // Check all fields for unresolved placeholders
            let fields_to_check: Vec<(&str, &str)> = if let Some(exec) = &entry.exec {
                vec![
                    ("target.namespace", &exec.target.namespace),
                    ("target.selector", &exec.target.selector),
                    ("target.pod_name", &exec.target.pod_name),
                    ("target.container", &exec.target.container),
                    ("workdir", &exec.workdir),
                    ("cmd", &exec.cmd),
                ]
            } else {
                vec![]
            };

            for (field, value) in fields_to_check {
                for cap in PLACEHOLDER_REGEX.captures_iter(value) {
                    if let Some(name) = cap.get(1) {
                        let placeholder_name = name.as_str().to_string();
                        // Skip if it's a defined placeholder or an input placeholder
                        if !defined.contains(&placeholder_name) {
                            // Check if it's in the input map (runtime input)
                            let is_input_placeholder = entry
                                .exec
                                .as_ref()
                                .map(|e| e.input.contains_key(&placeholder_name))
                                .unwrap_or(false);

                            if !is_input_placeholder {
                                self.result
                                    .add_warning(ValidationWarning::UnresolvedPlaceholder {
                                        path: format!("{}.{}", entry_path, field),
                                        placeholder: placeholder_name,
                                    });
                            }
                        }
                    }
                }
            }

            // Recurse into nested commands
            self.check_unresolved_in_entries(&entry.commands, &entry_path, defined);
        }
    }

    /// Check for empty command groups
    pub(super) fn check_empty_command_groups(&mut self) {
        for group in &self.config.commands {
            if group.commands.is_empty() {
                self.result
                    .add_warning(ValidationWarning::EmptyCommandGroup {
                        name: group.name.clone(),
                    });
            }
        }
    }

    /// Check for suspicious port configurations
    pub(super) fn check_suspicious_ports(&mut self) {
        let infra = &self.config.infrastructure;

        // Check for privileged ports
        if infra.http_port < 1024 && infra.http_port != 80 {
            self.result.add_warning(ValidationWarning::SuspiciousPort {
                port: infra.http_port,
                reason: "non-standard privileged port for HTTP".to_string(),
            });
        }

        if infra.https_port < 1024 && infra.https_port != 443 {
            self.result.add_warning(ValidationWarning::SuspiciousPort {
                port: infra.https_port,
                reason: "non-standard privileged port for HTTPS".to_string(),
            });
        }

        // Check for commonly conflicting ports
        let common_conflicts: HashMap<u16, &str> = [
            (22, "SSH"),
            (53, "DNS"),
            (3000, "common dev server"),
            (3306, "MySQL"),
            (5432, "PostgreSQL"),
            (5672, "RabbitMQ"),
            (6379, "Redis"),
            (8080, "common proxy/alt HTTP"),
            (9090, "Prometheus"),
            (27017, "MongoDB"),
        ]
        .into_iter()
        .collect();

        for port in [infra.http_port, infra.https_port, infra.api_port] {
            if let Some(service) = common_conflicts.get(&port) {
                // Only warn if it's not the expected port
                if (port == infra.http_port && port != 80)
                    || (port == infra.https_port && port != 443)
                    || (port == infra.api_port && port != 6443)
                {
                    self.result.add_warning(ValidationWarning::SuspiciousPort {
                        port,
                        reason: format!("commonly used by {}", service),
                    });
                }
            }
        }
    }

    /// Check for keybinding conflicts (if keybindings are configured)
    pub(super) fn check_keybinding_conflicts(&mut self) {
        if let Some(keybindings) = &self.config.keybindings {
            let mut bindings: HashMap<String, Vec<String>> = HashMap::new();

            // Collect all keybindings
            let builtin_bindings = [
                (&keybindings.quit, "quit"),
                (&keybindings.help, "help"),
                (&keybindings.refresh, "refresh"),
                (&keybindings.command_palette, "command_palette"),
                (&keybindings.update_hosts, "update_hosts"),
                (&keybindings.cancel, "cancel"),
                (&keybindings.move_up, "move_up"),
                (&keybindings.move_down, "move_down"),
                (&keybindings.move_left, "move_left"),
                (&keybindings.move_right, "move_right"),
                (&keybindings.toggle_focus, "toggle_focus"),
                (&keybindings.execute, "execute"),
            ];

            for (opt_key, action) in builtin_bindings {
                if let Some(key) = opt_key {
                    let normalized = key.to_lowercase();
                    bindings
                        .entry(normalized)
                        .or_default()
                        .push(action.to_string());
                }
            }

            // Add custom bindings
            for (key, command_path) in &keybindings.custom {
                let normalized = key.to_lowercase();
                bindings
                    .entry(normalized)
                    .or_default()
                    .push(format!("custom:{}", command_path));
            }

            // Report conflicts
            for (key, actions) in bindings {
                if actions.len() > 1 {
                    self.result
                        .add_warning(ValidationWarning::DuplicateKeybinding { key, actions });
                }
            }

            // Validate keybinding syntax
            self.validate_keybinding_syntax(keybindings);
        }
    }

    pub(super) fn validate_keybinding_syntax(
        &mut self,
        keybindings: &crate::config::types::KeybindingsConfig,
    ) {
        let all_keys: Vec<(&str, Option<&String>)> = vec![
            ("quit", keybindings.quit.as_ref()),
            ("help", keybindings.help.as_ref()),
            ("refresh", keybindings.refresh.as_ref()),
            ("command_palette", keybindings.command_palette.as_ref()),
            ("update_hosts", keybindings.update_hosts.as_ref()),
            ("cancel", keybindings.cancel.as_ref()),
            ("move_up", keybindings.move_up.as_ref()),
            ("move_down", keybindings.move_down.as_ref()),
            ("move_left", keybindings.move_left.as_ref()),
            ("move_right", keybindings.move_right.as_ref()),
            ("toggle_focus", keybindings.toggle_focus.as_ref()),
            ("execute", keybindings.execute.as_ref()),
        ];

        for (action, opt_key) in all_keys {
            if let Some(key) = opt_key {
                if let Err(reason) = super::validate_key_syntax(key) {
                    self.result
                        .add_warning(ValidationWarning::InvalidKeybindingSyntax {
                            key: format!("{} = {}", action, key),
                            reason,
                        });
                }
            }
        }

        for key in keybindings.custom.keys() {
            if let Err(reason) = super::validate_key_syntax(key) {
                self.result
                    .add_warning(ValidationWarning::InvalidKeybindingSyntax {
                        key: key.clone(),
                        reason,
                    });
            }
        }
    }
}
