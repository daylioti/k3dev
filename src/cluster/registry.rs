//! Persisted per-cluster index registry.
//!
//! Each cluster gets a stable small integer used to derive non-overlapping
//! pod/service CIDRs. The index must be persisted (not hashed from the name)
//! so two clusters can never silently collide on the same subnet.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Highest index that still yields a valid `10.{42+2i}.0.0/16` pair
pub const MAX_INDEX: u16 = 100;

fn registry_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".k3dev")
        .join("clusters.json")
}

fn load() -> BTreeMap<String, u16> {
    std::fs::read_to_string(registry_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the registry atomically (temp file + rename).
///
/// A half-written file parses as empty, which would re-index every cluster onto
/// a different subnet on the next start.
fn save(map: &BTreeMap<String, u16>) {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = match serde_json::to_string_pretty(map) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize cluster registry");
            return;
        }
    };

    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, json) {
        tracing::warn!(error = %e, path = %tmp.display(), "Failed to persist cluster registry");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!(error = %e, path = %path.display(), "Failed to persist cluster registry");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Lowest index not already reserved, capped at [`MAX_INDEX`].
fn lowest_free(used: &[u16]) -> u16 {
    (0..=MAX_INDEX)
        .find(|i| !used.contains(i))
        .unwrap_or(MAX_INDEX)
}

/// Resolve the index for `cluster_name`, allocating the lowest free one on
/// first use. Called once at config construction, so blocking I/O is fine.
pub fn resolve_index(cluster_name: &str) -> u16 {
    let mut map = load();

    if let Some(idx) = map.get(cluster_name) {
        return *idx;
    }

    let used: Vec<u16> = map.values().copied().collect();
    let index = lowest_free(&used);

    map.insert(cluster_name.to_string(), index);
    save(&map);
    index
}

/// Record a hand-pinned `cluster_index` so that auto-allocation for other
/// clusters avoids it. Without this an index pinned in a config file is
/// invisible here and the next cluster is handed the same one, which means two
/// clusters on identical pod/service CIDRs.
///
/// Out-of-range values are clamped: `42 + 2 * index` has to stay a valid octet,
/// and in a debug build it would otherwise overflow.
pub fn reserve_index(cluster_name: &str, index: u16) -> u16 {
    let index = if index > MAX_INDEX {
        tracing::warn!(
            cluster = %cluster_name,
            requested = index,
            max = MAX_INDEX,
            "cluster_index out of range, clamping"
        );
        MAX_INDEX
    } else {
        index
    };

    let mut map = load();
    if map.get(cluster_name) != Some(&index) {
        map.insert(cluster_name.to_string(), index);
        save(&map);
    }
    index
}

/// Every known cluster name → index reservation.
pub fn all() -> BTreeMap<String, u16> {
    load()
}

/// Drop a cluster's index reservation (called on `delete`).
pub fn release_index(cluster_name: &str) {
    let mut map = load();
    if map.remove(cluster_name).is_some() {
        save(&map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allocator must never hand out an index that is already taken —
    /// including one another cluster pinned by hand in its config file, which
    /// is why pinned indices are written to the registry too.
    #[test]
    fn lowest_free_index_skips_every_reservation() {
        assert_eq!(lowest_free(&[]), 0);
        assert_eq!(lowest_free(&[0, 2]), 1);
        assert_eq!(lowest_free(&[1, 0]), 2);
        // Exhausted: never returns an index that would overflow the octet
        let all: Vec<u16> = (0..=MAX_INDEX).collect();
        assert_eq!(lowest_free(&all), MAX_INDEX);
    }
}
