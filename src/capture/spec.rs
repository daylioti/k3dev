//! Capture specification types
//!
//! Defines what to capture (target), where to write the pcap file, and
//! optional limits/filters. Pure data + path/parser helpers; no I/O.

use std::path::PathBuf;
use std::time::Duration;

/// What to capture traffic from.
#[derive(Debug, Clone)]
pub enum CaptureTarget {
    /// A k8s pod (resolved to its pause container).
    Pod { pod: String, namespace: String },
    /// A specific Docker container by name.
    Container(String),
}

impl CaptureTarget {
    /// Short, filename-safe label describing the target.
    pub fn label(&self) -> String {
        match self {
            CaptureTarget::Pod { pod, namespace } => {
                sanitize(&format!("pod-{}-{}", namespace, pod))
            }
            CaptureTarget::Container(name) => sanitize(&format!("container-{}", name)),
        }
    }
}

/// Specification for a capture run.
#[derive(Debug, Clone)]
pub struct CaptureSpec {
    pub target: CaptureTarget,
    /// Absolute path to the .pcap file to write.
    pub output_path: PathBuf,
    /// Sidecar image (e.g. "nicolaka/netshoot").
    pub image: String,
    /// Interface to capture on (e.g. "any", "eth0").
    pub iface: String,
    /// Optional BPF filter expression (e.g. "port 80").
    pub filter: Option<String>,
    /// Stop after this much wall time has elapsed.
    pub duration: Option<Duration>,
    /// Stop once this many bytes have been written to the pcap file.
    pub max_bytes: Option<u64>,
}

/// Build a default output path under `output_dir` with a timestamped name.
///
/// Format: `<target-label>-YYYYMMDD-HHMMSS.pcap`
pub fn default_output_path(output_dir: &std::path::Path, target: &CaptureTarget) -> PathBuf {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    output_dir.join(format!("{}-{}.pcap", target.label(), ts))
}

/// Replace any character that isn't alphanumeric, '-' or '_' with '-'.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Parse a duration string like "30s", "5m", "1h", "250ms".
/// Returns `None` on parse failure (caller decides how to surface).
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, unit) = match s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|i| s.split_at(i))
    {
        Some((n, u)) => (n, u),
        None => (s, "s"),
    };
    let n: f64 = num_str.parse().ok()?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" | "" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return None,
    };
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(Duration::from_millis((secs * 1000.0) as u64))
}

/// Parse a size string like "100K", "10M", "1G", "500" (bytes).
pub fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, unit) = match s.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => s.split_at(i),
        None => (s, ""),
    };
    let n: f64 = num_str.parse().ok()?;
    let mult: f64 = match unit.trim().to_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let total = n * mult;
    if !total.is_finite() || total < 0.0 {
        return None;
    }
    Some(total as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_pod_sanitizes() {
        let t = CaptureTarget::Pod {
            pod: "my-app/v1".to_string(),
            namespace: "default".to_string(),
        };
        let label = t.label();
        assert!(label.starts_with("pod-default-my-app-v1"));
        assert!(!label.contains('/'));
    }

    #[test]
    fn label_container_sanitizes() {
        let t = CaptureTarget::Container("k8s_POD_x_y_uid_0".to_string());
        let label = t.label();
        assert!(label.starts_with("container-"));
        assert!(label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn default_output_path_format() {
        let p = default_output_path(
            std::path::Path::new("/tmp/cap"),
            &CaptureTarget::Container("foo".to_string()),
        );
        let s = p.to_string_lossy();
        assert!(s.starts_with("/tmp/cap/container-foo-"));
        assert!(s.ends_with(".pcap"));
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("30"), Some(Duration::from_secs(30)));
        assert!(parse_duration("abc").is_none());
        assert!(parse_duration("").is_none());
    }

    #[test]
    fn parse_bytes_units() {
        assert_eq!(parse_bytes("100"), Some(100));
        assert_eq!(parse_bytes("100B"), Some(100));
        assert_eq!(parse_bytes("1K"), Some(1024));
        assert_eq!(parse_bytes("2M"), Some(2 * 1024 * 1024));
        assert_eq!(parse_bytes("1G"), Some(1024 * 1024 * 1024));
        assert!(parse_bytes("xyz").is_none());
    }
}
