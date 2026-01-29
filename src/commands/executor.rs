//! Command execution utilities
//!
//! This module provides helper types for executing async commands with
//! common patterns like output forwarding, timeouts, and cancellation.

use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::app::AppMessage;
use crate::ui::components::OutputLine;

/// Context for command execution
pub struct CommandContext {
    /// Sender for output lines
    pub output_tx: mpsc::Sender<OutputLine>,
    /// Sender for app messages
    pub message_tx: mpsc::Sender<AppMessage>,
    /// Timeout duration
    pub timeout: Duration,
}

impl CommandContext {
    /// Create a new command context
    pub fn new(
        message_tx: mpsc::Sender<AppMessage>,
        timeout: Duration,
    ) -> (Self, mpsc::Sender<OutputLine>) {
        let (output_tx, mut output_rx) = mpsc::channel::<OutputLine>(100);

        // Spawn output forwarder
        let msg_tx = message_tx.clone();
        tokio::spawn(async move {
            while let Some(line) = output_rx.recv().await {
                let _ = msg_tx.send(AppMessage::OutputLine(line)).await;
            }
        });

        (
            Self {
                output_tx: output_tx.clone(),
                message_tx,
                timeout,
            },
            output_tx,
        )
    }

    /// Execute an async operation with timeout and proper completion handling
    ///
    /// The operation receives an output sender and should return Ok(()) on success
    /// or Err(error_message) on failure.
    pub async fn execute<F, Fut>(self, operation: F)
    where
        F: FnOnce(mpsc::Sender<OutputLine>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send,
    {
        let output_tx = self.output_tx;
        let message_tx = self.message_tx;
        let timeout = self.timeout;

        let result = tokio::time::timeout(timeout, operation(output_tx)).await;

        match result {
            Ok(Ok(_)) => {
                let _ = message_tx.send(AppMessage::CommandComplete(0)).await;
            }
            Ok(Err(e)) => {
                let _ = message_tx.send(AppMessage::Error(e)).await;
                let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
            }
            Err(_) => {
                let _ = message_tx
                    .send(AppMessage::Error("Operation timed out".to_string()))
                    .await;
                let _ = message_tx.send(AppMessage::CommandComplete(1)).await;
            }
        }
    }
}

/// Helper to spawn a background refresh task that silently handles failures
#[allow(dead_code)]
pub fn spawn_refresh<F, Fut, T, M>(
    message_tx: mpsc::Sender<AppMessage>,
    timeout: Duration,
    operation: F,
    on_success: M,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, anyhow::Error>> + Send,
    T: Send + 'static,
    M: FnOnce(T) -> AppMessage + Send + 'static,
{
    tokio::spawn(async move {
        let result = tokio::time::timeout(timeout, operation()).await;

        if let Ok(Ok(value)) = result {
            let _ = message_tx.send(on_success(value)).await;
        }
        // Silently ignore errors and timeouts for refresh operations
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_command_context_creation() {
        let (tx, _rx) = mpsc::channel(10);
        let (ctx, _output_tx) = CommandContext::new(tx, Duration::from_secs(1));
        assert_eq!(ctx.timeout, Duration::from_secs(1));
    }
}
