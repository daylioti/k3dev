use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::types::{CommandEntry, CommandGroup, Config, ExecConfig};

/// Configuration file loader
pub struct ConfigLoader {
    config_path: Option<PathBuf>,
}

impl ConfigLoader {
    pub fn new(config_path: Option<&str>) -> Self {
        Self {
            config_path: config_path.map(PathBuf::from),
        }
    }

    /// Load and parse the configuration file
    pub fn load(&self) -> Result<Config> {
        let path = self.find_config_file()?;

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config: Config = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        // Resolve placeholders throughout the config
        self.resolve_placeholders(&mut config);

        // Validate the configuration
        self.validate(&config)?;

        Ok(config)
    }

    /// Search for configuration file in standard locations
    fn find_config_file(&self) -> Result<PathBuf> {
        // If explicit path provided, use it
        if let Some(path) = &self.config_path {
            let expanded = expand_home(path)?;
            if expanded.exists() {
                return Ok(expanded);
            }
            return Err(anyhow!("Config file not found: {}", path.display()));
        }

        // Search in standard locations
        let mut search_paths = vec![PathBuf::from("./k3dev.yml"), PathBuf::from("./k3dev.yaml")];

        // Add user config directory
        if let Some(config_dir) = dirs::config_dir() {
            search_paths.push(config_dir.join("k3dev").join("config.yml"));
            search_paths.push(config_dir.join("k3dev").join("config.yaml"));
        }

        // Add system config
        search_paths.push(PathBuf::from("/etc/k3dev/config.yml"));
        search_paths.push(PathBuf::from("/etc/k3dev/config.yaml"));

        for path in search_paths {
            if path.exists() {
                return Ok(path);
            }
        }

        Err(anyhow!("No configuration file found in standard locations"))
    }

    /// Replace @placeholder patterns with values from placeholders map
    fn resolve_placeholders(&self, config: &mut Config) {
        if config.placeholders.is_empty() {
            return;
        }

        let placeholders = config.placeholders.clone();

        for group in &mut config.commands {
            self.resolve_command_group(group, &placeholders);
        }
    }

    fn resolve_command_group(
        &self,
        group: &mut CommandGroup,
        placeholders: &HashMap<String, String>,
    ) {
        group.name = self.replace_placeholders(&group.name, placeholders);

        for entry in &mut group.commands {
            self.resolve_command_entry(entry, placeholders);
        }
    }

    fn resolve_command_entry(
        &self,
        entry: &mut CommandEntry,
        placeholders: &HashMap<String, String>,
    ) {
        entry.name = self.replace_placeholders(&entry.name, placeholders);

        if let Some(exec) = &mut entry.exec {
            exec.target.namespace = self.replace_placeholders(&exec.target.namespace, placeholders);
            exec.target.selector = self.replace_placeholders(&exec.target.selector, placeholders);
            exec.target.pod_name = self.replace_placeholders(&exec.target.pod_name, placeholders);
            exec.target.container = self.replace_placeholders(&exec.target.container, placeholders);
            exec.workdir = self.replace_placeholders(&exec.workdir, placeholders);
            exec.cmd = self.replace_placeholders(&exec.cmd, placeholders);
        }

        // Recurse into nested commands
        for nested in &mut entry.commands {
            self.resolve_command_entry(nested, placeholders);
        }
    }

    fn replace_placeholders(&self, s: &str, placeholders: &HashMap<String, String>) -> String {
        if s.is_empty() {
            return s.to_string();
        }

        let mut result = s.to_string();
        for (key, value) in placeholders {
            let pattern = format!("@{}", key);
            result = result.replace(&pattern, value);
        }
        result
    }

    /// Validate the configuration
    fn validate(&self, config: &Config) -> Result<()> {
        for group in &config.commands {
            if group.name.is_empty() {
                return Err(anyhow!("Command group must have a name"));
            }

            for cmd in &group.commands {
                self.validate_command_entry(cmd, &group.name)?;
            }
        }

        Ok(())
    }

    fn validate_command_entry(&self, entry: &CommandEntry, group_name: &str) -> Result<()> {
        if entry.name.is_empty() {
            return Err(anyhow!(
                "In group '{}': command must have a name",
                group_name
            ));
        }

        if let Some(exec) = &entry.exec {
            if exec.cmd.is_empty() {
                return Err(anyhow!(
                    "In group '{}': command '{}': exec.cmd is required",
                    group_name,
                    entry.name
                ));
            }

            // Must have either selector or pod_name (unless it has input placeholders)
            if exec.target.selector.is_empty()
                && exec.target.pod_name.is_empty()
                && !has_input_placeholders(&exec.target.selector)
                && !has_input_placeholders(&exec.target.pod_name)
            {
                return Err(anyhow!(
                    "In group '{}': command '{}': target must specify selector or pod_name",
                    group_name,
                    entry.name
                ));
            }
        }

        // Validate nested commands
        for nested in &entry.commands {
            self.validate_command_entry(nested, group_name)?;
        }

        Ok(())
    }
}

/// Expand ~ to home directory
pub fn expand_home(path: &Path) -> Result<PathBuf> {
    let path_str = path.to_string_lossy();
    if let Some(stripped) = path_str.strip_prefix('~') {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
        let rest = stripped.strip_prefix('/').unwrap_or(stripped);
        Ok(home.join(rest))
    } else {
        Ok(path.to_path_buf())
    }
}

/// Check if a string contains unresolved @placeholder patterns
pub fn has_input_placeholders(s: &str) -> bool {
    let re = Regex::new(r"@\w+").unwrap();
    re.is_match(s)
}

/// Extract all @placeholder names from a string
pub fn get_input_placeholders(s: &str) -> Vec<String> {
    let re = Regex::new(r"@(\w+)").unwrap();
    let mut placeholders = Vec::new();
    let mut seen = HashSet::new();

    for cap in re.captures_iter(s) {
        if let Some(name) = cap.get(1) {
            let name_str = name.as_str().to_string();
            if !seen.contains(&name_str) {
                seen.insert(name_str.clone());
                placeholders.push(name_str);
            }
        }
    }

    placeholders
}

/// Get all placeholders from an ExecConfig (checking all fields)
pub fn get_exec_placeholders(exec: &ExecConfig) -> Vec<String> {
    let mut all_placeholders = Vec::new();
    let mut seen = HashSet::new();

    let fields = [
        &exec.target.namespace,
        &exec.target.selector,
        &exec.target.pod_name,
        &exec.target.container,
        &exec.workdir,
        &exec.cmd,
    ];

    for field in fields {
        for placeholder in get_input_placeholders(field) {
            if !seen.contains(&placeholder) {
                seen.insert(placeholder.clone());
                all_placeholders.push(placeholder);
            }
        }
    }

    all_placeholders
}
