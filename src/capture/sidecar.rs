//! Builds `ContainerRunConfig` for the tcpdump sidecar.
//!
//! The sidecar joins the target's network namespace via
//! `NetworkMode = container:<target-id>` and runs `tcpdump -i <iface> -U -s 0
//! -w - [filter]`. Stdout is the raw pcap stream; stderr is tcpdump status.

use std::collections::HashMap;

use crate::cluster::ContainerRunConfig;

/// Label key used on every sidecar so we can identify them later.
pub const CAPTURE_LABEL: &str = "k3dev.capture";

/// Build the tcpdump command-line.
///
/// `-U` flushes per-packet so the streaming reader sees data immediately.
/// `-s 0` removes the snaplen cap (full packets).
/// `-w -` writes pcap to stdout.
pub fn build_tcpdump_cmd(iface: &str, filter: Option<&str>) -> Vec<String> {
    let mut cmd: Vec<String> = vec![
        "tcpdump".to_string(),
        "-i".to_string(),
        iface.to_string(),
        "-U".to_string(),
        "-s".to_string(),
        "0".to_string(),
        "-w".to_string(),
        "-".to_string(),
    ];
    if let Some(f) = filter {
        let f = f.trim();
        if !f.is_empty() {
            cmd.push(f.to_string());
        }
    }
    cmd
}

/// Build a `ContainerRunConfig` for a sidecar that captures traffic from the
/// given target container's network namespace.
///
/// `target_container_id` is anything bollard accepts as a container reference
/// (name or hex id). `description` is a human-readable label for the
/// `k3dev.target` annotation.
pub fn build_capture_config(
    sidecar_name: &str,
    target_container_id: &str,
    image: &str,
    iface: &str,
    filter: Option<&str>,
    description: &str,
) -> ContainerRunConfig {
    let mut labels = HashMap::new();
    labels.insert(CAPTURE_LABEL.to_string(), "true".to_string());
    labels.insert("k3dev.target".to_string(), description.to_string());

    ContainerRunConfig {
        name: sidecar_name.to_string(),
        hostname: None,
        image: image.to_string(),
        detach: true,
        privileged: true,
        ports: Vec::new(),
        volumes: Vec::new(),
        env: Vec::new(),
        network: Some(format!("container:{}", target_container_id)),
        cgroupns_host: false,
        pid_host: false,
        // Override the default entrypoint so `command` is what runs.
        entrypoint: Some(String::new()),
        command: Some(build_tcpdump_cmd(iface, filter)),
        security_opt: Vec::new(),
        labels,
        auto_remove: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_default_iface() {
        let cmd = build_tcpdump_cmd("any", None);
        assert_eq!(
            cmd,
            vec!["tcpdump", "-i", "any", "-U", "-s", "0", "-w", "-"]
        );
    }

    #[test]
    fn cmd_with_filter() {
        let cmd = build_tcpdump_cmd("eth0", Some("port 80"));
        assert_eq!(cmd.last().unwrap(), "port 80");
        assert!(cmd.iter().any(|s| s == "eth0"));
    }

    #[test]
    fn cmd_strips_empty_filter() {
        let cmd = build_tcpdump_cmd("any", Some("   "));
        // No trailing filter when blank.
        assert_eq!(cmd.last().unwrap(), "-");
    }

    #[test]
    fn config_shares_target_netns() {
        let cfg = build_capture_config(
            "k3dev-capture-1",
            "abc123",
            "nicolaka/netshoot",
            "any",
            None,
            "pod default/nginx",
        );
        assert_eq!(cfg.network.as_deref(), Some("container:abc123"));
        assert!(cfg.privileged);
        assert!(cfg.auto_remove);
        assert_eq!(cfg.entrypoint.as_deref(), Some(""));
        assert_eq!(
            cfg.labels.get(CAPTURE_LABEL).map(|s| s.as_str()),
            Some("true")
        );
        assert_eq!(
            cfg.labels.get("k3dev.target").map(|s| s.as_str()),
            Some("pod default/nginx")
        );
        let cmd = cfg.command.as_ref().expect("command set");
        assert_eq!(cmd[0], "tcpdump");
    }

    #[test]
    fn config_passes_filter_through_to_tcpdump() {
        let cfg = build_capture_config("s", "id", "img", "any", Some("tcp port 443"), "desc");
        let cmd = cfg.command.as_ref().unwrap();
        assert_eq!(cmd.last().unwrap(), "tcp port 443");
    }
}
