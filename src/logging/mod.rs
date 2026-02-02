//! Logging module for k3dev
//!
//! Provides file-based logging with timestamps including milliseconds.
//! Logs are written to a configurable file path that supports {cluster_name} placeholder.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::Level;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::LoggingConfig;

/// Initialize logging based on configuration
///
/// # Arguments
/// * `config` - Logging configuration
/// * `cluster_name` - Cluster name to substitute in the log file path
pub fn init_logging(config: &LoggingConfig, cluster_name: &str) -> Result<()> {
    if !config.enabled {
        // Logging disabled, skip initialization
        return Ok(());
    }

    // Resolve log file path with cluster_name placeholder
    let log_file = config.file.replace("{cluster_name}", cluster_name);
    let log_path = PathBuf::from(&log_file);

    // Extract directory and filename
    let log_dir = log_path
        .parent()
        .context("Invalid log file path")?
        .to_path_buf();
    let log_filename = log_path
        .file_name()
        .context("Invalid log filename")?
        .to_str()
        .context("Invalid UTF-8 in log filename")?;

    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    let level = parse_log_level(&config.level)?;

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::NEVER)
        .filename_prefix(log_filename)
        .build(log_dir)
        .context("Failed to create log file appender")?;

    let file_layer = fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_timer(fmt::time::ChronoLocal::new(
            "%Y-%m-%d %H:%M:%S%.3f".to_string(),
        ))
        .with_target(false)
        .with_level(true)
        .with_thread_ids(false)
        .with_thread_names(false);

    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .try_init()
        .context("Failed to initialize tracing subscriber")?;

    tracing::info!(
        cluster_name = %cluster_name,
        log_file = %log_file,
        level = %level,
        "Logging initialized"
    );

    Ok(())
}

/// Parse log level string into tracing::Level
fn parse_log_level(level: &str) -> Result<Level> {
    match level.to_lowercase().as_str() {
        "trace" => Ok(Level::TRACE),
        "debug" => Ok(Level::DEBUG),
        "info" => Ok(Level::INFO),
        "warn" | "warning" => Ok(Level::WARN),
        "error" => Ok(Level::ERROR),
        _ => anyhow::bail!("Invalid log level: {}", level),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_log_level() {
        assert!(matches!(parse_log_level("trace"), Ok(Level::TRACE)));
        assert!(matches!(parse_log_level("debug"), Ok(Level::DEBUG)));
        assert!(matches!(parse_log_level("info"), Ok(Level::INFO)));
        assert!(matches!(parse_log_level("warn"), Ok(Level::WARN)));
        assert!(matches!(parse_log_level("error"), Ok(Level::ERROR)));
        assert!(parse_log_level("invalid").is_err());
    }
}
