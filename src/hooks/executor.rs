use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

use crate::config::{HookCommand, HookEvent, HooksConfig};
use crate::ui::components::OutputLine;

/// Executor for running hook commands
pub struct HookExecutor {
    config: HooksConfig,
}

impl HookExecutor {
    pub fn new(config: HooksConfig) -> Self {
        Self { config }
    }

    /// Execute all hooks for a given event
    pub async fn execute_hooks(
        &self,
        event: HookEvent,
        output_tx: mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        let hooks = self.config.get_hooks(event);

        if hooks.is_empty() {
            return Ok(());
        }

        let _ = output_tx
            .send(OutputLine::info(format!(
                "Running {} hooks ({} total)...",
                event.as_str(),
                hooks.len()
            )))
            .await;

        for (index, hook) in hooks.iter().enumerate() {
            let _ = output_tx
                .send(OutputLine::info(format!(
                    "[{}/{}] {}",
                    index + 1,
                    hooks.len(),
                    hook.name
                )))
                .await;

            match self.execute_hook(hook, output_tx.clone()).await {
                Ok(_) => {
                    let _ = output_tx
                        .send(OutputLine::success(format!("  {} completed", hook.name)))
                        .await;
                }
                Err(e) => {
                    let _ = output_tx
                        .send(OutputLine::error(format!("  {} failed: {}", hook.name, e)))
                        .await;

                    if !hook.continue_on_error {
                        return Err(anyhow!("Hook '{}' failed: {}", hook.name, e));
                    }
                }
            }
        }

        let _ = output_tx
            .send(OutputLine::success(format!(
                "{} hooks completed",
                event.as_str()
            )))
            .await;

        Ok(())
    }

    /// Execute a single hook command
    async fn execute_hook(
        &self,
        hook: &HookCommand,
        output_tx: mpsc::Sender<OutputLine>,
    ) -> Result<()> {
        // Expand workdir if specified
        let workdir = if let Some(ref wd) = hook.workdir {
            let expanded = expand_home(wd);
            let path = PathBuf::from(&expanded);
            if !path.exists() {
                return Err(anyhow!("Working directory does not exist: {}", expanded));
            }
            Some(expanded)
        } else {
            None
        };

        // Merge global env with hook-specific env (hook-specific takes precedence)
        let mut env: HashMap<String, String> = self.config.env.clone();
        for (key, value) in &hook.env {
            // Expand ~ in environment variable values
            env.insert(key.clone(), expand_home(value));
        }

        // Expand ~ in global env values too
        for value in env.values_mut() {
            *value = expand_home(value);
        }

        // Build the command
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&hook.command);

        if let Some(ref wd) = workdir {
            cmd.current_dir(wd);
        }

        // Set environment variables
        for (key, value) in &env {
            cmd.env(key, value);
        }

        // Configure stdio for streaming output
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Spawn the process
        let mut child = cmd.spawn()?;

        // Get stdout and stderr handles
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Spawn tasks to read stdout and stderr
        let stdout_tx = output_tx.clone();
        let stdout_handle = tokio::spawn(async move {
            if let Some(stdout) = stdout {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = stdout_tx
                        .send(OutputLine::info(format!("  {}", line)))
                        .await;
                }
            }
        });

        let stderr_tx = output_tx.clone();
        let stderr_handle = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = stderr_tx
                        .send(OutputLine::warning(format!("  {}", line)))
                        .await;
                }
            }
        });

        // Wait for the process with timeout
        let timeout_duration = Duration::from_secs(hook.timeout);
        let result = timeout(timeout_duration, child.wait()).await;

        // Wait for output tasks to complete
        let _ = stdout_handle.await;
        let _ = stderr_handle.await;

        match result {
            Ok(Ok(status)) => {
                if status.success() {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "Command exited with code {}",
                        status.code().unwrap_or(-1)
                    ))
                }
            }
            Ok(Err(e)) => Err(anyhow!("Failed to execute command: {}", e)),
            Err(_) => {
                // Timeout occurred - try to kill the process
                let _ = child.kill().await;
                Err(anyhow!("Command timed out after {} seconds", hook.timeout))
            }
        }
    }
}

/// Expand ~ to home directory in a path string
fn expand_home(path: &str) -> String {
    if path.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            return path.replacen('~', &home.to_string_lossy(), 1);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_home() {
        let home = dirs::home_dir().unwrap();
        let home_str = home.to_string_lossy();

        assert_eq!(expand_home("~"), home_str.to_string());
        assert_eq!(expand_home("~/foo/bar"), format!("{}/foo/bar", home_str));
        assert_eq!(expand_home("/absolute/path"), "/absolute/path");
        assert_eq!(expand_home("relative/path"), "relative/path");
    }
}
