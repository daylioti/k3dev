mod client;
mod executor;
pub mod shell_session;
pub mod timeline;

pub use client::{K8sClient, PendingPodInfo, PvcInfo};
pub use executor::PodExecutor;
pub use shell_session::ShellSessionHandle;
pub use timeline::{get_pod_timeline, PodTimeline};

/// Convert a jiff::Timestamp (from k8s-openapi 0.27+) to chrono::DateTime<Utc>
pub(crate) fn jiff_to_chrono(ts: k8s_openapi::jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32)
        .unwrap_or_default()
}
