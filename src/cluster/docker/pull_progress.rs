//! Image pull progress tracking via Docker stream API
//!
//! This module tracks Docker image pull progress by joining the existing
//! kubelet-initiated pull via Docker's `create_image` streaming API.
//! Docker's transfer manager deduplicates the download and provides
//! real-time byte-level `progress_detail` per layer.
//!
//! Additionally, a registry manifest is pre-fetched before starting the
//! Docker stream to obtain accurate total sizes upfront, avoiding the
//! "growing denominator" problem where total bytes increase as layers
//! are discovered.

use bollard::image::CreateImageOptions;
use bollard::Docker;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, WWW_AUTHENTICATE};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, Semaphore};

use crate::app::AppMessage;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Maximum duration for a single image pull operation (30 minutes)
const PULL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Maximum time without any byte progress before aborting (5 minutes)
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Maximum concurrent manifest fetches to avoid overwhelming registries
const MAX_CONCURRENT_MANIFEST_FETCHES: usize = 5;

/// Time-to-live for cached manifest entries (10 minutes)
const MANIFEST_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

// ─── Manifest cache ─────────────────────────────────────────────────────────

struct ManifestCacheEntry {
    info: ManifestInfo,
    fetched_at: Instant,
}

static MANIFEST_CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, ManifestCacheEntry>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

static MANIFEST_SEMAPHORE: once_cell::sync::Lazy<Semaphore> =
    once_cell::sync::Lazy::new(|| Semaphore::new(MAX_CONCURRENT_MANIFEST_FETCHES));

/// Look up a cached manifest, returning None if absent or expired
async fn get_cached_manifest(cache_key: &str) -> Option<ManifestInfo> {
    let cache = MANIFEST_CACHE.lock().await;
    if let Some(entry) = cache.get(cache_key) {
        if entry.fetched_at.elapsed() < MANIFEST_CACHE_TTL {
            return Some(entry.info.clone());
        }
    }
    None
}

/// Store a manifest in the cache
async fn set_cached_manifest(cache_key: String, info: ManifestInfo) {
    let mut cache = MANIFEST_CACHE.lock().await;
    cache.insert(
        cache_key,
        ManifestCacheEntry {
            info,
            fetched_at: Instant::now(),
        },
    );
    // Evict expired entries opportunistically
    cache.retain(|_, entry| entry.fetched_at.elapsed() < MANIFEST_CACHE_TTL);
}

// ─── Manifest types (internal) ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ManifestV2 {
    #[serde(rename = "mediaType")]
    #[allow(dead_code)]
    media_type: Option<String>,
    layers: Option<Vec<ManifestLayer>>,
    manifests: Option<Vec<ManifestListEntry>>,
}

#[derive(Debug, Deserialize)]
struct ManifestLayer {
    size: Option<u64>,
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManifestListEntry {
    digest: Option<String>,
    platform: Option<ManifestPlatform>,
}

#[derive(Debug, Deserialize)]
struct ManifestPlatform {
    architecture: Option<String>,
    os: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// Pre-fetched manifest info with total size and per-layer sizes
#[derive(Debug, Clone)]
struct ManifestInfo {
    total_bytes: u64,
    /// Maps 12-char digest prefix to compressed layer size
    layer_sizes: HashMap<String, u64>,
}

// ─── Pull phase tracking ────────────────────────────────────────────────────

/// Phase of an individual layer during pull
#[derive(Debug, Clone, PartialEq, Eq)]
enum LayerPhase {
    Pending,
    Downloading,
    Extracting,
    Done,
    Cached,
}

impl LayerPhase {
    fn from_status(s: &str) -> Self {
        match s {
            "Pulling fs layer" | "Waiting" | "Downloading" => Self::Downloading,
            "Verifying Checksum" | "Download complete" => Self::Downloading,
            "Extracting" => Self::Extracting,
            "Pull complete" => Self::Done,
            "Already exists" => Self::Cached,
            _ => Self::Pending,
        }
    }
}

/// Overall phase of the image pull operation
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PullPhase {
    #[default]
    Downloading,
    Extracting,
    Complete,
}

/// Per-layer tracking state (internal)
#[derive(Debug)]
struct LayerTracker {
    phase: LayerPhase,
    current_bytes: u64,
    total_bytes: u64,
    /// Manifest-reported size for this layer (set once, zeroed after "Already exists")
    manifest_bytes: u64,
}

/// Progress for a single container's image pull
#[derive(Debug, Clone, Default)]
pub struct ContainerPullProgress {
    pub container_name: String,
    pub image: String,
    /// Total bytes to download (sum of all layers)
    pub total_bytes: u64,
    /// Bytes downloaded so far
    pub downloaded_bytes: u64,
    /// Progress percentage (0-100)
    pub progress_percent: f64,
    /// Whether progress tracking is available (registry accessible, etc.)
    pub tracking_available: bool,
    /// Current phase of the pull operation
    pub phase: PullPhase,
    /// Number of layers completed (downloaded + cached)
    pub layers_done: u16,
    /// Total number of layers
    pub layers_total: u16,
}

impl ContainerPullProgress {
    pub fn new(container_name: &str, image: &str) -> Self {
        Self {
            container_name: container_name.to_string(),
            image: image.to_string(),
            ..Default::default()
        }
    }

    pub fn with_progress(mut self, downloaded: u64, total: u64) -> Self {
        self.downloaded_bytes = downloaded;
        self.total_bytes = total;
        self.progress_percent = if total > 0 {
            (downloaded as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        self.tracking_available = total > 0;
        self
    }

    pub fn with_phase(mut self, phase: PullPhase) -> Self {
        self.phase = phase;
        self
    }

    pub fn with_layers(mut self, done: u16, total: u16) -> Self {
        self.layers_done = done;
        self.layers_total = total;
        self
    }
}

/// Parsed image reference
#[derive(Debug, Clone)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

impl ImageRef {
    /// Parse an image reference like "nginx:1.25" or "docker.io/library/nginx:1.25"
    pub fn parse(image: &str) -> Self {
        let (image_part, tag) = if let Some(at_pos) = image.rfind('@') {
            // Handle digest references like nginx@sha256:...
            (&image[..at_pos], &image[at_pos + 1..])
        } else if let Some(colon_pos) = image.rfind(':') {
            // Check if colon is part of port (registry:port/image)
            let before_colon = &image[..colon_pos];
            let after_colon = &image[colon_pos + 1..];
            if before_colon.contains('/') || !after_colon.contains('/') {
                // If there's a slash before the colon, this is a tag separator
                // If there's no slash after the colon, this is also a tag separator
                (&image[..colon_pos], after_colon)
            } else {
                // Colon before any slash and slash after = could be port
                // Validate port: must be numeric
                let potential_port = after_colon.split('/').next().unwrap_or("");
                if potential_port.parse::<u16>().is_ok() {
                    (image, "latest")
                } else {
                    // Not a valid port, treat as tag
                    (&image[..colon_pos], after_colon)
                }
            }
        } else {
            (image, "latest")
        };

        let parts: Vec<&str> = image_part.split('/').collect();

        let (registry, repository) = match parts.len() {
            1 => {
                // Simple image like "nginx" -> docker.io/library/nginx
                (
                    "registry-1.docker.io".to_string(),
                    format!("library/{}", parts[0]),
                )
            }
            2 => {
                // Could be "user/repo" or "registry/repo"
                if parts[0].contains('.') || parts[0].contains(':') {
                    // It's a registry
                    (parts[0].to_string(), parts[1].to_string())
                } else {
                    // It's user/repo on Docker Hub
                    (
                        "registry-1.docker.io".to_string(),
                        format!("{}/{}", parts[0], parts[1]),
                    )
                }
            }
            _ => {
                // Full path like "registry.example.com/path/to/image"
                let registry = parts[0].to_string();
                let repo = parts[1..].join("/");
                (registry, repo)
            }
        };

        Self {
            registry,
            repository,
            tag: tag.to_string(),
        }
    }

    /// Cache key for manifest lookups
    fn cache_key(&self) -> String {
        format!("{}/{}/{}", self.registry, self.repository, self.tag)
    }
}

// ─── Manifest pre-fetch ─────────────────────────────────────────────────────

/// Fetch registry manifest with caching and concurrency limiting.
/// Returns None on any failure (timeout, auth error, parse error).
async fn fetch_manifest_cached(image_ref: &ImageRef) -> Option<ManifestInfo> {
    let cache_key = image_ref.cache_key();

    // Check cache first
    if let Some(cached) = get_cached_manifest(&cache_key).await {
        tracing::debug!(image_key = %cache_key, "Manifest cache hit");
        return Some(cached);
    }

    // Acquire semaphore permit to limit concurrent fetches
    let _permit = MANIFEST_SEMAPHORE.acquire().await.ok()?;

    // Double-check cache after acquiring permit (another task may have populated it)
    if let Some(cached) = get_cached_manifest(&cache_key).await {
        return Some(cached);
    }

    let result = fetch_manifest_total(image_ref).await;

    // Cache successful results
    if let Some(ref info) = result {
        set_cached_manifest(cache_key, info.clone()).await;
    }

    result
}

/// Fetch registry manifest to get accurate total layer sizes upfront.
/// Returns None on any failure (timeout, auth error, parse error).
async fn fetch_manifest_total(image_ref: &ImageRef) -> Option<ManifestInfo> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;

    let manifest_url = format!(
        "https://{}/v2/{}/manifests/{}",
        image_ref.registry, image_ref.repository, image_ref.tag
    );

    let accept_headers = [
        "application/vnd.docker.distribution.manifest.v2+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.oci.image.index.v1+json",
    ]
    .join(", ");

    let resp = client
        .get(&manifest_url)
        .header(ACCEPT, &accept_headers)
        .send()
        .await
        .ok()?;

    // Handle 401 with bearer token auth (Docker Hub)
    let body = if resp.status().as_u16() == 401 {
        let www_auth = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let token = fetch_bearer_token(&client, www_auth).await?;

        let resp2 = client
            .get(&manifest_url)
            .header(ACCEPT, &accept_headers)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .ok()?;

        if !resp2.status().is_success() {
            return None;
        }
        resp2.text().await.ok()?
    } else if resp.status().is_success() {
        resp.text().await.ok()?
    } else {
        return None;
    };

    let manifest: ManifestV2 = serde_json::from_str(&body).ok()?;

    // If manifest list (multi-arch), resolve to current platform
    if let Some(manifests) = &manifest.manifests {
        let target_arch = target_architecture();
        let entry = manifests.iter().find(|m| {
            m.platform
                .as_ref()
                .map(|p| {
                    p.architecture.as_deref() == Some(target_arch)
                        && p.os.as_deref() == Some("linux")
                })
                .unwrap_or(false)
        })?;

        let digest = entry.digest.as_deref()?;
        return fetch_manifest_by_digest(&client, image_ref, digest).await;
    }

    // Single manifest with layers
    extract_manifest_info(&manifest)
}

/// Resolve a manifest list entry by digest to get layer details
async fn fetch_manifest_by_digest(
    client: &reqwest::Client,
    image_ref: &ImageRef,
    digest: &str,
) -> Option<ManifestInfo> {
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        image_ref.registry, image_ref.repository, digest
    );
    let accept = "application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json";

    let resp = client.get(&url).header(ACCEPT, accept).send().await.ok()?;

    let body = if resp.status().as_u16() == 401 {
        let www_auth = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = fetch_bearer_token(client, www_auth).await?;
        let resp2 = client
            .get(&url)
            .header(ACCEPT, accept)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .ok()?;
        if !resp2.status().is_success() {
            return None;
        }
        resp2.text().await.ok()?
    } else if resp.status().is_success() {
        resp.text().await.ok()?
    } else {
        return None;
    };

    let manifest: ManifestV2 = serde_json::from_str(&body).ok()?;
    extract_manifest_info(&manifest)
}

/// Extract ManifestInfo from a parsed single-platform manifest
fn extract_manifest_info(manifest: &ManifestV2) -> Option<ManifestInfo> {
    let layers = manifest.layers.as_ref()?;
    let mut total_bytes: u64 = 0;
    let mut layer_sizes: HashMap<String, u64> = HashMap::new();

    for layer in layers {
        let size = layer.size.unwrap_or(0);
        total_bytes += size;
        if let Some(digest) = &layer.digest {
            // Use 12-char prefix of the hash part (after "sha256:")
            let prefix = digest_prefix(digest);
            layer_sizes.insert(prefix, size);
        }
    }

    if total_bytes == 0 {
        return None;
    }

    Some(ManifestInfo {
        total_bytes,
        layer_sizes,
    })
}

/// Extract 12-char digest prefix (Docker uses this as layer ID in stream)
fn digest_prefix(digest: &str) -> String {
    let hash = digest.strip_prefix("sha256:").unwrap_or(digest);
    hash.chars().take(12).collect()
}

/// Parse WWW-Authenticate header and fetch bearer token
async fn fetch_bearer_token(client: &reqwest::Client, www_auth: &str) -> Option<String> {
    let auth_str = www_auth
        .strip_prefix("Bearer ")
        .or_else(|| www_auth.strip_prefix("bearer "))?;

    let mut realm = String::new();
    let mut service = String::new();
    let mut scope = String::new();

    for (key, value) in split_auth_params(auth_str) {
        match key {
            "realm" => realm = value,
            "service" => service = value,
            "scope" => scope = value,
            _ => {}
        }
    }

    if realm.is_empty() {
        return None;
    }

    let mut token_url = format!("{}?service={}", realm, service);
    if !scope.is_empty() {
        token_url.push_str(&format!("&scope={}", scope));
    }

    let resp = client.get(&token_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let token_resp: TokenResponse = resp.json().await.ok()?;
    token_resp.token.or(token_resp.access_token)
}

/// Split "realm=\"...\",service=\"...\",scope=\"...\"" into key-value pairs.
/// Handles quoted values (including escaped quotes), unquoted values,
/// and various whitespace/comma combinations.
fn split_auth_params(s: &str) -> Vec<(&str, String)> {
    let mut result = Vec::new();
    let mut remaining = s;

    while !remaining.is_empty() {
        remaining = remaining.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        if remaining.is_empty() {
            break;
        }

        let eq_pos = match remaining.find('=') {
            Some(p) => p,
            None => break,
        };

        let key = remaining[..eq_pos].trim();
        remaining = &remaining[eq_pos + 1..];

        let value = if remaining.starts_with('"') {
            remaining = &remaining[1..];
            // Find closing quote, handling escaped quotes
            let mut end = 0;
            let bytes = remaining.as_bytes();
            while end < bytes.len() {
                if bytes[end] == b'"' && (end == 0 || bytes[end - 1] != b'\\') {
                    break;
                }
                end += 1;
            }
            let val = remaining[..end].replace("\\\"", "\"");
            if end < remaining.len() {
                remaining = &remaining[end + 1..];
            } else {
                remaining = "";
            }
            val
        } else {
            let end = remaining
                .find(|c: char| c == ',' || c.is_whitespace())
                .unwrap_or(remaining.len());
            let val = remaining[..end].to_string();
            remaining = &remaining[end..];
            val
        };

        result.push((key, value));
    }

    result
}

/// Get target architecture string for manifest resolution
fn target_architecture() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

// ─── Pull monitor ───────────────────────────────────────────────────────────

/// RAII guard that sends ImagePullMonitorDone when dropped,
/// ensuring cleanup even on panics or early returns.
struct PullMonitorGuard {
    image: String,
    message_tx: mpsc::Sender<AppMessage>,
}

impl Drop for PullMonitorGuard {
    fn drop(&mut self) {
        let image = self.image.clone();
        let tx = self.message_tx.clone();
        // Best-effort send; if the channel is closed, the app is shutting down anyway
        tokio::spawn(async move {
            let _ = tx.send(AppMessage::ImagePullMonitorDone(image)).await;
        });
    }
}

/// Monitor an image pull by joining Docker's create_image stream.
///
/// Before starting the stream, attempts to pre-fetch the registry manifest
/// to obtain accurate total layer sizes. This prevents the "growing denominator"
/// problem where total bytes increase as layers are discovered.
///
/// If the image is already fully pulled, the stream returns immediately.
///
/// The `container_name` parameter is passed through to progress updates for
/// better multi-container pod identification.
///
/// Enforces two timeouts:
/// - Overall pull timeout (30 minutes)
/// - Inactivity timeout (5 minutes without byte progress)
pub async fn monitor_image_pull(
    docker: Docker,
    image: String,
    container_name: String,
    message_tx: mpsc::Sender<AppMessage>,
    manifest_semaphore: Arc<Semaphore>,
) {
    // RAII guard ensures cleanup message is always sent
    let _guard = PullMonitorGuard {
        image: image.clone(),
        message_tx: message_tx.clone(),
    };

    let pull_start = Instant::now();
    let image_ref = ImageRef::parse(&image);

    // Pre-fetch manifest with 5s timeout, caching, and concurrency limit
    let manifest_info = {
        let _permit = manifest_semaphore.acquire().await.ok();
        match tokio::time::timeout(Duration::from_secs(5), fetch_manifest_cached(&image_ref)).await
        {
            Ok(result) => {
                if let Some(ref info) = result {
                    tracing::debug!(
                        image = %image,
                        total_bytes = info.total_bytes,
                        layers = info.layer_sizes.len(),
                        "Manifest pre-fetch succeeded"
                    );
                } else {
                    tracing::info!(
                        image = %image,
                        "Manifest pre-fetch unavailable — progress will use stream data only"
                    );
                }
                result
            }
            Err(_) => {
                tracing::info!(
                    image = %image,
                    "Manifest pre-fetch timed out — progress will use stream data only"
                );
                None
            }
        }
    };

    let manifest_total = manifest_info.as_ref().map(|m| m.total_bytes).unwrap_or(0);

    // Start Docker create_image stream
    let from_image = if image_ref.registry.contains("docker.io") {
        image_ref.repository.clone()
    } else {
        format!("{}/{}", image_ref.registry, image_ref.repository)
    };
    let tag = image_ref.tag.clone();

    let options = Some(CreateImageOptions {
        from_image: from_image.as_str(),
        tag: tag.as_str(),
        ..Default::default()
    });

    let mut stream = docker.create_image(options, None, None);

    // Per-layer tracking
    let mut layers: HashMap<String, LayerTracker> = HashMap::new();
    let mut last_send = Instant::now();
    let mut high_current: u64 = 0;
    let mut high_total: u64 = 0;
    let mut last_bytes_change = Instant::now();
    let mut last_total_bytes: u64 = 0;
    // Track how much manifest_total to subtract for cached layers
    let mut cached_subtract: u64 = 0;

    // If we have manifest info, send initial progress with layer count
    if let Some(ref info) = manifest_info {
        let progress = ContainerPullProgress::new(&container_name, &image)
            .with_layers(0, info.layer_sizes.len() as u16)
            .with_progress(0, info.total_bytes);
        let mut map = HashMap::new();
        map.insert(image.clone(), progress);
        let _ = message_tx.send(AppMessage::PullProgressUpdated(map)).await;
    }

    while let Some(result) = stream.next().await {
        // Check overall pull timeout
        if pull_start.elapsed() > PULL_TIMEOUT {
            tracing::warn!(
                image = %image,
                elapsed_mins = pull_start.elapsed().as_secs() / 60,
                "Pull operation timed out"
            );
            break;
        }

        // Check inactivity timeout
        if last_bytes_change.elapsed() > INACTIVITY_TIMEOUT {
            tracing::warn!(
                image = %image,
                inactivity_secs = last_bytes_change.elapsed().as_secs(),
                "Pull stalled — no byte progress"
            );
            break;
        }

        let info = match result {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!(image = %image, error = %e, "Pull monitor stream error");
                break;
            }
        };

        // Update per-layer tracking from stream events
        if let Some(id) = &info.id {
            let status = info.status.as_deref().unwrap_or("");
            let new_phase = LayerPhase::from_status(status);

            let layer = layers.entry(id.clone()).or_insert_with(|| {
                // Look up manifest size for this layer
                let manifest_bytes = manifest_info
                    .as_ref()
                    .and_then(|m| m.layer_sizes.get(id).copied())
                    .unwrap_or(0);

                LayerTracker {
                    phase: LayerPhase::Pending,
                    current_bytes: 0,
                    total_bytes: 0,
                    manifest_bytes,
                }
            });

            layer.phase = new_phase;

            // Update bytes from progress_detail, capping per-layer current to total
            if let Some(detail) = &info.progress_detail {
                if let (Some(current), Some(total)) = (detail.current, detail.total) {
                    if total > 0 {
                        let total_u64 = total as u64;
                        layer.total_bytes = total_u64;
                        // Cap current_bytes to total_bytes per-layer (prevents overflow)
                        layer.current_bytes = (current as u64).min(total_u64);
                    }
                }
            }

            // Handle "Already exists": subtract manifest bytes from total (once)
            if layer.phase == LayerPhase::Cached && layer.manifest_bytes > 0 {
                cached_subtract += layer.manifest_bytes;
                layer.manifest_bytes = 0; // prevent double-subtract
            }
        }

        // Throttle: send at most every 500ms
        if last_send.elapsed() >= Duration::from_millis(500) {
            let (progress, has_data) =
                aggregate_progress(&layers, manifest_total, cached_subtract, &manifest_info);

            if has_data {
                let mono_total = progress.total_bytes.max(high_total);
                let mono_current = progress.downloaded_bytes.max(high_current);
                high_total = mono_total;
                high_current = mono_current;

                // Track inactivity: reset timer when bytes actually change
                if mono_current != last_total_bytes {
                    last_bytes_change = Instant::now();
                    last_total_bytes = mono_current;
                }

                let final_progress = ContainerPullProgress::new(&container_name, &image)
                    .with_progress(mono_current, mono_total)
                    .with_phase(progress.phase)
                    .with_layers(progress.layers_done, progress.layers_total);

                let mut map = HashMap::new();
                map.insert(image.clone(), final_progress);
                let _ = message_tx.send(AppMessage::PullProgressUpdated(map)).await;
                last_send = Instant::now();
            }
        }
    }

    // PullMonitorGuard::drop sends ImagePullMonitorDone automatically
}

/// Aggregate per-layer state into overall progress
fn aggregate_progress(
    layers: &HashMap<String, LayerTracker>,
    manifest_total: u64,
    cached_subtract: u64,
    manifest_info: &Option<ManifestInfo>,
) -> (ContainerPullProgress, bool) {
    if layers.is_empty() {
        return (ContainerPullProgress::default(), false);
    }

    let mut stream_total: u64 = 0;
    let mut stream_current: u64 = 0;
    let mut any_downloading = false;
    let mut any_extracting = false;
    let mut layers_done: u16 = 0;
    let mut layers_total: u16 = 0;

    for tracker in layers.values() {
        match tracker.phase {
            LayerPhase::Cached => {
                layers_done += 1;
                layers_total += 1;
            }
            LayerPhase::Done => {
                layers_done += 1;
                layers_total += 1;
                stream_current += tracker.total_bytes;
                stream_total += tracker.total_bytes;
            }
            LayerPhase::Extracting => {
                any_extracting = true;
                layers_total += 1;
                stream_current += tracker.total_bytes; // download finished
                stream_total += tracker.total_bytes;
            }
            LayerPhase::Downloading => {
                any_downloading = true;
                layers_total += 1;
                // current_bytes already capped per-layer during tracking
                stream_current += tracker.current_bytes;
                stream_total += tracker.total_bytes;
            }
            LayerPhase::Pending => {
                layers_total += 1;
            }
        }
    }

    // Use manifest total as denominator if available (more accurate)
    let effective_total = if manifest_total > 0 {
        manifest_total.saturating_sub(cached_subtract)
    } else {
        stream_total
    };

    // Use manifest layer count if we have it and it's higher
    if let Some(info) = manifest_info {
        let manifest_layers = info.layer_sizes.len() as u16;
        if manifest_layers > layers_total {
            layers_total = manifest_layers;
        }
    }

    let phase = if !any_downloading && any_extracting {
        PullPhase::Extracting
    } else if layers_done == layers_total && layers_total > 0 {
        PullPhase::Complete
    } else {
        PullPhase::Downloading
    };

    let has_data = effective_total > 0 || layers_total > 0;

    let progress = ContainerPullProgress {
        container_name: String::new(),
        image: String::new(),
        total_bytes: effective_total,
        downloaded_bytes: stream_current.min(effective_total),
        progress_percent: if effective_total > 0 {
            (stream_current.min(effective_total) as f64 / effective_total as f64 * 100.0).min(100.0)
        } else {
            0.0
        },
        tracking_available: effective_total > 0,
        phase,
        layers_done,
        layers_total,
    };

    (progress, has_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_image() {
        let img = ImageRef::parse("nginx");
        assert_eq!(img.registry, "registry-1.docker.io");
        assert_eq!(img.repository, "library/nginx");
        assert_eq!(img.tag, "latest");
    }

    #[test]
    fn test_parse_image_with_tag() {
        let img = ImageRef::parse("nginx:1.25");
        assert_eq!(img.registry, "registry-1.docker.io");
        assert_eq!(img.repository, "library/nginx");
        assert_eq!(img.tag, "1.25");
    }

    #[test]
    fn test_parse_user_image() {
        let img = ImageRef::parse("myuser/myimage:v1");
        assert_eq!(img.registry, "registry-1.docker.io");
        assert_eq!(img.repository, "myuser/myimage");
        assert_eq!(img.tag, "v1");
    }

    #[test]
    fn test_parse_custom_registry() {
        let img = ImageRef::parse("ghcr.io/owner/repo:latest");
        assert_eq!(img.registry, "ghcr.io");
        assert_eq!(img.repository, "owner/repo");
        assert_eq!(img.tag, "latest");
    }

    #[test]
    fn test_parse_registry_with_port() {
        let img = ImageRef::parse("localhost:5000/myimage:v1");
        assert_eq!(img.registry, "localhost:5000");
        assert_eq!(img.repository, "myimage");
        assert_eq!(img.tag, "v1");
    }

    #[test]
    fn test_parse_registry_with_port_no_tag() {
        let img = ImageRef::parse("localhost:5000/myimage");
        assert_eq!(img.registry, "localhost:5000");
        assert_eq!(img.repository, "myimage");
        assert_eq!(img.tag, "latest");
    }

    #[test]
    fn test_parse_digest_reference() {
        let img = ImageRef::parse("nginx@sha256:abc123def456");
        assert_eq!(img.registry, "registry-1.docker.io");
        assert_eq!(img.repository, "library/nginx");
        assert_eq!(img.tag, "sha256:abc123def456");
    }

    #[test]
    fn test_parse_nested_path() {
        let img = ImageRef::parse("registry.io/org/team/image:v1");
        assert_eq!(img.registry, "registry.io");
        assert_eq!(img.repository, "org/team/image");
        assert_eq!(img.tag, "v1");
    }

    #[test]
    fn test_parse_registry_with_invalid_port() {
        // "localhost:abc/image" — abc is not a valid port, treat as tag
        // "localhost" alone looks like a simple image, so gets library/ prefix
        let img = ImageRef::parse("localhost:abc/image");
        assert_eq!(img.registry, "registry-1.docker.io");
        assert_eq!(img.repository, "library/localhost");
        assert_eq!(img.tag, "abc/image");
    }

    #[test]
    fn test_with_progress_zero_total() {
        let p = ContainerPullProgress::new("test", "img").with_progress(0, 0);
        assert_eq!(p.progress_percent, 0.0);
        assert!(!p.tracking_available);
    }

    #[test]
    fn test_with_progress_partial() {
        let p = ContainerPullProgress::new("test", "img").with_progress(50, 100);
        assert!((p.progress_percent - 50.0).abs() < 0.01);
        assert!(p.tracking_available);
    }

    #[test]
    fn test_with_progress_complete() {
        let p = ContainerPullProgress::new("test", "img").with_progress(100, 100);
        assert!((p.progress_percent - 100.0).abs() < 0.01);
        assert!(p.tracking_available);
    }

    #[test]
    fn test_with_progress_overflow_capped() {
        let p = ContainerPullProgress::new("test", "img").with_progress(200, 100);
        assert!((p.progress_percent - 100.0).abs() < 0.01);
    }

    // ─── Layer phase tests ──────────────────────────────────────────────────

    #[test]
    fn test_layer_phase_from_status() {
        assert_eq!(
            LayerPhase::from_status("Pulling fs layer"),
            LayerPhase::Downloading
        );
        assert_eq!(LayerPhase::from_status("Waiting"), LayerPhase::Downloading);
        assert_eq!(
            LayerPhase::from_status("Downloading"),
            LayerPhase::Downloading
        );
        assert_eq!(
            LayerPhase::from_status("Verifying Checksum"),
            LayerPhase::Downloading
        );
        assert_eq!(
            LayerPhase::from_status("Download complete"),
            LayerPhase::Downloading
        );
        assert_eq!(
            LayerPhase::from_status("Extracting"),
            LayerPhase::Extracting
        );
        assert_eq!(LayerPhase::from_status("Pull complete"), LayerPhase::Done);
        assert_eq!(
            LayerPhase::from_status("Already exists"),
            LayerPhase::Cached
        );
        assert_eq!(
            LayerPhase::from_status("unknown status"),
            LayerPhase::Pending
        );
    }

    // ─── Manifest extraction tests ──────────────────────────────────────────

    #[test]
    fn test_manifest_info_extraction() {
        let manifest = ManifestV2 {
            media_type: None,
            layers: Some(vec![
                ManifestLayer {
                    size: Some(1000),
                    digest: Some("sha256:abcdef123456789000".to_string()),
                },
                ManifestLayer {
                    size: Some(2000),
                    digest: Some("sha256:fedcba987654321000".to_string()),
                },
            ]),
            manifests: None,
        };

        let info = extract_manifest_info(&manifest).unwrap();
        assert_eq!(info.total_bytes, 3000);
        assert_eq!(info.layer_sizes.len(), 2);
        assert_eq!(info.layer_sizes.get("abcdef123456"), Some(&1000));
        assert_eq!(info.layer_sizes.get("fedcba987654"), Some(&2000));
    }

    #[test]
    fn test_manifest_info_empty_layers() {
        let manifest = ManifestV2 {
            media_type: None,
            layers: Some(vec![]),
            manifests: None,
        };
        assert!(extract_manifest_info(&manifest).is_none());
    }

    #[test]
    fn test_manifest_info_no_layers() {
        let manifest = ManifestV2 {
            media_type: None,
            layers: None,
            manifests: None,
        };
        assert!(extract_manifest_info(&manifest).is_none());
    }

    #[test]
    fn test_digest_prefix() {
        assert_eq!(
            digest_prefix("sha256:abcdef1234567890abcdef"),
            "abcdef123456"
        );
        assert_eq!(digest_prefix("short"), "short");
        assert_eq!(digest_prefix("sha256:abc"), "abc");
    }

    // ─── Aggregation tests ──────────────────────────────────────────────────

    #[test]
    fn test_aggregate_phase_downloading() {
        let mut layers = HashMap::new();
        layers.insert(
            "a".into(),
            LayerTracker {
                phase: LayerPhase::Downloading,
                current_bytes: 50,
                total_bytes: 100,
                manifest_bytes: 100,
            },
        );
        layers.insert(
            "b".into(),
            LayerTracker {
                phase: LayerPhase::Downloading,
                current_bytes: 30,
                total_bytes: 200,
                manifest_bytes: 200,
            },
        );

        let (progress, has_data) = aggregate_progress(&layers, 300, 0, &None);
        assert!(has_data);
        assert_eq!(progress.phase, PullPhase::Downloading);
        assert_eq!(progress.layers_done, 0);
        assert_eq!(progress.layers_total, 2);
        assert_eq!(progress.downloaded_bytes, 80);
        assert_eq!(progress.total_bytes, 300);
    }

    #[test]
    fn test_aggregate_phase_extracting() {
        let mut layers = HashMap::new();
        layers.insert(
            "a".into(),
            LayerTracker {
                phase: LayerPhase::Extracting,
                current_bytes: 100,
                total_bytes: 100,
                manifest_bytes: 0,
            },
        );
        layers.insert(
            "b".into(),
            LayerTracker {
                phase: LayerPhase::Done,
                current_bytes: 200,
                total_bytes: 200,
                manifest_bytes: 0,
            },
        );

        let (progress, _) = aggregate_progress(&layers, 0, 0, &None);
        assert_eq!(progress.phase, PullPhase::Extracting);
        assert_eq!(progress.layers_done, 1);
        assert_eq!(progress.layers_total, 2);
    }

    #[test]
    fn test_aggregate_with_cached_layers() {
        let mut layers = HashMap::new();
        layers.insert(
            "a".into(),
            LayerTracker {
                phase: LayerPhase::Cached,
                current_bytes: 0,
                total_bytes: 0,
                manifest_bytes: 0, // already zeroed
            },
        );
        layers.insert(
            "b".into(),
            LayerTracker {
                phase: LayerPhase::Downloading,
                current_bytes: 50,
                total_bytes: 200,
                manifest_bytes: 200,
            },
        );

        // manifest_total=500, cached_subtract=300 (cached layer was 300 bytes)
        let (progress, _) = aggregate_progress(&layers, 500, 300, &None);
        assert_eq!(progress.total_bytes, 200); // 500 - 300
        assert_eq!(progress.layers_done, 1); // cached layer
        assert_eq!(progress.layers_total, 2);
    }

    #[test]
    fn test_with_phase_and_layers() {
        let p = ContainerPullProgress::new("test", "img")
            .with_progress(50, 100)
            .with_phase(PullPhase::Extracting)
            .with_layers(3, 5);
        assert_eq!(p.phase, PullPhase::Extracting);
        assert_eq!(p.layers_done, 3);
        assert_eq!(p.layers_total, 5);
        assert!(p.tracking_available);
    }

    // ─── Auth parser tests ──────────────────────────────────────────────────

    #[test]
    fn test_split_auth_params_standard() {
        let params = split_auth_params(
            r#"realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
        );
        assert_eq!(params.len(), 3);
        assert_eq!(
            params[0],
            ("realm", "https://auth.docker.io/token".to_string())
        );
        assert_eq!(params[1], ("service", "registry.docker.io".to_string()));
        assert_eq!(
            params[2],
            ("scope", "repository:library/nginx:pull".to_string())
        );
    }

    #[test]
    fn test_split_auth_params_unquoted() {
        let params = split_auth_params("realm=https://example.com,service=test");
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], ("realm", "https://example.com".to_string()));
        assert_eq!(params[1], ("service", "test".to_string()));
    }

    #[test]
    fn test_split_auth_params_empty() {
        let params = split_auth_params("");
        assert!(params.is_empty());
    }

    #[test]
    fn test_split_auth_params_extra_whitespace() {
        let params = split_auth_params(r#"  realm="val1" ,  service="val2"  "#);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], ("realm", "val1".to_string()));
        assert_eq!(params[1], ("service", "val2".to_string()));
    }

    #[test]
    fn test_split_auth_params_escaped_quotes() {
        let params = split_auth_params(r#"key="value with \"escaped\" quotes""#);
        assert_eq!(params.len(), 1);
        assert_eq!(
            params[0],
            ("key", "value with \"escaped\" quotes".to_string())
        );
    }

    #[test]
    fn test_split_auth_params_no_equals() {
        let params = split_auth_params("invalid-no-equals");
        assert!(params.is_empty());
    }

    // ─── Manifest list resolution tests ─────────────────────────────────────

    #[test]
    fn test_manifest_list_with_matching_platform() {
        let manifest = ManifestV2 {
            media_type: None,
            layers: None,
            manifests: Some(vec![
                ManifestListEntry {
                    digest: Some("sha256:arm64digest".to_string()),
                    platform: Some(ManifestPlatform {
                        architecture: Some("arm64".to_string()),
                        os: Some("linux".to_string()),
                    }),
                },
                ManifestListEntry {
                    digest: Some("sha256:amd64digest".to_string()),
                    platform: Some(ManifestPlatform {
                        architecture: Some("amd64".to_string()),
                        os: Some("linux".to_string()),
                    }),
                },
            ]),
        };

        // extract_manifest_info returns None for manifest lists (no layers)
        // This test validates the structure — the actual resolution
        // happens in fetch_manifest_total which calls fetch_manifest_by_digest
        assert!(extract_manifest_info(&manifest).is_none());
        assert!(manifest.manifests.is_some());
        let manifests = manifest.manifests.unwrap();
        let target = target_architecture();
        let entry = manifests
            .iter()
            .find(|m| {
                m.platform
                    .as_ref()
                    .map(|p| {
                        p.architecture.as_deref() == Some(target)
                            && p.os.as_deref() == Some("linux")
                    })
                    .unwrap_or(false)
            })
            .unwrap();
        assert!(entry.digest.is_some());
    }

    #[test]
    fn test_manifest_list_no_matching_platform() {
        let manifests = [ManifestListEntry {
            digest: Some("sha256:s390xdigest".to_string()),
            platform: Some(ManifestPlatform {
                architecture: Some("s390x".to_string()),
                os: Some("linux".to_string()),
            }),
        }];

        let target = target_architecture();
        let entry = manifests.iter().find(|m| {
            m.platform
                .as_ref()
                .map(|p| {
                    p.architecture.as_deref() == Some(target) && p.os.as_deref() == Some("linux")
                })
                .unwrap_or(false)
        });
        assert!(entry.is_none());
    }

    #[test]
    fn test_manifest_list_windows_platform_skipped() {
        let manifests = [ManifestListEntry {
            digest: Some("sha256:windowsdigest".to_string()),
            platform: Some(ManifestPlatform {
                architecture: Some("amd64".to_string()),
                os: Some("windows".to_string()),
            }),
        }];

        let target = target_architecture();
        let entry = manifests.iter().find(|m| {
            m.platform
                .as_ref()
                .map(|p| {
                    p.architecture.as_deref() == Some(target) && p.os.as_deref() == Some("linux")
                })
                .unwrap_or(false)
        });
        assert!(entry.is_none());
    }

    #[test]
    fn test_manifest_list_missing_platform_field() {
        let manifests = [ManifestListEntry {
            digest: Some("sha256:noplatform".to_string()),
            platform: None,
        }];

        let target = target_architecture();
        let entry = manifests.iter().find(|m| {
            m.platform
                .as_ref()
                .map(|p| {
                    p.architecture.as_deref() == Some(target) && p.os.as_deref() == Some("linux")
                })
                .unwrap_or(false)
        });
        assert!(entry.is_none());
    }

    // ─── Cache key tests ────────────────────────────────────────────────────

    #[test]
    fn test_image_ref_cache_key() {
        let img = ImageRef::parse("nginx:1.25");
        assert_eq!(img.cache_key(), "registry-1.docker.io/library/nginx/1.25");

        let img2 = ImageRef::parse("ghcr.io/owner/repo:v2");
        assert_eq!(img2.cache_key(), "ghcr.io/owner/repo/v2");
    }
}
