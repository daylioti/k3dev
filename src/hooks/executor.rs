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
    /// Standalone kubeconfig pinned to this cluster, if one has been written
    kubeconfig: Option<PathBuf>,
    /// Kubeconfig context name for this cluster
    context: String,
}

impl HookExecutor {
    pub fn new(config: HooksConfig, kubeconfig: Option<PathBuf>, context: String) -> Self {
        Self {
            config,
            kubeconfig,
            context,
        }
    }

    /// Environment for a hook process.
    ///
    /// `KUBECONFIG` and `K3DEV_CONTEXT` are injected so a hook reaches the
    /// cluster it was fired for rather than whatever the user's current-context
    /// points at — which may well be a context left over from a deleted
    /// cluster. Configured env wins, so a hook can still opt out.
    fn hook_env(&self, hook: &HookCommand) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = HashMap::new();

        env.insert("K3DEV_CONTEXT".to_string(), self.context.clone());
        if let Some(path) = &self.kubeconfig {
            env.insert("KUBECONFIG".to_string(), path.to_string_lossy().to_string());
        }

        // Global env, then hook-specific env (most specific wins)
        for (key, value) in self.config.env.iter().chain(hook.env.iter()) {
            env.insert(key.clone(), expand_home(value));
        }

        env
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

        let env = self.hook_env(hook);

        // Build the command (platform-aware shell)
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(&hook.command);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&hook.command);
            c
        };

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

        // Under the TUI, give the hook a pty so tty prompts reach the popup
        let pty = crate::tty::attach_pty(&mut cmd)?;

        // Spawn the process
        let mut child = cmd.spawn()?;

        let pty_io = match pty {
            Some(pty) => Some(pty.start_io().await),
            None => None,
        };

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
        if let Some(pty_io) = pty_io {
            pty_io.finish().await;
        }

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

#[cfg(test)]
mod env_tests {
    use super::*;

    fn executor(kubeconfig: Option<PathBuf>) -> HookExecutor {
        HookExecutor::new(HooksConfig::default(), kubeconfig, "k3dev".to_string())
    }

    fn hook() -> HookCommand {
        HookCommand {
            name: "t".to_string(),
            command: "true".to_string(),
            workdir: None,
            env: HashMap::new(),
            continue_on_error: false,
            timeout: 30,
        }
    }

    #[test]
    fn injects_cluster_context_and_pinned_kubeconfig() {
        let env = executor(Some(PathBuf::from("/state/k3dev.yaml"))).hook_env(&hook());

        assert_eq!(env.get("K3DEV_CONTEXT").map(String::as_str), Some("k3dev"));
        assert_eq!(
            env.get("KUBECONFIG").map(String::as_str),
            Some("/state/k3dev.yaml")
        );
    }

    #[test]
    fn without_a_pinned_kubeconfig_only_the_context_is_injected() {
        let env = executor(None).hook_env(&hook());

        assert_eq!(env.get("K3DEV_CONTEXT").map(String::as_str), Some("k3dev"));
        assert!(!env.contains_key("KUBECONFIG"));
    }

    #[test]
    fn user_env_wins_over_injected_defaults() {
        let mut h = hook();
        h.env
            .insert("KUBECONFIG".to_string(), "/custom/config".to_string());

        let env = executor(Some(PathBuf::from("/state/k3dev.yaml"))).hook_env(&h);

        assert_eq!(
            env.get("KUBECONFIG").map(String::as_str),
            Some("/custom/config")
        );
    }
}

#[cfg(all(test, unix))]
mod terminal_tests {
    use super::*;
    use crate::app::AppMessage;

    #[tokio::test]
    async fn tty_prompt_is_routed_through_the_attached_terminal() {
        let _guard = crate::tty::test_guard().await;
        let (app_tx, mut app_rx) = mpsc::channel::<AppMessage>(32);
        crate::tty::attach(app_tx);

        let hook = HookCommand {
            name: "mfa".to_string(),
            command:
                "printf 'Enter code: ' > /dev/tty; read -r code < /dev/tty; echo \"got $code\""
                    .to_string(),
            workdir: None,
            env: HashMap::new(),
            continue_on_error: false,
            timeout: 10,
        };
        let executor = HookExecutor::new(HooksConfig::default(), None, "k3dev".to_string());
        let (out_tx, mut out_rx) = mpsc::channel::<OutputLine>(32);
        let run = tokio::spawn(async move { executor.execute_hook(&hook, out_tx).await });

        let input = match app_rx.recv().await {
            Some(AppMessage::ChildTtyOpened { input }) => input,
            _ => panic!("expected the child terminal to open first"),
        };
        match app_rx.recv().await {
            Some(AppMessage::ChildTtyOutput(text)) => assert_eq!(text, "Enter code: "),
            _ => panic!("expected the tty prompt"),
        }
        input.send(b"123456\r".to_vec()).await.unwrap();

        run.await.unwrap().unwrap();
        let mut lines = Vec::new();
        while let Ok(line) = out_rx.try_recv() {
            lines.push(line.content);
        }
        assert!(
            lines.iter().any(|l| l.trim() == "got 123456"),
            "hook never received the typed input: {lines:?}"
        );
        // The tty echoes the typed code before the terminal closes
        loop {
            match app_rx.recv().await {
                Some(AppMessage::ChildTtyClosed) => break,
                Some(AppMessage::ChildTtyOutput(_)) => continue,
                _ => panic!("expected the child terminal to close"),
            }
        }

        crate::tty::detach();
    }
}
