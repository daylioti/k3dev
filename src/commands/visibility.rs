//! Visibility probe executor.
//!
//! Runs a `VisibleCheck` and returns a bool: visible or not. Used by commands
//! and info blocks to gate menu entries on runtime state (pod exists, container
//! running, host probe succeeds).

use anyhow::Result;
use std::time::Duration;

use crate::cluster::DockerManager;
use crate::config::{ExecConfig, ExecutionTarget, VisibleCheck};
use crate::k8s::{K8sClient, PodExecutor};

/// Evaluate a `VisibleCheck`. A timeout elapsing — or a missing k8s/docker
/// client when the check needs one — is treated as "not visible".
pub async fn check_visible(
    check: &VisibleCheck,
    k8s: Option<&K8sClient>,
    docker: Option<&DockerManager>,
    timeout: Duration,
) -> Result<bool> {
    let fut = run(check, k8s, docker);
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Ok(false),
    }
}

async fn run(
    check: &VisibleCheck,
    k8s: Option<&K8sClient>,
    docker: Option<&DockerManager>,
) -> Result<bool> {
    match check {
        VisibleCheck::Pod {
            namespace,
            selector,
        } => {
            let Some(k8s) = k8s else { return Ok(false) };
            let sel = if selector.is_empty() {
                None
            } else {
                Some(selector.as_str())
            };
            match k8s.list_pods(namespace, sel).await {
                Ok(list) => Ok(!list.is_empty()),
                Err(_) => Ok(false),
            }
        }
        VisibleCheck::Container { container } => {
            let Some(docker) = docker else {
                return Ok(false);
            };
            Ok(docker.container_exists(container).await)
        }
        VisibleCheck::Exec(cfg) => run_exec(cfg, k8s, docker).await,
    }
}

async fn run_exec(
    exec: &ExecConfig,
    k8s: Option<&K8sClient>,
    docker: Option<&DockerManager>,
) -> Result<bool> {
    match &exec.target {
        ExecutionTarget::Host => run_host(&exec.workdir, &exec.cmd).await,
        ExecutionTarget::Docker { container } => {
            let Some(docker) = docker else {
                return Ok(false);
            };
            let shell_cmd = build_shell_cmd(&exec.workdir, &exec.cmd);
            docker
                .exec_status(container, &["sh", "-c", &shell_cmd])
                .await
        }
        ExecutionTarget::Kubernetes {
            namespace,
            selector,
            pod_name,
            container,
        } => {
            let Some(k8s) = k8s else { return Ok(false) };
            let executor = PodExecutor::new(k8s);
            let pod = match executor
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
                .await
            {
                Ok(p) => p,
                Err(_) => return Ok(false),
            };

            let shell_cmd = build_shell_cmd(&exec.workdir, &exec.cmd);
            let container_opt = if container.is_empty() {
                None
            } else {
                Some(container.as_str())
            };
            executor
                .exec_status(&pod.namespace, &pod.name, container_opt, &shell_cmd)
                .await
        }
    }
}

async fn run_host(workdir: &str, cmd: &str) -> Result<bool> {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(cmd);
    if !workdir.is_empty() {
        command.current_dir(workdir);
    }
    // Discard stdout/stderr — we only care about the exit status.
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    let status = command.status().await?;
    Ok(status.success())
}

fn build_shell_cmd(workdir: &str, cmd: &str) -> String {
    if workdir.is_empty() {
        cmd.to_string()
    } else {
        format!("cd {} && {}", workdir, cmd)
    }
}
