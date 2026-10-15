//! Shared Docker API relay behind `k3dev docker` on macOS.
//!
//! Docker Desktop's host socket is a proxy that hides the pod containers
//! cri-dockerd creates, so `k3dev docker` talks to the VM's raw socket instead,
//! through socat in a helper container that shares the VM's PID namespace.
//! Every cluster runs on that same daemon, so one relay serves all of them and
//! outlives any single cluster.
//!
//! The relay is the unauthenticated Docker API, so it is published on loopback
//! only. Docker picks the host port, so it can never collide with anything else
//! on the host; callers read the port back each time.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::docker::{ContainerRunConfig, DockerManager, PortMapping};
use super::platform::{docker_host_tcp_url, PlatformInfo};

/// Name of the shared relay container
pub const RELAY_CONTAINER: &str = "k3dev-docker-relay";

/// Port socat listens on inside the relay container
const RELAY_PORT: u16 = 2375;

const SOCAT_DIR: &str = "/usr/local/bin";
const SOCAT_PATH: &str = "/usr/local/bin/socat";

/// The VM's own Docker socket, reached through PID 1's root (`--pid=host`)
const RAW_DOCKER_SOCK: &str = "/proc/1/root/run/docker.sock";

/// How long a fresh relay gets to start answering
const READY_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_arch = "aarch64")]
const SOCAT_BINARY: &[u8] = include_bytes!("../../assets/socat-aarch64");
#[cfg(target_arch = "x86_64")]
const SOCAT_BINARY: &[u8] = include_bytes!("../../assets/socat-x86_64");

/// Whether the docker CLI needs the relay at all. A TCP or remote endpoint is
/// not Docker Desktop's filtering socket, and the relay's loopback port would
/// be on the wrong machine for a remote daemon anyway.
pub fn needed() -> bool {
    docker_host_tcp_url().is_none() && !PlatformInfo::is_docker_remote()
}

/// Make sure the relay is running and return the loopback port it answers on.
///
/// `image` is only used to create the relay: the k3dev k3s image ships socat,
/// and the embedded copy covers a custom image without it.
pub async fn ensure(docker: &DockerManager, image: &str) -> Result<u16> {
    if !docker.container_exists(RELAY_CONTAINER).await {
        create(docker, image).await?;
    } else if !docker.container_running(RELAY_CONTAINER).await
        && docker.start_container(RELAY_CONTAINER).await.is_err()
        && !docker.container_running(RELAY_CONTAINER).await
    {
        // Holds no state, so a relay that won't start is simply replaced
        docker.remove_container(RELAY_CONTAINER, true).await?;
        create(docker, image).await?;
    }

    let socat_present = docker
        .exec_status(
            RELAY_CONTAINER,
            &["/bin/sh", "-c", &format!("test -x {}", SOCAT_PATH)],
        )
        .await?;
    if !socat_present {
        docker
            .exec_in_container(RELAY_CONTAINER, &["mkdir", "-p", SOCAT_DIR])
            .await?;
        docker
            .copy_to_container(RELAY_CONTAINER, "socat", SOCAT_BINARY, SOCAT_DIR)
            .await
            .context("Failed to install socat in the Docker relay")?;
    }

    let port = docker
        .get_container_ports(RELAY_CONTAINER)
        .await?
        .get(&RELAY_PORT)
        .copied()
        .ok_or_else(|| anyhow!("{} publishes no port for the relay", RELAY_CONTAINER))?;

    wait_ready(port).await?;
    Ok(port)
}

/// Remove the relay once no cluster is running.
///
/// It only serves `k3dev docker` against a running cluster, and it is a
/// privileged container exposing the Docker API, so it should not linger;
/// the next `k3dev docker` recreates it.
pub async fn remove_if_idle(docker: &DockerManager) {
    if !docker.list_cluster_servers().await.is_empty()
        || !docker.container_exists(RELAY_CONTAINER).await
    {
        return;
    }
    match docker.remove_container(RELAY_CONTAINER, true).await {
        Ok(()) => tracing::debug!("Removed Docker relay (no clusters running)"),
        Err(e) => tracing::warn!(error = %e, "Failed to remove Docker relay"),
    }
}

async fn create(docker: &DockerManager, image: &str) -> Result<()> {
    if !docker.image_exists(image).await {
        docker
            .pull_image(image)
            .await
            .with_context(|| format!("Failed to pull {} for the Docker relay", image))?;
    }

    let mut labels = HashMap::new();
    labels.insert("k3dev.role".to_string(), "docker-relay".to_string());

    let run_config = ContainerRunConfig {
        name: RELAY_CONTAINER.to_string(),
        image: image.to_string(),
        detach: true,
        // Opening another process's root through /proc takes ptrace rights
        privileged: true,
        pid_host: true,
        ports: vec![PortMapping::loopback(0, RELAY_PORT)],
        entrypoint: Some(String::new()),
        command: Some(relay_command()),
        labels,
        ..Default::default()
    };

    if let Err(e) = docker.run_container(&run_config).await {
        // Two `k3dev docker` calls at once both try to create the relay; the
        // loser gets a name conflict and uses the winner's.
        if !is_name_conflict(&e) {
            return Err(e).context("Failed to create Docker relay container");
        }
        tracing::debug!("Docker relay created concurrently: {:#}", e);
        wait_for_concurrent_create(docker).await?;
    }

    Ok(())
}

fn is_name_conflict(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<bollard::errors::Error>(),
        Some(bollard::errors::Error::DockerResponseServerError {
            status_code: 409,
            ..
        })
    )
}

/// Wait for the relay another caller is creating to come up.
///
/// Docker reserves the name before the container can be inspected, so right
/// after the conflict it may not even exist yet. If it never starts on its
/// own (that caller died), start it here.
async fn wait_for_concurrent_create(docker: &DockerManager) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if docker.container_running(RELAY_CONTAINER).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    docker
        .start_container(RELAY_CONTAINER)
        .await
        .context("Failed to start Docker relay container")
}

/// socat may only be uploaded after the container starts (see `ensure`), so
/// the command waits for it before handing over.
fn relay_command() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!(
            "until [ -x {socat} ]; do sleep 0.1; done; \
             exec {socat} TCP-LISTEN:{port},fork,reuseaddr UNIX-CONNECT:{sock}",
            socat = SOCAT_PATH,
            port = RELAY_PORT,
            sock = RAW_DOCKER_SOCK,
        ),
    ]
}

/// Wait until the Docker API answers through the relay.
///
/// A TCP connect alone proves nothing: Docker Desktop accepts on the published
/// port before socat listens behind it.
async fn wait_ready(port: u16) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if ping(port).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "Docker relay on 127.0.0.1:{} did not answer within {}s",
                port,
                READY_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn ping(port: u16) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        stream
            .write_all(b"GET /_ping HTTP/1.0\r\n\r\n")
            .await
            .ok()?;
        let mut status = [0u8; 12];
        stream.read_exact(&mut status).await.ok()?;
        Some(is_ok_status(&status))
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(1), attempt).await,
        Ok(Some(true))
    )
}

/// Whether an HTTP status line starts with a 200 (`HTTP/1.x 200`)
fn is_ok_status(status: &[u8]) -> bool {
    status.len() >= 12 && status.starts_with(b"HTTP/1.") && &status[8..12] == b" 200"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_waits_for_socat_and_targets_the_raw_socket() {
        let command = relay_command();
        let script = &command[2];
        assert!(script.starts_with("until [ -x /usr/local/bin/socat ]"));
        assert!(script.contains("TCP-LISTEN:2375,fork,reuseaddr"));
        assert!(script.ends_with("UNIX-CONNECT:/proc/1/root/run/docker.sock"));
    }

    #[test]
    fn only_a_200_counts_as_ready() {
        assert!(is_ok_status(b"HTTP/1.1 200 OK"));
        assert!(is_ok_status(b"HTTP/1.0 200 OK"));
        assert!(!is_ok_status(b"HTTP/1.1 500 Internal"));
        assert!(!is_ok_status(b""));
        assert!(!is_ok_status(b"garbage-bytes"));
    }
}
