use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

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
    pub info_blocks: Vec<InfoBlock>,

    #[serde(default)]
    pub hooks: HooksConfig,

    /// Custom keybindings
    #[serde(default)]
    pub keybindings: Option<KeybindingsConfig>,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Packet-capture (tcpdump → pcap) settings
    #[serde(default)]
    pub capture: CaptureConfig,
}

/// Menu width configuration
#[derive(Debug, Clone, PartialEq, Default)]
pub enum MenuWidth {
    /// Auto-calculate based on longest visible item
    #[default]
    Auto,
    /// Percentage of terminal width (e.g., 30)
    Percent(u16),
    /// Fixed number of characters
    Fixed(u16),
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
                // Auto-expand based on longest item, with padding for borders only.
                // The scrollbar overlays the inner content area, so no extra column is needed.
                (longest_item + 2).max(22).min(total_width * 28 / 100)
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
    "v1.35.2-k3s1".to_string()
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

    /// Optional visibility gate — entry is hidden until the check passes.
    #[serde(default)]
    pub visible: Option<Visible>,
}

/// How to execute a command
#[derive(Debug, Clone, Deserialize)]
pub struct ExecConfig {
    pub target: ExecutionTarget,

    #[serde(default)]
    pub workdir: String,

    pub cmd: String,

    #[serde(default)]
    pub input: HashMap<String, InputDefinition>,
}

/// Definition for one runtime input prompt.
///
/// Two YAML shapes are accepted:
/// - bare string — shorthand for a plain text prompt
/// - tagged map — `{ type: text|select|multi-select, prompt, options, default, required }`
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InputDefinition {
    Prompt(String),
    Detailed(InputSpec),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum InputSpec {
    Text {
        prompt: String,
        #[serde(default)]
        default: String,
        #[serde(default)]
        required: bool,
    },
    Select {
        prompt: String,
        options: Vec<String>,
        #[serde(default)]
        default: Option<String>,
    },
    MultiSelect {
        prompt: String,
        options: Vec<String>,
        #[serde(default)]
        default: Vec<String>,
        #[serde(default)]
        required: bool,
    },
}

/// Where a command runs.
///
/// Tagged YAML representation; if `type:` is omitted, defaults to `kubernetes`:
/// ```yaml
/// target: { type: host }
/// target: { type: docker, container: "k3dev-server" }
/// target: { type: kubernetes, namespace: "default", selector: "app=foo" }
/// target: { namespace: "default", selector: "app=foo" }   # implicit kubernetes
/// ```
#[derive(Debug, Clone)]
pub enum ExecutionTarget {
    /// Run on the user's host shell.
    Host,
    /// `docker exec` into a named container.
    Docker { container: String },
    /// `kubectl exec` style — into a pod located by selector or name.
    Kubernetes {
        namespace: String,
        selector: String,
        pod_name: String,
        container: String,
    },
}

// Tagged shape used internally by the deserializer. The public `ExecutionTarget`
// converts from this — that lets us inject a default `type: kubernetes` when the
// user omits the discriminator.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ExecutionTargetTagged {
    Host,
    Docker {
        #[serde(default)]
        container: String,
    },
    Kubernetes {
        #[serde(default)]
        namespace: String,
        #[serde(default)]
        selector: String,
        #[serde(default)]
        pod_name: String,
        #[serde(default)]
        container: String,
    },
}

impl From<ExecutionTargetTagged> for ExecutionTarget {
    fn from(t: ExecutionTargetTagged) -> Self {
        match t {
            ExecutionTargetTagged::Host => ExecutionTarget::Host,
            ExecutionTargetTagged::Docker { container } => ExecutionTarget::Docker { container },
            ExecutionTargetTagged::Kubernetes {
                namespace,
                selector,
                pod_name,
                container,
            } => ExecutionTarget::Kubernetes {
                namespace,
                selector,
                pod_name,
                container,
            },
        }
    }
}

impl<'de> serde::Deserialize<'de> for ExecutionTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize into a free-form YAML value first so we can inject
        // `type: kubernetes` when the user omits the discriminator.
        let mut value = serde_yml::Value::deserialize(deserializer)?;

        if let serde_yml::Value::Mapping(map) = &mut value {
            let type_key = serde_yml::Value::String("type".into());
            if !map.contains_key(&type_key) {
                map.insert(type_key, serde_yml::Value::String("kubernetes".into()));
            }
        }

        let tagged: ExecutionTargetTagged =
            serde_yml::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(tagged.into())
    }
}

impl ExecutionTarget {
    /// Return the K8s target fields if this is a Kubernetes target.
    pub fn as_kubernetes(&self) -> Option<KubernetesTargetRef<'_>> {
        if let ExecutionTarget::Kubernetes {
            namespace,
            selector,
            pod_name,
            container,
        } = self
        {
            Some(KubernetesTargetRef {
                namespace,
                selector,
                pod_name,
                container,
            })
        } else {
            None
        }
    }
}

/// Borrowed view of a Kubernetes target's fields.
pub struct KubernetesTargetRef<'a> {
    pub namespace: &'a str,
    pub selector: &'a str,
    pub pod_name: &'a str,
    pub container: &'a str,
}

/// A user-configurable info block rendered at the bottom of the left sidebar.
///
/// Each block runs a script on its own schedule and displays the (trimmed)
/// output below a header. The script reuses the same execution targets as
/// custom commands.
#[derive(Debug, Clone, Deserialize)]
pub struct InfoBlock {
    pub name: String,

    #[serde(default)]
    pub icon: String,

    pub exec: ExecConfig,

    #[serde(
        default = "default_info_block_interval",
        deserialize_with = "deser_duration"
    )]
    pub interval: Duration,

    /// Keep only the last N lines of output. Applied before `max_length`.
    #[serde(default)]
    pub max_lines: Option<usize>,

    /// Hard character cap on output. Truncates on UTF-8 boundary.
    #[serde(default)]
    pub max_length: Option<usize>,

    /// Optional visibility gate — block is hidden until the check passes.
    #[serde(default)]
    pub visible: Option<Visible>,
}

fn default_info_block_interval() -> Duration {
    Duration::from_secs(30)
}

fn default_visible_interval() -> Duration {
    Duration::from_secs(5)
}

/// A conditional visibility gate — re-evaluated on its own interval.
///
/// Accepts several YAML shapes:
/// ```yaml
/// visible: "test -f /etc/hosts"                                  # host-shell shorthand
/// visible: { type: pod, namespace: default, selector: "app=x" }  # any matching pod exists
/// visible: { type: container, container: "k3dev-server" }        # docker container exists
/// visible: { type: exec, target: {...}, cmd: "..." }             # full ExecConfig, exit 0 = visible
/// ```
/// An optional `interval` (duration string) customises the re-check cadence; default 5s.
#[derive(Debug, Clone)]
pub struct Visible {
    pub check: VisibleCheck,
    pub interval: Duration,
}

#[derive(Debug, Clone)]
pub enum VisibleCheck {
    /// At least one pod matches the selector in the given namespace.
    Pod { namespace: String, selector: String },
    /// A docker container with this name exists on the host daemon.
    Container { container: String },
    /// Run a command; exit code 0 = visible.
    Exec(ExecConfig),
}

impl<'de> serde::Deserialize<'de> for Visible {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let value = serde_yml::Value::deserialize(deserializer)?;

        // Shorthand: plain string = host-shell probe.
        if let serde_yml::Value::String(cmd) = &value {
            return Ok(Visible {
                check: VisibleCheck::Exec(ExecConfig {
                    target: ExecutionTarget::Host,
                    workdir: String::new(),
                    cmd: cmd.clone(),
                    input: HashMap::new(),
                }),
                interval: default_visible_interval(),
            });
        }

        let mut map = match value {
            serde_yml::Value::Mapping(m) => m,
            other => {
                return Err(D::Error::custom(format!(
                    "visible: expected string or mapping, got {:?}",
                    other
                )));
            }
        };

        // Optional interval.
        let interval = match map.remove(serde_yml::Value::String("interval".into())) {
            Some(serde_yml::Value::String(raw)) => {
                parse_duration_str(&raw).map_err(D::Error::custom)?
            }
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "visible.interval: expected string, got {:?}",
                    other
                )));
            }
            None => default_visible_interval(),
        };

        let type_val = map
            .remove(serde_yml::Value::String("type".into()))
            .ok_or_else(|| D::Error::custom("visible: missing `type` field"))?;
        let type_str = match type_val {
            serde_yml::Value::String(s) => s,
            other => {
                return Err(D::Error::custom(format!(
                    "visible.type: expected string, got {:?}",
                    other
                )));
            }
        };

        let check = match type_str.as_str() {
            "pod" => {
                let namespace = take_string(&mut map, "namespace").map_err(D::Error::custom)?;
                let selector = take_string(&mut map, "selector").map_err(D::Error::custom)?;
                VisibleCheck::Pod {
                    namespace,
                    selector,
                }
            }
            "container" => {
                let container = take_string(&mut map, "container").map_err(D::Error::custom)?;
                VisibleCheck::Container { container }
            }
            "exec" => {
                // Feed the remaining keys back through ExecConfig's deserializer.
                let remaining = serde_yml::Value::Mapping(map);
                let exec: ExecConfig =
                    serde_yml::from_value(remaining).map_err(D::Error::custom)?;
                VisibleCheck::Exec(exec)
            }
            other => {
                return Err(D::Error::custom(format!(
                    "visible.type: unknown `{}` (expected pod|container|exec)",
                    other
                )));
            }
        };

        Ok(Visible { check, interval })
    }
}

fn take_string(map: &mut serde_yml::Mapping, key: &str) -> Result<String, String> {
    match map.remove(serde_yml::Value::String(key.into())) {
        None => Ok(String::new()),
        Some(serde_yml::Value::String(s)) => Ok(s),
        Some(other) => Err(format!("visible.{}: expected string, got {:?}", key, other)),
    }
}

/// Parse duration strings like "1s", "500ms", "5m", "2h" into `Duration`.
/// Minimum accepted value is 1s (or 1000ms).
pub(crate) fn deser_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let raw = String::deserialize(deserializer)?;
    parse_duration_str(&raw).map_err(D::Error::custom)
}

fn parse_duration_str(raw: &str) -> Result<Duration, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }

    let (num_str, unit, scale_ms) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, "ms", 1u64)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, "s", 1000)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, "m", 60_000)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, "h", 3_600_000)
    } else {
        return Err(format!(
            "invalid duration \"{}\" (expected Nms/Ns/Nm/Nh)",
            raw
        ));
    };

    let n: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid number in duration \"{}\"", raw))?;

    let total_ms = n.checked_mul(scale_ms).ok_or("duration overflow")?;
    if total_ms < 1000 {
        return Err(format!(
            "duration \"{}\" below 1s minimum (got {}{})",
            raw, n, unit
        ));
    }
    Ok(Duration::from_millis(total_ms))
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

/// Packet-capture settings (tcpdump sidecar → .pcap files).
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureConfig {
    /// Directory where captured .pcap files are written.
    /// Default: `<XDG_DATA_HOME>/k3dev/captures` (or platform equivalent).
    #[serde(default = "default_capture_output_dir")]
    pub output_dir: std::path::PathBuf,

    /// Sidecar Docker image used to run tcpdump.
    /// Default: `nicolaka/netshoot`.
    #[serde(default = "default_capture_image")]
    pub image: String,

    /// Default network interface for tcpdump (`-i`).
    #[serde(default = "default_capture_iface")]
    pub iface: String,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            output_dir: default_capture_output_dir(),
            image: default_capture_image(),
            iface: default_capture_iface(),
        }
    }
}

fn default_capture_output_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .map(|d| d.join("k3dev").join("captures"))
        .unwrap_or_else(|| std::env::temp_dir().join("k3dev-captures"))
}

fn default_capture_image() -> String {
    "nicolaka/netshoot".to_string()
}

fn default_capture_iface() -> String {
    "any".to_string()
}

fn default_log_file() -> String {
    let tmp = std::env::temp_dir();
    format!("{}/k3dev-{{cluster_name}}.log", tmp.display())
}

fn default_log_level() -> String {
    "info".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parses_seconds() {
        assert_eq!(parse_duration_str("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration_str("30s").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn duration_parses_minutes_and_hours() {
        assert_eq!(parse_duration_str("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration_str("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn duration_parses_millis_at_or_above_1s() {
        assert_eq!(
            parse_duration_str("1000ms").unwrap(),
            Duration::from_millis(1000)
        );
    }

    #[test]
    fn duration_rejects_below_1s() {
        assert!(parse_duration_str("500ms").is_err());
        assert!(parse_duration_str("0s").is_err());
    }

    #[test]
    fn duration_rejects_garbage() {
        assert!(parse_duration_str("").is_err());
        assert!(parse_duration_str("abc").is_err());
        assert!(parse_duration_str("5").is_err());
        assert!(parse_duration_str("5x").is_err());
    }

    #[test]
    fn visible_string_shorthand_parses_as_host_exec() {
        let v: Visible = serde_yml::from_str(r#""test -f /etc/hosts""#).unwrap();
        assert_eq!(v.interval, default_visible_interval());
        match v.check {
            VisibleCheck::Exec(exec) => {
                assert!(matches!(exec.target, ExecutionTarget::Host));
                assert_eq!(exec.cmd, "test -f /etc/hosts");
            }
            _ => panic!("expected Exec variant"),
        }
    }

    #[test]
    fn visible_pod_type_parses() {
        let yaml = r#"
type: pod
namespace: default
selector: "app=mysql"
"#;
        let v: Visible = serde_yml::from_str(yaml).unwrap();
        match v.check {
            VisibleCheck::Pod {
                namespace,
                selector,
            } => {
                assert_eq!(namespace, "default");
                assert_eq!(selector, "app=mysql");
            }
            _ => panic!("expected Pod variant"),
        }
    }

    #[test]
    fn visible_container_type_parses() {
        let v: Visible = serde_yml::from_str(r#"{ type: container, container: "x" }"#).unwrap();
        match v.check {
            VisibleCheck::Container { container } => assert_eq!(container, "x"),
            _ => panic!("expected Container variant"),
        }
    }

    #[test]
    fn visible_exec_type_with_custom_interval() {
        let yaml = r#"
type: exec
interval: "10s"
target: { type: host }
cmd: "uname -s"
"#;
        let v: Visible = serde_yml::from_str(yaml).unwrap();
        assert_eq!(v.interval, Duration::from_secs(10));
        match v.check {
            VisibleCheck::Exec(exec) => {
                assert!(matches!(exec.target, ExecutionTarget::Host));
                assert_eq!(exec.cmd, "uname -s");
            }
            _ => panic!("expected Exec variant"),
        }
    }

    #[test]
    fn visible_rejects_unknown_type() {
        let err = serde_yml::from_str::<Visible>(r#"{ type: bogus }"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown"), "unexpected error: {msg}");
    }
}
