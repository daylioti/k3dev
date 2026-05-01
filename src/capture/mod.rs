//! Pod / container traffic capture via a tcpdump sidecar container.
//!
//! Spawns a sidecar Docker container that joins the target's network
//! namespace (`NetworkMode = container:<id>`), runs `tcpdump -w -`, and
//! streams the pcap bytes back to a file on the host. Works regardless of
//! whether the target image has tcpdump installed.
//!
//! Both the TUI and the headless CLI drive the same `start_capture` engine;
//! progress and completion are surfaced via `AppMessage::Capture*` variants
//! sent through the provided mpsc sender. Cancellation is via the supplied
//! `CancellationToken`.

mod sidecar;
mod spec;

pub use spec::{default_output_path, parse_bytes, parse_duration, CaptureSpec, CaptureTarget};

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bollard::container::LogOutput;
use futures_util::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::AppMessage;
use crate::cluster::DockerManager;

use sidecar::build_capture_config;

/// Resolve a `CaptureTarget` to a Docker container name/id.
async fn resolve_target(docker: &DockerManager, target: &CaptureTarget) -> Result<String> {
    match target {
        CaptureTarget::Pod { pod, namespace } => {
            docker.find_pod_pause_container(pod, namespace).await
        }
        CaptureTarget::Container(name) => {
            if !docker.container_exists(name).await {
                anyhow::bail!("Container '{}' not found", name);
            }
            Ok(name.clone())
        }
    }
}

/// Generate a sidecar name with a timestamp suffix so concurrent captures
/// don't collide.
fn sidecar_name() -> String {
    let nanos = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0);
    format!("k3dev-capture-{}", nanos)
}

/// Start a capture in the background.
///
/// Spawns a task that:
/// 1. Resolves the target → container id.
/// 2. Pulls the sidecar image if missing.
/// 3. Starts the sidecar (joins target netns, runs tcpdump, auto-removes on exit).
/// 4. Attaches to the sidecar's stdout/stderr.
/// 5. Streams stdout (pcap) bytes to `output_path`; surfaces stderr lines as status.
/// 6. Sends throttled progress via `CaptureProgress` and a final `CaptureComplete`
///    or `CaptureFailed` when it stops.
///
/// The caller cancels via the supplied `CancellationToken`; capture events come
/// back through `msg_tx` as `AppMessage::Capture*` variants.
pub async fn start_capture(
    spec: CaptureSpec,
    docker: Arc<DockerManager>,
    msg_tx: mpsc::Sender<AppMessage>,
    cancel: CancellationToken,
) -> Result<()> {
    // Make sure the output directory exists before we kick off any Docker work.
    if let Some(parent) = spec.output_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create capture dir {}", parent.display()))?;
    }

    let sidecar_name = sidecar_name();

    let docker_for_task = Arc::clone(&docker);
    tokio::spawn(async move {
        if let Err(e) =
            run_capture(spec, sidecar_name, docker_for_task, msg_tx.clone(), cancel).await
        {
            let _ = msg_tx
                .send(AppMessage::CaptureFailed(format!("{:#}", e)))
                .await;
        }
    });

    Ok(())
}

async fn run_capture(
    spec: CaptureSpec,
    sidecar_name: String,
    docker: Arc<DockerManager>,
    msg_tx: mpsc::Sender<AppMessage>,
    cancel: CancellationToken,
) -> Result<()> {
    // 1. Resolve target container.
    let target_id = resolve_target(&docker, &spec.target).await?;
    let _ = msg_tx
        .send(AppMessage::CaptureStatus(format!("Target: {}", target_id)))
        .await;

    // 2. Warn if target shares the host netns (capture would see all host traffic).
    if let Ok(Some(mode)) = docker.inspect_network_mode(&target_id).await {
        if mode == "host" {
            let _ = msg_tx
                .send(AppMessage::CaptureStatus(
                    "Warning: target uses hostNetwork — capture sees all host traffic".to_string(),
                ))
                .await;
        }
    }

    // 3. Pull the sidecar image if missing.
    if !docker.image_exists(&spec.image).await {
        let _ = msg_tx
            .send(AppMessage::CaptureStatus(format!(
                "Pulling capture image {}",
                spec.image
            )))
            .await;
        docker
            .pull_image(&spec.image)
            .await
            .with_context(|| format!("Failed to pull capture image {}", spec.image))?;
    }

    // 4. Build + start the sidecar.
    let target_label = match &spec.target {
        CaptureTarget::Pod { pod, namespace } => format!("pod {}/{}", namespace, pod),
        CaptureTarget::Container(name) => format!("container {}", name),
    };
    let cfg = build_capture_config(
        &sidecar_name,
        &target_id,
        &spec.image,
        &spec.iface,
        spec.filter.as_deref(),
        &target_label,
    );

    docker
        .run_container(&cfg)
        .await
        .with_context(|| format!("Failed to start capture sidecar {}", sidecar_name))?;

    let _ = msg_tx
        .send(AppMessage::CaptureStatus(format!(
            "Capture started → {}",
            spec.output_path.display()
        )))
        .await;

    // 5. Open output file + attach.
    //    If anything below fails, ensure we tear down the sidecar.
    let result = stream_to_file(&spec, &sidecar_name, &docker, &msg_tx, &cancel).await;

    // Always make sure the sidecar is dead. AutoRemove on the container
    // cleans up the filesystem entry once it stops.
    let _ = docker.kill_container(&sidecar_name, "SIGTERM").await;

    result
}

async fn stream_to_file(
    spec: &CaptureSpec,
    sidecar_name: &str,
    docker: &DockerManager,
    msg_tx: &mpsc::Sender<AppMessage>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut file = File::create(&spec.output_path)
        .await
        .with_context(|| format!("Failed to open {}", spec.output_path.display()))?;

    let attach = docker.attach_container_stream(sidecar_name).await?;
    let mut output = attach.output;

    let started = Instant::now();
    let mut bytes_written: u64 = 0;
    let mut last_progress = Instant::now();
    let mut packets_seen: Option<u64> = None;
    let mut stderr_buf = String::new();

    loop {
        // Build optional duration timeout future.
        let duration_left = spec
            .duration
            .map(|d| d.checked_sub(started.elapsed()).unwrap_or_default());

        let next = output.next();
        tokio::pin!(next);

        let outcome = if let Some(remaining) = duration_left {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => StepOutcome::Cancelled,
                _ = tokio::time::sleep(remaining) => StepOutcome::DurationElapsed,
                msg = &mut next => StepOutcome::Frame(msg),
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => StepOutcome::Cancelled,
                msg = &mut next => StepOutcome::Frame(msg),
            }
        };

        match outcome {
            StepOutcome::Cancelled => {
                let _ = msg_tx
                    .send(AppMessage::CaptureStatus("Stopping capture…".to_string()))
                    .await;
                break;
            }
            StepOutcome::DurationElapsed => {
                let _ = msg_tx
                    .send(AppMessage::CaptureStatus(
                        "Duration limit reached".to_string(),
                    ))
                    .await;
                break;
            }
            StepOutcome::Frame(None) => {
                // Stream closed (sidecar exited).
                break;
            }
            StepOutcome::Frame(Some(Err(e))) => {
                anyhow::bail!("Attach stream error: {}", e);
            }
            StepOutcome::Frame(Some(Ok(LogOutput::StdOut { message }))) => {
                file.write_all(&message)
                    .await
                    .with_context(|| format!("Write to {} failed", spec.output_path.display()))?;
                bytes_written += message.len() as u64;

                if let Some(limit) = spec.max_bytes {
                    if bytes_written >= limit {
                        let _ = msg_tx
                            .send(AppMessage::CaptureStatus("Byte limit reached".to_string()))
                            .await;
                        break;
                    }
                }

                if last_progress.elapsed() >= Duration::from_millis(500) {
                    let _ = msg_tx
                        .send(AppMessage::CaptureProgress {
                            bytes: bytes_written,
                            packets: packets_seen,
                        })
                        .await;
                    last_progress = Instant::now();
                }
            }
            StepOutcome::Frame(Some(Ok(LogOutput::StdErr { message }))) => {
                stderr_buf.push_str(&String::from_utf8_lossy(&message));
                while let Some(nl) = stderr_buf.find('\n') {
                    let line = stderr_buf[..nl].trim_end().to_string();
                    stderr_buf.drain(..=nl);
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(n) = parse_packet_count(&line) {
                        packets_seen = Some(n);
                    }
                    let _ = msg_tx.send(AppMessage::CaptureStatus(line)).await;
                }
            }
            StepOutcome::Frame(Some(Ok(_))) => {
                // Console / Stdin variants are not expected; ignore.
            }
        }
    }

    file.flush().await.ok();
    drop(file);

    let _ = msg_tx
        .send(AppMessage::CaptureComplete {
            path: spec.output_path.clone(),
            bytes: bytes_written,
            packets: packets_seen,
        })
        .await;

    Ok(())
}

enum StepOutcome {
    Cancelled,
    DurationElapsed,
    Frame(Option<Result<LogOutput, bollard::errors::Error>>),
}

/// Parse a tcpdump status line like "12345 packets captured" → 12345.
static PACKET_COUNT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d+)\s+packets\s+captured").unwrap());

fn parse_packet_count(line: &str) -> Option<u64> {
    PACKET_COUNT_RE
        .captures(line)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_packet_count_matches() {
        assert_eq!(parse_packet_count("142 packets captured"), Some(142));
        assert_eq!(parse_packet_count("0 packets captured by filter"), Some(0));
        assert_eq!(parse_packet_count("listening on any"), None);
    }
}
