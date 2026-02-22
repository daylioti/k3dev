//! Pod startup timeline — fetches pod conditions + events and computes phase breakdown

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::{
    api::{Api, ListParams},
    Client,
};

/// A single phase in the pod startup timeline
#[derive(Debug, Clone)]
pub struct TimelinePhase {
    pub name: String,
    #[allow(dead_code)]
    pub start: DateTime<Utc>,
    #[allow(dead_code)]
    pub end: DateTime<Utc>,
    pub duration: chrono::Duration,
}

/// A single K8s event associated with the pod
#[derive(Debug, Clone)]
pub struct TimelineEvent {
    pub timestamp: DateTime<Utc>,
    pub reason: String,
    pub message: String,
}

/// Complete pod startup timeline
#[derive(Debug, Clone)]
pub struct PodTimeline {
    pub pod_name: String,
    #[allow(dead_code)]
    pub namespace: String,
    pub total_duration: Option<chrono::Duration>,
    pub phases: Vec<TimelinePhase>,
    pub events: Vec<TimelineEvent>,
    pub is_ready: bool,
    pub note: Option<String>,
}

/// Fetch pod conditions and events, compute startup timeline phases
pub async fn get_pod_timeline(
    client: &Client,
    namespace: &str,
    pod_name: &str,
) -> Result<PodTimeline> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let pod = pods
        .get(pod_name)
        .await
        .context("Failed to fetch pod")?;

    let events_api: Api<Event> = Api::namespaced(client.clone(), namespace);
    let lp = ListParams::default().fields(&format!(
        "involvedObject.name={},involvedObject.kind=Pod",
        pod_name
    ));
    let event_list = events_api
        .list(&lp)
        .await
        .context("Failed to fetch events")?;

    // Extract timestamps from pod metadata and status
    let creation_ts = pod
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0);

    let status = pod.status.as_ref();

    let start_time = status.and_then(|s| s.start_time.as_ref()).map(|t| t.0);

    // Extract condition timestamps
    let conditions = status
        .and_then(|s| s.conditions.as_ref())
        .cloned()
        .unwrap_or_default();

    let scheduled_ts = find_condition_time(&conditions, "PodScheduled");
    let initialized_ts = find_condition_time(&conditions, "Initialized");
    let containers_ready_ts = find_condition_time(&conditions, "ContainersReady");
    let ready_ts = find_condition_time(&conditions, "Ready");

    let is_ready = conditions
        .iter()
        .any(|c| c.type_ == "Ready" && c.status == "True");

    // Extract container start time from container_statuses
    let container_start_ts = status
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|statuses| {
            statuses.iter().filter_map(|cs| {
                cs.state.as_ref()?.running.as_ref()?.started_at.as_ref().map(|t| t.0)
            }).min()
        });

    // Extract init container finish time
    let init_finished_ts = status
        .and_then(|s| s.init_container_statuses.as_ref())
        .and_then(|statuses| {
            statuses.iter().filter_map(|cs| {
                cs.state.as_ref()?.terminated.as_ref()?.finished_at.as_ref().map(|t| t.0)
            }).max()
        });

    let has_init_containers = status
        .and_then(|s| s.init_container_statuses.as_ref())
        .is_some_and(|s| !s.is_empty());

    // Find "Pulled" event timestamp (end of image pull)
    let pulled_event_ts = event_list
        .items
        .iter()
        .filter(|e| {
            e.reason.as_deref() == Some("Pulled")
        })
        .filter_map(|e| {
            e.last_timestamp.as_ref().map(|t| t.0)
                .or_else(|| e.event_time.as_ref().map(|t| t.0))
        })
        .max();

    // Build phases by chaining timestamps
    let mut phases = Vec::new();

    // Phase 1: Scheduling (creation → PodScheduled or start_time)
    let scheduling_start = creation_ts;
    let scheduling_end = scheduled_ts.or(start_time);
    if let (Some(start), Some(end)) = (scheduling_start, scheduling_end) {
        if end >= start {
            phases.push(TimelinePhase {
                name: "Scheduling".to_string(),
                start,
                end,
                duration: end - start,
            });
        }
    }

    // Phase 2: Image Pull (end of scheduling → Pulled event or Initialized)
    let pull_start = scheduling_end.or(creation_ts);
    let pull_end = pulled_event_ts.or(if has_init_containers {
        None
    } else {
        initialized_ts
    });
    if let (Some(start), Some(end)) = (pull_start, pull_end) {
        if end > start {
            phases.push(TimelinePhase {
                name: "Image Pull".to_string(),
                start,
                end,
                duration: end - start,
            });
        }
    }

    // Phase 3: Init Containers (only if init containers exist)
    if has_init_containers {
        let init_start = pull_end.or(scheduling_end).or(creation_ts);
        let init_end = init_finished_ts.or(initialized_ts);
        if let (Some(start), Some(end)) = (init_start, init_end) {
            if end > start {
                phases.push(TimelinePhase {
                    name: "Init Containers".to_string(),
                    start,
                    end,
                    duration: end - start,
                });
            }
        }
    }

    // Phase 4: Container Start (end of init → ContainersReady)
    let container_start_phase_start = if has_init_containers {
        init_finished_ts.or(initialized_ts)
    } else {
        pull_end.or(scheduling_end)
    }
    .or(creation_ts);
    let container_start_phase_end = container_start_ts.or(containers_ready_ts);
    if let (Some(start), Some(end)) = (container_start_phase_start, container_start_phase_end) {
        if end >= start {
            phases.push(TimelinePhase {
                name: "Container Start".to_string(),
                start,
                end,
                duration: end - start,
            });
        }
    }

    // Phase 5: Readiness (ContainersReady or container start → Ready)
    let readiness_start = containers_ready_ts.or(container_start_ts).or(container_start_phase_start);
    if let (Some(start), Some(end)) = (readiness_start, ready_ts) {
        if end > start {
            phases.push(TimelinePhase {
                name: "Readiness".to_string(),
                start,
                end,
                duration: end - start,
            });
        }
    }

    // Compute total duration
    let total_duration = match (creation_ts, ready_ts) {
        (Some(start), Some(end)) => Some(end - start),
        (Some(start), None) => Some(Utc::now() - start),
        _ => None,
    };

    // Build note
    let note = if !is_ready {
        Some("Pod not yet ready".to_string())
    } else {
        None
    };

    // Build events list
    let mut events: Vec<TimelineEvent> = event_list
        .items
        .iter()
        .filter_map(|e| {
            let timestamp = e
                .last_timestamp
                .as_ref()
                .map(|t| t.0)
                .or_else(|| e.event_time.as_ref().map(|t| t.0))?;
            Some(TimelineEvent {
                timestamp,
                reason: e.reason.clone().unwrap_or_default(),
                message: e.message.clone().unwrap_or_default(),
            })
        })
        .collect();

    events.sort_by_key(|e| e.timestamp);

    Ok(PodTimeline {
        pod_name: pod_name.to_string(),
        namespace: namespace.to_string(),
        total_duration,
        phases,
        events,
        is_ready,
        note,
    })
}

/// Find the last_transition_time for a condition type where status is "True"
fn find_condition_time(
    conditions: &[k8s_openapi::api::core::v1::PodCondition],
    condition_type: &str,
) -> Option<DateTime<Utc>> {
    conditions
        .iter()
        .find(|c| c.type_ == condition_type && c.status == "True")
        .and_then(|c| c.last_transition_time.as_ref())
        .map(|t| t.0)
}
