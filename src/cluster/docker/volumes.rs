//! Volume/PVC stats from Docker containers and filesystem
//!
//! Gathers PVC usage data by executing commands inside the k3s container to
//! scan the local-path-provisioner storage directory, and mapping PVC directories
//! to pods via Docker container mounts.

use anyhow::{Context, Result};
use std::collections::HashMap;

use super::DockerManager;

/// PVC directory name format: pvc-<uuid>_<namespace>_<pvc-name>
const PVC_DIR_PREFIX: &str = "pvc-";

/// Volume/PVC info gathered from Docker + filesystem
pub struct VolumeStats {
    pub pvc_name: String,
    pub namespace: String,
    pub used_bytes: u64,
    pub pods: Vec<String>,
}

impl DockerManager {
    /// Get volume stats by executing commands inside the k3s container and matching
    /// with Docker container mounts.
    ///
    /// 1. `docker exec` to list PVC directories and get sizes (avoids permission issues)
    /// 2. Call list_containers_with_mounts to map pods to PVCs
    pub async fn get_volume_stats(
        &self,
        container_name: &str,
        storage_path: &str,
    ) -> Result<Vec<VolumeStats>> {
        // List PVC directories and their sizes via docker exec
        // Using `du -sb` for each dir gives us byte-accurate sizes
        let output = self
            .exec_in_container(
                container_name,
                &[
                    "sh",
                    "-c",
                    &format!(
                        "cd {} 2>/dev/null && for d in {}*/; do [ -d \"$d\" ] && du -sb \"$d\" 2>/dev/null; done",
                        storage_path, PVC_DIR_PREFIX
                    ),
                ],
            )
            .await
            .context("Failed to list PVC directories in container")?;

        // Parse du output: "12345\tpvc-xxx_ns_name/\n"
        let pvc_entries: Vec<(String, u64)> = output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(2, '\t').collect();
                if parts.len() != 2 {
                    return None;
                }
                let size = parts[0].trim().parse::<u64>().ok()?;
                let dir_name = parts[1].trim().trim_end_matches('/').to_string();
                if dir_name.starts_with(PVC_DIR_PREFIX) {
                    Some((dir_name, size))
                } else {
                    None
                }
            })
            .collect();

        if pvc_entries.is_empty() {
            return Ok(Vec::new());
        }

        // Get pod-to-PVC mapping from Docker container mounts
        let container_mounts = self
            .list_containers_with_mounts("k8s_")
            .await
            .unwrap_or_default();

        // Build a map: PVC dir name -> list of pod names.
        // Mount sources use the PV name (pvc-<uuid>) not the full dir name (pvc-<uuid>_ns_name),
        // e.g. ".../kubernetes.io~local-volume/pvc-808866a4-..." so we match on the PV name part.
        let pv_to_dir: HashMap<&str, &str> = pvc_entries
            .iter()
            .filter_map(|(d, _)| {
                let pv_name = d.split('_').next()?;
                Some((pv_name, d.as_str()))
            })
            .collect();

        let mut pvc_pod_map: HashMap<String, Vec<String>> = HashMap::new();
        for cm in &container_mounts {
            for mount in &cm.mounts {
                for (&pv_name, &dir_name) in &pv_to_dir {
                    if mount.source.contains(pv_name) {
                        pvc_pod_map
                            .entry(dir_name.to_string())
                            .or_default()
                            .push(cm.pod_name.clone());
                        break;
                    }
                }
            }
        }
        // Deduplicate pod names per PVC
        for pods in pvc_pod_map.values_mut() {
            pods.sort();
            pods.dedup();
        }

        // Build VolumeStats
        let mut results = Vec::new();
        for (dir_name, size) in &pvc_entries {
            if let Some((namespace, pvc_name)) = parse_pvc_dir_name(dir_name) {
                results.push(VolumeStats {
                    pvc_name,
                    namespace,
                    used_bytes: *size,
                    pods: pvc_pod_map.remove(dir_name).unwrap_or_default(),
                });
            }
        }

        Ok(results)
    }
}

/// Parse a PVC directory name into (namespace, pvc_name).
///
/// Format: `pvc-<uuid>_<namespace>_<pvc-name>`
/// The UUID part contains hyphens but no underscores, so the first `_` after "pvc-" separates
/// the UUID from the namespace, and the second `_` separates namespace from PVC name.
/// PVC names may contain underscores, so we rejoin remaining parts.
fn parse_pvc_dir_name(dir_name: &str) -> Option<(String, String)> {
    // Split on '_' — first part is "pvc-<uuid>", second is namespace, rest is PVC name
    let parts: Vec<&str> = dir_name.splitn(3, '_').collect();
    if parts.len() < 3 {
        return None;
    }
    let namespace = parts[1].to_string();
    let pvc_name = parts[2].to_string();

    if namespace.is_empty() || pvc_name.is_empty() {
        return None;
    }

    Some((namespace, pvc_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pvc_dir_name() {
        let result = parse_pvc_dir_name("pvc-a1b2c3d4-e5f6-7890-abcd-ef1234567890_default_my-data");
        assert_eq!(result, Some(("default".to_string(), "my-data".to_string())));
    }

    #[test]
    fn test_parse_pvc_dir_name_with_underscores() {
        let result =
            parse_pvc_dir_name("pvc-a1b2c3d4-e5f6-7890-abcd-ef1234567890_kube-system_my_pvc_data");
        assert_eq!(
            result,
            Some(("kube-system".to_string(), "my_pvc_data".to_string()))
        );
    }

    #[test]
    fn test_parse_pvc_dir_name_invalid() {
        assert_eq!(parse_pvc_dir_name("pvc-abc"), None);
        assert_eq!(parse_pvc_dir_name("not-a-pvc"), None);
        assert_eq!(parse_pvc_dir_name("pvc-abc_ns_"), None);
        assert_eq!(parse_pvc_dir_name("pvc-abc__name"), None);
    }
}
