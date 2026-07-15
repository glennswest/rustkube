//! Leader election via `coordination.k8s.io` Lease objects.
//!
//! Same acquire/renew/expire shape as an etcd lease keepalive (as in fastetcd),
//! but at the Kubernetes API layer so it's a drop-in for upstream
//! kube-controller-manager: a single Lease object holds the current leader's
//! identity; the holder renews it before `leaseDurationSeconds` elapses, and any
//! candidate acquires it once it expires. Renewal uses the object's
//! resourceVersion for compare-and-swap, so two candidates can't both win.
//!
//! Only the elected leader runs the controllers.

use crate::runner::ApiClient;
use chrono::{SecondsFormat, Utc};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Upstream kube-controller-manager defaults.
const LEASE_DURATION_SECS: i64 = 15;
/// How often to attempt an acquire/renew.
const RETRY_PERIOD: Duration = Duration::from_secs(2);

/// Elects one holder of a Lease object and keeps it renewed.
pub struct LeaderElector {
    api: Arc<ApiClient>,
    name: String,
    namespace: String,
    identity: String,
    get_path: String,
    create_path: String,
}

impl LeaderElector {
    pub fn new(api: Arc<ApiClient>, name: &str, namespace: &str, identity: &str) -> Self {
        Self {
            api,
            get_path: format!(
                "/apis/coordination.k8s.io/v1/namespaces/{namespace}/leases/{name}"
            ),
            create_path: format!(
                "/apis/coordination.k8s.io/v1/namespaces/{namespace}/leases"
            ),
            name: name.to_string(),
            namespace: namespace.to_string(),
            identity: identity.to_string(),
        }
    }

    pub fn retry_period(&self) -> Duration {
        RETRY_PERIOD
    }

    /// Block until this instance holds the lease.
    pub async fn acquire(&self) {
        loop {
            if self.try_acquire_or_renew().await {
                return;
            }
            tokio::time::sleep(RETRY_PERIOD).await;
        }
    }

    /// Try to become or remain leader. Returns true if we hold the lease now.
    pub async fn try_acquire_or_renew(&self) -> bool {
        let now = Utc::now();
        let now_micro = now.to_rfc3339_opts(SecondsFormat::Micros, true);

        let resp = match self.api.get(&self.get_path).await {
            Ok(r) => r,
            Err(e) => {
                warn!("leaderelection: GET lease failed: {e}");
                return false;
            }
        };

        // No lease yet — create it and claim leadership.
        if resp.status().as_u16() == 404 {
            let body = serde_json::json!({
                "apiVersion": "coordination.k8s.io/v1",
                "kind": "Lease",
                "metadata": { "name": self.name, "namespace": self.namespace },
                "spec": {
                    "holderIdentity": self.identity,
                    "leaseDurationSeconds": LEASE_DURATION_SECS,
                    "acquireTime": now_micro,
                    "renewTime": now_micro,
                    "leaseTransitions": 0
                }
            });
            return match self.api.create(&self.create_path, &body).await {
                Ok(v) if v["kind"] == "Lease" => {
                    info!("leaderelection: created lease, leading as {}", self.identity);
                    true
                }
                // Lost the create race (409) or other error — a Status object.
                Ok(_) | Err(_) => false,
            };
        }

        let lease: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("leaderelection: parse lease failed: {e}");
                return false;
            }
        };

        let spec = &lease["spec"];
        let holder = spec["holderIdentity"].as_str().unwrap_or("");
        let renew_time = spec["renewTime"].as_str().unwrap_or("");
        let lease_dur = spec["leaseDurationSeconds"].as_i64().unwrap_or(LEASE_DURATION_SECS);
        let transitions = spec["leaseTransitions"].as_i64().unwrap_or(0);
        let acquire_time = spec["acquireTime"].as_str().unwrap_or(&now_micro).to_string();
        let rv = lease["metadata"]["resourceVersion"].as_str().unwrap_or("").to_string();

        let held_by_us = holder == self.identity;

        // Expired if renewTime + leaseDuration is in the past (or unparseable).
        let expired = match chrono::DateTime::parse_from_rfc3339(renew_time) {
            Ok(rt) => now.signed_duration_since(rt.with_timezone(&Utc)).num_seconds() > lease_dur,
            Err(_) => true,
        };

        // A valid lease held by someone else — we stay a follower.
        if !held_by_us && !expired {
            return false;
        }

        // Renew (ours) or acquire (expired): keep acquireTime/transitions when
        // renewing; take a fresh acquireTime and bump transitions on takeover.
        let (new_acquire, new_transitions) = if held_by_us {
            (acquire_time, transitions)
        } else {
            (now_micro.clone(), transitions + 1)
        };
        let body = serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": self.name,
                "namespace": self.namespace,
                "resourceVersion": rv
            },
            "spec": {
                "holderIdentity": self.identity,
                "leaseDurationSeconds": lease_dur,
                "acquireTime": new_acquire,
                "renewTime": now_micro,
                "leaseTransitions": new_transitions
            }
        });
        match self.api.update(&self.get_path, &body).await {
            // Success returns the updated Lease; a 409 (lost CAS) returns Status.
            Ok(v) if v["kind"] == "Lease" => {
                if !held_by_us {
                    info!("leaderelection: acquired lease as {}", self.identity);
                }
                true
            }
            Ok(_) | Err(_) => false,
        }
    }
}
