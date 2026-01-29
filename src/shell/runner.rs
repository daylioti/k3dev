use anyhow::{anyhow, Result};
use chrono::Local;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ui::components::{OutputLine, OutputType};

/// Result of a command execution
#[allow(dead_code)]
#[derive(Debug)]
pub struct CommandResult {
    pub exit_code: i32,
    pub error: Option<String>,
}

/// Shell command runner with streaming output
#[allow(dead_code)]
pub struct ShellRunner {
    work_dir: Option<PathBuf>,
}

#[allow(dead_code)]
impl ShellRunner {
    pub fn new() -> Self {
        Self { work_dir: None }
    }

    pub fn with_work_dir(mut self, dir: PathBuf) -> Self {
        self.work_dir = Some(dir);
        self
    }

    /// Run a command and stream output to channel
    pub async fn run_streaming(
        &self,
        command: &str,
        args: &[&str],
        output_tx: mpsc::Sender<OutputLine>,
        cancel_token: CancellationToken,
    ) -> Result<CommandResult> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(dir) = &self.work_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture stderr"))?;

        let tx1 = output_tx.clone();
        let tx2 = output_tx;

        // Spawn stdout reader
        let stdout_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx1
                    .send(OutputLine {
                        content: line,
                        output_type: OutputType::Info,
                        timestamp: Local::now(),
                    })
                    .await;
            }
        });

        // Spawn stderr reader
        let stderr_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx2
                    .send(OutputLine {
                        content: line,
                        output_type: OutputType::Error,
                        timestamp: Local::now(),
                    })
                    .await;
            }
        });

        // Wait for completion or cancellation
        tokio::select! {
            status = child.wait() => {
                // Wait for output readers to finish
                let _ = stdout_handle.await;
                let _ = stderr_handle.await;

                match status {
                    Ok(s) => Ok(CommandResult {
                        exit_code: s.code().unwrap_or(-1),
                        error: None,
                    }),
                    Err(e) => Ok(CommandResult {
                        exit_code: -1,
                        error: Some(e.to_string()),
                    }),
                }
            }
            _ = cancel_token.cancelled() => {
                let _ = child.kill().await;
                Err(anyhow!("Command cancelled"))
            }
        }
    }

    /// Run a shell command (via /bin/sh -c)
    pub async fn run_shell(
        &self,
        command: &str,
        output_tx: mpsc::Sender<OutputLine>,
        cancel_token: CancellationToken,
    ) -> Result<CommandResult> {
        self.run_streaming("sh", &["-c", command], output_tx, cancel_token)
            .await
    }

    /// Run a script file with arguments
    pub async fn run_script(
        &self,
        script_path: &str,
        args: &[&str],
        output_tx: mpsc::Sender<OutputLine>,
        cancel_token: CancellationToken,
    ) -> Result<CommandResult> {
        let mut all_args = vec![script_path];
        all_args.extend(args);
        self.run_streaming("bash", &all_args, output_tx, cancel_token)
            .await
    }

    /// Check if a command exists in PATH
    pub fn command_exists(command: &str) -> bool {
        which::which(command).is_ok()
    }
}

impl Default for ShellRunner {
    fn default() -> Self {
        Self::new()
    }
}
