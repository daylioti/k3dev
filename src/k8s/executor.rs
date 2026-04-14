use anyhow::{anyhow, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, AttachParams},
    Client,
};
use tokio::io::AsyncReadExt;

use super::client::{K8sClient, PodInfo};

/// Result of pod command execution
#[derive(Debug)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Pod command executor
pub struct PodExecutor {
    client: Client,
}

impl PodExecutor {
    pub fn new(k8s_client: &K8sClient) -> Self {
        Self {
            client: k8s_client.client().clone(),
        }
    }

    /// Find a pod by name or selector
    pub async fn find_pod(
        &self,
        namespace: &str,
        selector: Option<&str>,
        pod_name: Option<&str>,
    ) -> Result<PodInfo> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);

        if let Some(name) = pod_name.filter(|s| !s.is_empty()) {
            let pod = pods.get(name).await?;
            return Ok(pod_to_info(&pod));
        }

        let selector = selector
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Either selector or pod_name must be specified"))?;

        // Find by selector
        let params = kube::api::ListParams::default().labels(selector);
        let list = pods.list(&params).await?;

        // Find first running pod
        for pod in &list.items {
            let status = pod
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("Unknown");
            if status == "Running" {
                return Ok(pod_to_info(pod));
            }
        }

        // No running pod found, return first pod if any
        list.items
            .first()
            .map(pod_to_info)
            .ok_or_else(|| anyhow!("No pod found matching selector: {}", selector))
    }

    /// Execute a command in a pod
    pub async fn exec(
        &self,
        namespace: &str,
        pod_name: &str,
        container: Option<&str>,
        command: Vec<String>,
    ) -> Result<ExecResult> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);

        let attach_params = AttachParams {
            container: container.map(String::from),
            stdin: false,
            stdout: true,
            stderr: true,
            tty: false,
            ..Default::default()
        };

        let mut attached = pods.exec(pod_name, command, &attach_params).await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        if let Some(mut stdout_stream) = attached.stdout() {
            let mut buf = Vec::new();
            stdout_stream.read_to_end(&mut buf).await?;
            stdout = String::from_utf8_lossy(&buf).to_string();
        }

        if let Some(mut stderr_stream) = attached.stderr() {
            let mut buf = Vec::new();
            stderr_stream.read_to_end(&mut buf).await?;
            stderr = String::from_utf8_lossy(&buf).to_string();
        }

        let status = attached.take_status();
        let exit_code = if let Some(status_future) = status {
            match status_future.await {
                Some(status) => {
                    if status.status.as_deref() == Some("Success") {
                        0
                    } else {
                        1
                    }
                }
                None => 1,
            }
        } else {
            0
        };

        Ok(ExecResult {
            stdout,
            stderr,
            exit_code,
        })
    }

    /// Execute a simple command string
    pub async fn exec_simple(
        &self,
        namespace: &str,
        pod_name: &str,
        container: Option<&str>,
        command: &str,
    ) -> Result<ExecResult> {
        let cmd_parts = vec!["sh".to_string(), "-c".to_string(), command.to_string()];
        self.exec(namespace, pod_name, container, cmd_parts).await
    }
}

fn pod_to_info(pod: &Pod) -> PodInfo {
    PodInfo {
        name: pod.metadata.name.clone().unwrap_or_default(),
        namespace: pod.metadata.namespace.clone().unwrap_or_default(),
        status: pod
            .status
            .as_ref()
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string()),
        ready: pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|c| c.type_ == "Ready"))
            .map(|c| c.status == "True")
            .unwrap_or(false),
        ip: pod.status.as_ref().and_then(|s| s.pod_ip.clone()),
    }
}
