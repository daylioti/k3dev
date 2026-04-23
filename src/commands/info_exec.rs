//! Buffering command executor for info blocks.
//!
//! The regular command executor streams output line-by-line to the output
//! popup. Info blocks need the whole output captured into a `String` so it
//! can be trimmed and rendered in the sidebar.

use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::time::Duration;

use crate::cluster::DockerManager;
use crate::config::{ExecConfig, ExecutionTarget};
use crate::k8s::{K8sClient, PodExecutor};

/// Execute an `ExecConfig` and return combined stdout/stderr as a `String`.
///
/// Unlike the streaming executor used by custom commands, this buffers the
/// entire output. Intended for short-running probes driven by info blocks.
pub async fn capture_exec(
    exec: &ExecConfig,
    k8s: Option<&K8sClient>,
    docker: Option<&DockerManager>,
    timeout: Duration,
) -> Result<String> {
    let fut = run(exec, k8s, docker);
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow!("timed out after {:?}", timeout))?
}

async fn run(
    exec: &ExecConfig,
    k8s: Option<&K8sClient>,
    docker: Option<&DockerManager>,
) -> Result<String> {
    match &exec.target {
        ExecutionTarget::Host => run_host(&exec.workdir, &exec.cmd).await,
        ExecutionTarget::Docker { container } => {
            let docker = docker.ok_or_else(|| anyhow!("docker client unavailable"))?;
            run_docker(docker, container, &exec.workdir, &exec.cmd).await
        }
        ExecutionTarget::Kubernetes {
            namespace,
            selector,
            pod_name,
            container,
        } => {
            let k8s = k8s.ok_or_else(|| anyhow!("kubernetes client unavailable"))?;
            run_kubernetes(
                k8s,
                namespace,
                selector,
                pod_name,
                container,
                &exec.workdir,
                &exec.cmd,
            )
            .await
        }
    }
}

async fn run_host(workdir: &str, cmd: &str) -> Result<String> {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(cmd);
    if !workdir.is_empty() {
        command.current_dir(workdir);
    }
    let out = command
        .output()
        .await
        .with_context(|| format!("failed to run host command: {}", cmd))?;
    let mut buf = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        if !buf.is_empty() && !buf.ends_with('\n') {
            buf.push('\n');
        }
        buf.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(buf)
}

async fn run_docker(
    docker: &DockerManager,
    container: &str,
    workdir: &str,
    cmd: &str,
) -> Result<String> {
    let shell_cmd = if workdir.is_empty() {
        cmd.to_string()
    } else {
        format!("cd {} && {}", workdir, cmd)
    };
    docker
        .exec_in_container(container, &["sh", "-c", &shell_cmd])
        .await
}

async fn run_kubernetes(
    k8s: &K8sClient,
    namespace: &str,
    selector: &str,
    pod_name: &str,
    container: &str,
    workdir: &str,
    cmd: &str,
) -> Result<String> {
    let executor = PodExecutor::new(k8s);
    let pod = executor
        .find_pod(
            namespace,
            if selector.is_empty() {
                None
            } else {
                Some(selector)
            },
            if pod_name.is_empty() {
                None
            } else {
                Some(pod_name)
            },
        )
        .await?;

    let shell_cmd = if workdir.is_empty() {
        cmd.to_string()
    } else {
        format!("cd {} && {}", workdir, cmd)
    };
    let container_opt = if container.is_empty() {
        None
    } else {
        Some(container)
    };
    let result = executor
        .exec_simple(&pod.namespace, &pod.name, container_opt, &shell_cmd)
        .await?;
    let mut out = result.stdout;
    if !result.stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&result.stderr);
    }
    Ok(out)
}

static ANSI_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]").expect("valid regex"));

/// Strip ANSI color / cursor escape sequences.
pub fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
}

/// Trim an output string: keep the last `max_lines` lines (if set), then cap
/// total length to `max_length` chars (UTF-8 safe).
pub fn trim_output(input: &str, max_lines: Option<usize>, max_length: Option<usize>) -> String {
    // First, tail by line count.
    let tailed: String = match max_lines {
        Some(n) if n > 0 => {
            let lines: Vec<&str> = input.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].join("\n")
        }
        _ => input.to_string(),
    };

    // Then, hard-cap chars at a UTF-8 boundary.
    match max_length {
        Some(n) if tailed.chars().count() > n => {
            let mut out = String::with_capacity(n);
            for (i, ch) in tailed.chars().enumerate() {
                if i >= n {
                    break;
                }
                out.push(ch);
            }
            out
        }
        _ => tailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_color_codes() {
        let input = "\x1b[31mred\x1b[0m text";
        assert_eq!(strip_ansi(input), "red text");
    }

    #[test]
    fn strip_ansi_leaves_plain_text_alone() {
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn trim_output_tails_lines() {
        let input = "a\nb\nc\nd\ne";
        assert_eq!(trim_output(input, Some(2), None), "d\ne");
        assert_eq!(trim_output(input, Some(10), None), "a\nb\nc\nd\ne");
    }

    #[test]
    fn trim_output_caps_length() {
        assert_eq!(trim_output("abcdefgh", None, Some(3)), "abc");
    }

    #[test]
    fn trim_output_applies_both() {
        let input = "aaa\nbbb\nccc";
        assert_eq!(trim_output(input, Some(2), Some(4)), "bbb\n");
    }

    #[test]
    fn trim_output_utf8_boundary() {
        // "é" is 2 bytes in UTF-8 but 1 char
        let input = "éééé";
        let out = trim_output(input, None, Some(2));
        assert_eq!(out.chars().count(), 2);
    }

    #[test]
    fn trim_output_no_limits_returns_input() {
        assert_eq!(trim_output("hello", None, None), "hello");
    }
}
