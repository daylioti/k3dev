//! Shell session management for interactive pod exec

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, AttachParams};
use kube::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::AppMessage;

/// Handle to an active shell session
pub struct ShellSessionHandle {
    stdin_tx: mpsc::Sender<Vec<u8>>,
    cancel_token: CancellationToken,
    pod_name: String,
    namespace: String,
}

impl ShellSessionHandle {
    /// Send raw bytes to shell stdin
    pub fn write(&self, data: &[u8]) {
        let _ = self.stdin_tx.try_send(data.to_vec());
    }

    /// Send terminal resize via stty
    pub fn resize(&self, rows: u16, cols: u16) {
        let cmd = format!("stty rows {} cols {}\n", rows, cols);
        let _ = self.stdin_tx.try_send(cmd.into_bytes());
    }

    /// Close the shell session
    pub fn close(&self) {
        self.cancel_token.cancel();
    }

    pub fn pod_name(&self) -> &str {
        &self.pod_name
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

/// Start an interactive shell session on a pod.
/// Sends ShellSessionReady on success, ShellSessionEnded on completion/error.
pub async fn start_shell_session(
    client: Client,
    pod_name: String,
    namespace: String,
    container: Option<String>,
    message_tx: mpsc::Sender<AppMessage>,
) {
    let pods: Api<Pod> = Api::namespaced(client, &namespace);

    let attach_params = AttachParams {
        container,
        stdin: true,
        stdout: true,
        stderr: false,
        tty: true,
        ..Default::default()
    };

    let mut attached = match pods.exec(&pod_name, vec!["sh"], &attach_params).await {
        Ok(a) => a,
        Err(e) => {
            let _ = message_tx
                .send(AppMessage::ShellSessionEnded(Some(format!(
                    "Failed to exec: {}",
                    e
                ))))
                .await;
            return;
        }
    };

    let stdin_writer = match attached.stdin() {
        Some(w) => w,
        None => {
            let _ = message_tx
                .send(AppMessage::ShellSessionEnded(Some(
                    "No stdin available".into(),
                )))
                .await;
            return;
        }
    };

    let mut stdout_reader = match attached.stdout() {
        Some(r) => r,
        None => {
            let _ = message_tx
                .send(AppMessage::ShellSessionEnded(Some(
                    "No stdout available".into(),
                )))
                .await;
            return;
        }
    };

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(256);
    let cancel_token = CancellationToken::new();

    let handle = ShellSessionHandle {
        stdin_tx,
        cancel_token: cancel_token.clone(),
        pod_name: pod_name.clone(),
        namespace: namespace.clone(),
    };

    // Send handle back to app
    if message_tx
        .send(AppMessage::ShellSessionReady(handle))
        .await
        .is_err()
    {
        return;
    }

    // Spawn stdin writer task
    let cancel_stdin = cancel_token.clone();
    let stdin_task = tokio::spawn(async move {
        let mut stdin_writer = stdin_writer;
        loop {
            tokio::select! {
                data = stdin_rx.recv() => {
                    match data {
                        Some(bytes) => {
                            if stdin_writer.write_all(&bytes).await.is_err() {
                                break;
                            }
                            if stdin_writer.flush().await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = cancel_stdin.cancelled() => break,
            }
        }
    });

    // Read stdout and forward to app
    let msg_tx = message_tx.clone();
    let cancel_stdout = cancel_token.clone();
    let mut buf = [0u8; 4096];

    let error = loop {
        tokio::select! {
            result = stdout_reader.read(&mut buf) => {
                match result {
                    Ok(0) => break None,
                    Ok(n) => {
                        if msg_tx
                            .send(AppMessage::ShellOutput(buf[..n].to_vec()))
                            .await
                            .is_err()
                        {
                            break None;
                        }
                    }
                    Err(e) => break Some(e.to_string()),
                }
            }
            _ = cancel_stdout.cancelled() => break None,
        }
    };

    // Clean up
    stdin_task.abort();
    // Keep _attached alive until here so the WebSocket connection stays open
    drop(attached);

    let _ = message_tx.send(AppMessage::ShellSessionEnded(error)).await;
}
