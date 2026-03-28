//! k3dev-agent: lightweight stats collector running inside the k3s container.
//!
//! Reads cgroup v2 stats and queries Docker socket for container-to-pod mapping.
//! Outputs JSON to stdout. Zero external dependencies (std only).
//!
//! Output format:
//! {"ts":<usec>,"containers":[{"id":"...","pod":"...","ns":"...","cpu":<usec>,"cq":<quota>,"cp":<period>,"mem":<bytes>,"ml":<bytes>},...]}
//!
//! Fields: ts=timestamp_usec, cpu=usage_usec, cq=cpu_quota(-1=no limit),
//!         cp=cpu_period, mem=memory_current, ml=memory_max(-1=no limit)

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);

    let cgroups = collect_cgroups();
    let docker_names = query_docker_containers();

    let json = build_json(ts, &cgroups, &docker_names);
    let _ = std::io::stdout().write_all(json.as_bytes());
}

// --- Cgroup collection ---

struct CgroupStats {
    usage_usec: u64,
    cpu_quota: i64,  // -1 = no limit ("max")
    cpu_period: u64,
    mem_current: u64,
    mem_max: i64, // -1 = no limit ("max")
}

/// Detect cgroup version: v2 has cgroup.controllers at root
fn is_cgroup_v2() -> bool {
    Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
}

fn collect_cgroups() -> HashMap<String, CgroupStats> {
    let mut stats = HashMap::new();

    if is_cgroup_v2() {
        let base = Path::new("/sys/fs/cgroup/kubepods");
        if base.exists() {
            walk_cgroup_dir_v2(base, &mut stats);
        }
    } else {
        // cgroup v1: cpu stats under cpu,cpuacct (or cpu), memory under memory
        let cpu_base = if Path::new("/sys/fs/cgroup/cpu,cpuacct/kubepods").exists() {
            Some(Path::new("/sys/fs/cgroup/cpu,cpuacct/kubepods"))
        } else if Path::new("/sys/fs/cgroup/cpu/kubepods").exists() {
            Some(Path::new("/sys/fs/cgroup/cpu/kubepods"))
        } else {
            None
        };
        if let Some(base) = cpu_base {
            walk_cgroup_dir_v1(base, &mut stats);
        }
    }

    stats
}

fn walk_cgroup_dir_v2(dir: &Path, stats: &mut HashMap<String, CgroupStats>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // QoS class directories and pod directories — recurse
        if name == "burstable"
            || name == "besteffort"
            || name == "guaranteed"
            || name.starts_with("pod")
        {
            walk_cgroup_dir_v2(&path, stats);
        } else if path.join("cpu.stat").exists() {
            // Container cgroup directory — read stats
            if let Some(cg) = read_cgroup_v2_files(&path) {
                stats.insert(name.to_string(), cg);
            }
        }
    }
}

fn walk_cgroup_dir_v1(dir: &Path, stats: &mut HashMap<String, CgroupStats>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if name == "burstable"
            || name == "besteffort"
            || name == "guaranteed"
            || name.starts_with("pod")
            || name.starts_with("kubepods")
        {
            walk_cgroup_dir_v1(&path, stats);
        } else if path.join("cpuacct.usage").exists() {
            if let Some(cg) = read_cgroup_v1_files(&path) {
                stats.insert(name.to_string(), cg);
            }
        }
    }
}

fn read_cgroup_v2_files(path: &Path) -> Option<CgroupStats> {
    // cpu.stat → usage_usec
    let cpu_stat = fs::read_to_string(path.join("cpu.stat")).ok()?;
    let usage_usec = cpu_stat
        .lines()
        .find(|l| l.starts_with("usage_usec"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    // cpu.max → "quota period" or "max period"
    let cpu_max = fs::read_to_string(path.join("cpu.max")).unwrap_or_default();
    let parts: Vec<&str> = cpu_max.split_whitespace().collect();
    let (cpu_quota, cpu_period) = if parts.len() >= 2 {
        let period = parts[1].parse::<u64>().unwrap_or(100000);
        if parts[0] == "max" {
            (-1i64, period)
        } else {
            let q = parts[0].parse::<i64>().unwrap_or(-1);
            (q, period)
        }
    } else {
        (-1, 100000)
    };

    // memory.current
    let mem_current = fs::read_to_string(path.join("memory.current"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    // memory.max → number or "max"
    let mem_max_str = fs::read_to_string(path.join("memory.max")).unwrap_or_default();
    let mem_max = match mem_max_str.trim() {
        "max" => -1i64,
        s => s.parse::<i64>().unwrap_or(-1),
    };

    Some(CgroupStats {
        usage_usec,
        cpu_quota,
        cpu_period,
        mem_current,
        mem_max,
    })
}

fn read_cgroup_v1_files(path: &Path) -> Option<CgroupStats> {
    // cpuacct.usage is in nanoseconds → convert to microseconds
    let usage_usec = fs::read_to_string(path.join("cpuacct.usage"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|ns| ns / 1000)
        .unwrap_or(0);

    // cpu.cfs_quota_us (-1 = no limit), cpu.cfs_period_us
    let cpu_quota = fs::read_to_string(path.join("cpu.cfs_quota_us"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(-1);
    let cpu_period = fs::read_to_string(path.join("cpu.cfs_period_us"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(100000);

    // Memory: derive memory path from cpu path
    // /sys/fs/cgroup/cpu,cpuacct/kubepods/... → /sys/fs/cgroup/memory/kubepods/...
    let path_str = path.to_str().unwrap_or("");
    let mem_path = if path_str.contains("/cpu,cpuacct/") {
        Some(path_str.replacen("/cpu,cpuacct/", "/memory/", 1))
    } else if path_str.contains("/cpu/") {
        Some(path_str.replacen("/cpu/", "/memory/", 1))
    } else {
        None
    };

    let (mem_current, mem_max) = if let Some(ref mp) = mem_path {
        let mp = Path::new(mp);
        let current = fs::read_to_string(mp.join("memory.usage_in_bytes"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let max = fs::read_to_string(mp.join("memory.limit_in_bytes"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|v| {
                // v1 uses a very large number for "no limit" (typically >= 2^60)
                if v > (1u64 << 60) {
                    -1i64
                } else {
                    v as i64
                }
            })
            .unwrap_or(-1);
        (current, max)
    } else {
        (0, -1)
    };

    Some(CgroupStats {
        usage_usec,
        cpu_quota,
        cpu_period,
        mem_current,
        mem_max,
    })
}

// --- Docker socket query ---

/// Query Docker daemon for running containers and extract container_id → (pod_name, namespace).
/// Uses raw HTTP over Unix socket — no external dependencies.
/// Tries the default socket first, falls back to the VM's raw socket on Docker Desktop
/// where the proxy socket filters container visibility.
fn query_docker_containers() -> HashMap<String, (String, String)> {
    // Try default socket first
    let result = query_docker_socket("/var/run/docker.sock");
    if !result.is_empty() {
        return result;
    }

    // Fallback: Docker Desktop's raw VM socket (bypasses proxy filtering)
    // Accessible because k3s container runs with --pid=host
    let raw_socket = Path::new("/proc/1/root/run/docker.sock");
    if raw_socket.exists() {
        return query_docker_socket(raw_socket.to_str().unwrap_or_default());
    }

    HashMap::new()
}

fn query_docker_socket(socket_path: &str) -> HashMap<String, (String, String)> {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));

    let request = b"GET /containers/json HTTP/1.0\r\nHost: localhost\r\n\r\n";
    if stream.write_all(request).is_err() {
        return HashMap::new();
    }

    let mut response = Vec::new();
    if stream.read_to_end(&mut response).is_err() {
        return HashMap::new();
    }

    let response_str = String::from_utf8_lossy(&response);

    let body = match response_str.find("\r\n\r\n") {
        Some(pos) => &response_str[pos + 4..],
        None => return HashMap::new(),
    };

    parse_container_list(body)
}

/// Extract container Id and Names from Docker API JSON response.
/// Minimal parser — no serde, handles the specific Docker list format.
fn parse_container_list(body: &str) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    let mut pos = 0;

    while pos < body.len() {
        // Find next "Id":"
        let id_marker = match body[pos..].find("\"Id\":\"") {
            Some(p) => pos + p,
            None => break,
        };
        let id_start = id_marker + 6;
        let id_end = match body[id_start..].find('"') {
            Some(p) => id_start + p,
            None => break,
        };
        let container_id = &body[id_start..id_end];

        // Find "Names":[" after this Id (within the same object)
        let search_end = body[id_end..].find("\"Id\":\"").map_or(body.len(), |p| id_end + p);
        let search_region = &body[id_end..search_end];

        if let Some(names_offset) = search_region.find("\"Names\":[\"") {
            let name_start = id_end + names_offset + 10;
            if let Some(name_end_offset) = body[name_start..].find('"') {
                let name_end = name_start + name_end_offset;
                let container_name = body[name_start..name_end].trim_start_matches('/');

                // Parse k8s container name: k8s_{container}_{pod}_{ns}_{uid}_{attempt}
                // Skip pause containers (k8s_POD_)
                if container_name.starts_with("k8s_") && !container_name.starts_with("k8s_POD_") {
                    let parts: Vec<&str> = container_name.splitn(5, '_').collect();
                    if parts.len() >= 4 {
                        map.insert(
                            container_id.to_string(),
                            (parts[2].to_string(), parts[3].to_string()),
                        );
                    }
                }
            }
        }

        pos = id_end + 1;
    }

    map
}

// --- JSON output ---

fn build_json(
    ts: u64,
    cgroups: &HashMap<String, CgroupStats>,
    docker_names: &HashMap<String, (String, String)>,
) -> String {
    // Pre-allocate reasonable capacity
    let mut out = String::with_capacity(256 + cgroups.len() * 180);
    out.push_str("{\"ts\":");
    push_u64(&mut out, ts);
    out.push_str(",\"containers\":[");

    let mut first = true;
    for (cid, stats) in cgroups {
        let (pod, ns) = docker_names
            .get(cid.as_str())
            .map(|(p, n)| (p.as_str(), n.as_str()))
            .unwrap_or(("_unknown", "_unknown"));

        if !first {
            out.push(',');
        }
        first = false;

        out.push_str("{\"id\":\"");
        out.push_str(cid);
        out.push_str("\",\"pod\":\"");
        push_json_escaped(&mut out, pod);
        out.push_str("\",\"ns\":\"");
        push_json_escaped(&mut out, ns);
        out.push_str("\",\"cpu\":");
        push_u64(&mut out, stats.usage_usec);
        out.push_str(",\"cq\":");
        push_i64(&mut out, stats.cpu_quota);
        out.push_str(",\"cp\":");
        push_u64(&mut out, stats.cpu_period);
        out.push_str(",\"mem\":");
        push_u64(&mut out, stats.mem_current);
        out.push_str(",\"ml\":");
        push_i64(&mut out, stats.mem_max);
        out.push('}');
    }

    out.push_str("]}");
    out
}

fn push_u64(s: &mut String, v: u64) {
    use std::fmt::Write;
    let _ = write!(s, "{}", v);
}

fn push_i64(s: &mut String, v: i64) {
    use std::fmt::Write;
    let _ = write!(s, "{}", v);
}

/// Escape minimal JSON special characters in pod/namespace names.
fn push_json_escaped(s: &mut String, v: &str) {
    for ch in v.chars() {
        match ch {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            _ => s.push(ch),
        }
    }
}
