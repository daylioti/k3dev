//! Persisted cluster → config-file map.
//!
//! A k3dev cluster is defined by a whole config file (menu, hooks, info blocks,
//! ports), not just a kube context, so switching clusters at runtime means
//! finding the YAML that defines the target. Nothing else on disk records that
//! link, so every config the TUI opens is remembered here.
//!
//! Kept separate from `clusters.json` on purpose: that file pins each cluster's
//! CIDR index and a corrupt or reshaped copy would silently re-index a running
//! cluster onto a different subnet. This one is pure UI convenience — losing it
//! only shortens the switcher list.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// What the switcher needs to render and open one cluster
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub config_path: PathBuf,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub api_port: u16,
    /// Unix seconds, used to order the switcher most-recent-first
    #[serde(default)]
    pub last_used: u64,
}

fn registry_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".k3dev")
        .join("configs.json")
}

fn load() -> BTreeMap<String, ConfigEntry> {
    std::fs::read_to_string(registry_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(map: &BTreeMap<String, ConfigEntry>) {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(map) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(error = %e, path = %path.display(), "Failed to persist config registry");
            }
        }
        Err(e) => tracing::warn!(error = %e, "Failed to serialize config registry"),
    }
}

/// Every cluster the TUI has opened, name → config entry.
pub fn all() -> BTreeMap<String, ConfigEntry> {
    load()
}

/// Remember which config file defines `cluster_name`.
///
/// The path is absolutised: the switcher may be opened from a different working
/// directory than the one the config was originally loaded from.
pub fn record(cluster_name: &str, config_path: &Path, domain: &str, api_port: u16) {
    let config_path =
        std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let last_used = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut map = load();
    map.insert(
        cluster_name.to_string(),
        ConfigEntry {
            config_path,
            domain: domain.to_string(),
            api_port,
            last_used,
        },
    );
    save(&map);
}
