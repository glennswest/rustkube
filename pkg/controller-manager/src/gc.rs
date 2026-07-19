//! Garbage collector — owner-reference cascading delete.
//!
//! A periodic reconcile: gather every live owner UID in the cluster, then delete
//! any child object whose controlling `ownerReference` points at a UID that no
//! longer exists. This is what makes `kubectl delete deployment` cascade to its
//! ReplicaSets and Pods (once each layer's owner disappears, the next is
//! collected). A first, background-propagation implementation — no finalizers /
//! foreground deletion yet.

use crate::runner::ApiClient;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Reconcile interval.
const GC_INTERVAL: Duration = Duration::from_secs(30);

/// Events older than this are reaped (upstream default ~1h).
const EVENT_TTL: chrono::Duration = chrono::Duration::hours(1);

/// Kinds whose UIDs can be owners (cluster-wide list paths).
const OWNER_LIST_PATHS: &[&str] = &[
    "/apis/apps/v1/deployments",
    "/apis/apps/v1/replicasets",
    "/apis/apps/v1/statefulsets",
    "/apis/apps/v1/daemonsets",
    "/apis/batch/v1/jobs",
    "/apis/batch/v1/cronjobs",
    "/api/v1/replicationcontrollers",
    "/api/v1/pods",
];

/// Child kinds we collect: (cluster-wide list path, plural for the delete path).
const CHILD_KINDS: &[(&str, &str)] = &[
    ("/api/v1/pods", "pods"),
    ("/apis/apps/v1/replicasets", "replicasets"),
    ("/apis/batch/v1/jobs", "jobs"),
];

pub struct GarbageCollector {
    api: Arc<ApiClient>,
}

impl GarbageCollector {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Starting garbage collector");
        loop {
            self.collect().await;
            tokio::time::sleep(GC_INTERVAL).await;
        }
    }

    async fn collect(&self) {
        // 1. Snapshot every live owner UID.
        let mut live: HashSet<String> = HashSet::new();
        for path in OWNER_LIST_PATHS {
            if let Ok(list) = self.api.list(path).await {
                if let Some(items) = list["items"].as_array() {
                    for it in items {
                        if let Some(uid) = it["metadata"]["uid"].as_str() {
                            live.insert(uid.to_string());
                        }
                    }
                }
            }
        }
        if live.is_empty() {
            return; // apiserver unreachable / nothing to key against
        }

        // 2. Delete children whose controlling owner UID is gone.
        for (list_path, plural) in CHILD_KINDS {
            let list = match self.api.list(list_path).await {
                Ok(l) => l,
                Err(_) => continue,
            };
            let Some(items) = list["items"].as_array() else {
                continue;
            };
            for it in items {
                // Skip objects already being deleted.
                if !it["metadata"]["deletionTimestamp"].is_null() {
                    continue;
                }
                let owners = match it["metadata"]["ownerReferences"].as_array() {
                    Some(o) if !o.is_empty() => o,
                    _ => continue, // no owners → not garbage
                };
                // Prefer the controlling owner; fall back to the first.
                let owner = owners
                    .iter()
                    .find(|o| o["controller"].as_bool() == Some(true))
                    .unwrap_or(&owners[0]);
                let owner_uid = owner["uid"].as_str().unwrap_or("");
                if owner_uid.is_empty() || live.contains(owner_uid) {
                    continue; // owner still exists
                }

                let ns = it["metadata"]["namespace"].as_str().unwrap_or("default");
                let name = it["metadata"]["name"].as_str().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let del = child_delete_path(plural, ns, name);
                match self.api.delete(&del).await {
                    Ok(_) => info!(
                        "gc: deleted {plural}/{ns}/{name} — owner {} ({owner_uid}) is gone",
                        owner["kind"].as_str().unwrap_or("?")
                    ),
                    Err(e) => warn!("gc: failed to delete {del}: {e}"),
                }
            }
        }

        // 3. Expire old Events (#15). Now that controllers emit Events, they must
        //    be reaped or they accumulate forever — upstream drops them after ~1h.
        self.expire_events().await;
    }

    /// Delete Events whose lastTimestamp is older than `EVENT_TTL`.
    async fn expire_events(&self) {
        let list = match self.api.list("/api/v1/events").await {
            Ok(l) => l,
            Err(_) => return,
        };
        let Some(items) = list["items"].as_array() else {
            return;
        };
        let now = chrono::Utc::now();
        for ev in items {
            let ts = ev["lastTimestamp"]
                .as_str()
                .or_else(|| ev["eventTime"].as_str())
                .or_else(|| ev["metadata"]["creationTimestamp"].as_str());
            let stale = ts
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)) > EVENT_TTL)
                .unwrap_or(false);
            if !stale {
                continue;
            }
            let ns = ev["metadata"]["namespace"].as_str().unwrap_or("default");
            let name = ev["metadata"]["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let _ = self
                .api
                .delete(&format!("/api/v1/namespaces/{ns}/events/{name}"))
                .await;
        }
    }
}

fn child_delete_path(plural: &str, ns: &str, name: &str) -> String {
    match plural {
        "pods" => format!("/api/v1/namespaces/{ns}/pods/{name}"),
        "replicasets" => format!("/apis/apps/v1/namespaces/{ns}/replicasets/{name}"),
        "jobs" => format!("/apis/batch/v1/namespaces/{ns}/jobs/{name}"),
        _ => format!("/api/v1/namespaces/{ns}/{plural}/{name}"),
    }
}
