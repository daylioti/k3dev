use serde::Deserialize;
use std::collections::HashMap;

use crate::ui::Theme;

/// Root configuration structure
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    /// UI theme: "fallout", "cyberpunk", or "nord"
    #[serde(default)]
    pub theme: Theme,

    /// UI configuration
    #[serde(default)]
    pub ui: UiConfig,

    /// K8s client settings (kubeconfig, context)
    #[serde(default)]
    pub cluster: K8sClientConfig,

    #[serde(default)]
    pub infrastructure: InfrastructureConfig,

    #[serde(default)]
    pub placeholders: HashMap<String, String>,

    #[serde(default)]
    pub commands: Vec<CommandGroup>,

    #[serde(default)]
    pub hooks: HooksConfig,

    /// Custom keybindings
    #[serde(default)]
    pub keybindings: Option<KeybindingsConfig>,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Menu width configuration
#[derive(Debug, Clone, PartialEq)]
pub enum MenuWidth {
    /// Auto-calculate based on longest visible item
    Auto,
    /// Percentage of terminal width (e.g., 30)
    Percent(u16),
    /// Fixed number of characters
    Fixed(u16),
}

impl Default for MenuWidth {
    fn default() -> Self {
        MenuWidth::Percent(30)
    }
}

impl<'de> serde::Deserialize<'de> for MenuWidth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct MenuWidthVisitor;

        impl<'de> Visitor<'de> for MenuWidthVisitor {
            type Value = MenuWidth;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("\"auto\", a percentage string like \"30%\", or a number")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == "auto" {
                    Ok(MenuWidth::Auto)
                } else if value.ends_with('%') {
                    let percent_str = value.trim_end_matches('%');
                    let percent: u16 = percent_str.parse().map_err(de::Error::custom)?;
                    Ok(MenuWidth::Percent(percent))
                } else {
                    Err(de::Error::custom(
                        "expected \"auto\" or a percentage like \"30%\"",
                    ))
                }
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(MenuWidth::Fixed(value as u16))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value >= 0 {
                    Ok(MenuWidth::Fixed(value as u16))
                } else {
                    Err(de::Error::custom("menu width cannot be negative"))
                }
            }
        }

        deserializer.deserialize_any(MenuWidthVisitor)
    }
}

impl MenuWidth {
    /// Parse the menu width and return the actual width in characters
    pub fn calculate(&self, total_width: u16, longest_item: u16) -> u16 {
        match self {
            MenuWidth::Auto => {
                // Auto-expand based on longest item, with some padding
                (longest_item + 4).max(25).min(total_width / 2)
            }
            MenuWidth::Percent(percent) => {
                (total_width * percent / 100).max(25).min(total_width / 2)
            }
            MenuWidth::Fixed(width) => (*width).max(25).min(total_width / 2),
        }
    }
}

/// UI configuration options
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UiConfig {
    /// Menu panel width: "auto", "30%", or a fixed number
    #[serde(default)]
    pub menu_width: MenuWidth,
}

/// Keybinding configuration for customizing keyboard shortcuts
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KeybindingsConfig {
    // Built-in action remaps
    #[serde(default)]
    pub quit: Option<String>,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub refresh: Option<String>,
    #[serde(default)]
    pub command_palette: Option<String>,
    #[serde(default)]
    pub update_hosts: Option<String>,
    #[serde(default)]
    pub cancel: Option<String>,

    // Navigation
    #[serde(default)]
    pub move_up: Option<String>,
    #[serde(default)]
    pub move_down: Option<String>,
    #[serde(default)]
    pub move_left: Option<String>,
    #[serde(default)]
    pub move_right: Option<String>,
    #[serde(default)]
    pub toggle_focus: Option<String>,
    #[serde(default)]
    pub execute: Option<String>,

    /// Custom command bindings: key -> "Group Name/Command Name"
    #[serde(default)]
    pub custom: HashMap<String, String>,
}

/// Hook event types for cluster lifecycle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// After k3s API is responding, before traefik is deployed
    OnClusterAvailable,
    /// After traefik and core services are deployed
    OnServicesDeployed,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::OnClusterAvailable => "on_cluster_available",
            HookEvent::OnServicesDeployed => "on_services_deployed",
        }
    }
}

/// A single hook command to execute
#[derive(Debug, Clone, Deserialize)]
pub struct HookCommand {
    /// Name of the hook for display purposes
    pub name: String,

    /// Shell command to execute
    pub command: String,

    /// Working directory (supports ~ expansion)
    #[serde(default)]
    pub workdir: Option<String>,

    /// Additional environment variables for this hook
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Continue executing subsequent hooks if this one fails
    #[serde(default)]
    pub continue_on_error: bool,

    /// Timeout in seconds (default: 300)
    #[serde(default = "default_hook_timeout")]
    pub timeout: u64,
}

fn default_hook_timeout() -> u64 {
    300
}

/// Hooks configuration
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HooksConfig {
    /// Global environment variables for all hooks
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Hooks to run after k3s API is available
    #[serde(default)]
    pub on_cluster_available: Vec<HookCommand>,

    /// Hooks to run after services are deployed
    #[serde(default)]
    pub on_services_deployed: Vec<HookCommand>,
}

impl HooksConfig {
    /// Check if any hooks are configured
    pub fn has_hooks(&self) -> bool {
        !self.on_cluster_available.is_empty() || !self.on_services_deployed.is_empty()
    }

    /// Get hooks for a specific event
    pub fn get_hooks(&self, event: HookEvent) -> &[HookCommand] {
        match event {
            HookEvent::OnClusterAvailable => &self.on_cluster_available,
            HookEvent::OnServicesDeployed => &self.on_services_deployed,
        }
    }
}

/// Kubernetes client configuration (kubeconfig path and context)
/// Note: This is separate from cluster::ClusterConfig which contains infrastructure settings.
/// These values get merged into cluster::ClusterConfig at runtime.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct K8sClientConfig {
    #[serde(default)]
    pub kubeconfig: String,

    #[serde(default)]
    pub context: String,
}

/// Infrastructure configuration
#[derive(Debug, Clone, Deserialize)]
pub struct InfrastructureConfig {
    /// Cluster name - used to derive container and network names
    #[serde(default = "default_cluster_name")]
    pub cluster_name: String,

    /// Domain for the local cluster
    #[serde(default = "default_domain")]
    pub domain: String,

    /// K3s version to use (e.g., "v1.33.4-k3s1")
    #[serde(default = "default_k3s_version")]
    pub k3s_version: String,

    /// Kubernetes API port
    #[serde(default = "default_api_port")]
    pub api_port: u16,

    /// HTTP port
    #[serde(default = "default_http_port")]
    pub http_port: u16,

    /// HTTPS port
    #[serde(default = "default_https_port")]
    pub https_port: u16,

    /// Additional port mappings (host:container format)
    #[serde(default)]
    pub additional_ports: Vec<String>,

    /// Speedup optimizations configuration
    #[serde(default)]
    pub speedup: SpeedupConfig,
}

/// Speedup optimization configuration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpeedupConfig {
    /// Enable snapshot-based startup (faster subsequent starts)
    /// Default: true - snapshots are enabled by default for optimal performance
    #[serde(default = "default_true")]
    pub use_snapshot: bool,

    /// Automatically cleanup old snapshots when creating new ones
    /// Default: true - only keeps the current snapshot
    #[serde(default = "default_true")]
    pub snapshot_auto_cleanup: bool,
}

impl Default for SpeedupConfig {
    fn default() -> Self {
        Self {
            use_snapshot: true,
            snapshot_auto_cleanup: true,
        }
    }
}

fn default_true() -> bool {
    true
}

impl InfrastructureConfig {
    /// Get container name derived from cluster name
    pub fn container_name(&self) -> String {
        format!("{}-server", self.cluster_name)
    }

    /// Get network name derived from cluster name
    pub fn network_name(&self) -> String {
        format!("{}-net", self.cluster_name)
    }
}

// Default value functions
fn default_cluster_name() -> String {
    "k3dev".to_string()
}

fn default_domain() -> String {
    "local.k8s.dev".to_string()
}

fn default_k3s_version() -> String {
    "v1.33.4-k3s1".to_string()
}

fn default_api_port() -> u16 {
    6443
}

fn default_http_port() -> u16 {
    80
}

fn default_https_port() -> u16 {
    443
}

impl Default for InfrastructureConfig {
    fn default() -> Self {
        Self {
            cluster_name: default_cluster_name(),
            domain: default_domain(),
            k3s_version: default_k3s_version(),
            api_port: default_api_port(),
            http_port: default_http_port(),
            https_port: default_https_port(),
            additional_ports: vec!["2345:2345".to_string(), "8309:8309".to_string()],
            speedup: SpeedupConfig::default(),
        }
    }
}

/// A group of related commands in the menu
#[derive(Debug, Clone, Deserialize)]
pub struct CommandGroup {
    pub name: String,

    #[serde(default)]
    pub icon: String,

    #[serde(default)]
    pub commands: Vec<CommandEntry>,
}

/// A single executable command or submenu
#[derive(Debug, Clone, Deserialize)]
pub struct CommandEntry {
    pub name: String,

    /// Optional description for command palette display
    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub exec: Option<ExecConfig>,

    #[serde(default)]
    pub commands: Vec<CommandEntry>,
}

/// How to execute a command in a pod
#[derive(Debug, Clone, Deserialize)]
pub struct ExecConfig {
    pub target: TargetConfig,

    #[serde(default)]
    pub workdir: String,

    pub cmd: String,

    #[serde(default)]
    pub input: HashMap<String, String>,
}

/// Which pod to target
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TargetConfig {
    #[serde(default)]
    pub namespace: String,

    #[serde(default)]
    pub selector: String,

    #[serde(default)]
    pub pod_name: String,

    #[serde(default)]
    pub container: String,
}

/// Logging configuration
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    /// Enable file logging
    #[serde(default = "default_logging_enabled")]
    pub enabled: bool,

    /// Log file path template (supports {cluster_name} placeholder)
    /// Default: /tmp/k3dev-{cluster_name}.log
    #[serde(default = "default_log_file")]
    pub file: String,

    /// Log level: trace, debug, info, warn, error
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: default_logging_enabled(),
            file: default_log_file(),
            level: default_log_level(),
        }
    }
}

fn default_logging_enabled() -> bool {
    true
}

fn default_log_file() -> String {
    "/tmp/k3dev-{cluster_name}.log".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}
