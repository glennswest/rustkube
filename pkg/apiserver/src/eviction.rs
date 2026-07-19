//! Pod Eviction subresource (policy/v1) with PodDisruptionBudget gating (#7).
//!
//! `POST /api/v1/namespaces/{ns}/pods/{name}/eviction` deletes the pod, but only
//! if every PodDisruptionBudget selecting it would still be satisfied afterward;
//! otherwise it returns `429 Too Many Requests`, exactly like upstream. This is
//! what makes `kubectl drain` safe — it cordons the node, then evicts each pod
//! through here, and a PDB that would be violated blocks the eviction.

use crate::error::ApiError;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

pub async fn create_eviction(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    // Body is a policy/v1 Eviction; we don't need its fields beyond the target.
    _body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let pod_key = ResourceStorage::namespaced_key("pods", &namespace, &name);
    let pod = state.storage.get(&pod_key).await?;
    let pod_labels = pod["metadata"]["labels"].clone();

    // Every PDB in the namespace whose selector matches this pod must permit one
    // more disruption.
    let pdbs = state
        .storage
        .list(
            &ResourceStorage::namespace_prefix("poddisruptionbudgets", &namespace),
            1000,
            None,
        )
        .await?
        .0;

    for pdb in &pdbs {
        if !selector_matches(&pdb["spec"]["selector"], &pod_labels) {
            continue;
        }
        let allowed = disruptions_allowed(pdb, &state, &namespace).await?;
        if allowed <= 0 {
            let pdb_name = pdb["metadata"]["name"].as_str().unwrap_or("?");
            // 429 with a Status carrying the disruptionbudget cause is what
            // kubectl drain / client-go eviction expect and will retry against.
            let status = json!({
                "apiVersion": "v1",
                "kind": "Status",
                "metadata": {},
                "status": "Failure",
                "message": format!(
                    "Cannot evict pod as it would violate the pod's disruption budget \
                     ({namespace}/{pdb_name}): disruptionsAllowed=0"
                ),
                "reason": "TooManyRequests",
                "details": {
                    "causes": [{
                        "reason": "DisruptionBudget",
                        "message": format!("The disruption budget {pdb_name} needs 0 more healthy pods")
                    }]
                },
                "code": 429,
            });
            return Ok((StatusCode::TOO_MANY_REQUESTS, Json(status)).into_response());
        }
    }

    // Allowed — delete the pod (a graceful delete; deletionTimestamp/GC handle
    // the rest). Return the Eviction acknowledgement.
    state.storage.delete(&pod_key, None).await?;
    let ok = json!({
        "apiVersion": "policy/v1",
        "kind": "Eviction",
        "metadata": { "name": name, "namespace": namespace },
    });
    Ok((StatusCode::CREATED, Json(ok)).into_response())
}

/// How many more voluntary disruptions the PDB permits right now.
/// `disruptionsAllowed = currentHealthy - desiredHealthy`.
async fn disruptions_allowed(
    pdb: &Value,
    state: &AppState,
    namespace: &str,
) -> Result<i64, ApiError> {
    let pods = state
        .storage
        .list(&ResourceStorage::namespace_prefix("pods", namespace), 5000, None)
        .await?
        .0;

    let matched: Vec<&Value> = pods
        .iter()
        .filter(|p| selector_matches(&pdb["spec"]["selector"], &p["metadata"]["labels"]))
        .collect();
    let total = matched.len() as i64;
    let healthy = matched.iter().filter(|p| is_ready(p)).count() as i64;

    let spec = &pdb["spec"];
    let desired = if let Some(min) = intstr_to_count(&spec["minAvailable"], total) {
        min
    } else if let Some(max_unavail) = intstr_to_count(&spec["maxUnavailable"], total) {
        total - max_unavail
    } else {
        // No constraint set → never blocks.
        0
    };
    Ok(healthy - desired)
}

/// Resolve an IntOrString PDB field to an absolute pod count. `%` is taken of
/// `total` (rounded up for minAvailable semantics).
fn intstr_to_count(v: &Value, total: i64) -> Option<i64> {
    if v.is_null() {
        return None;
    }
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        if let Some(pct) = s.strip_suffix('%') {
            if let Ok(p) = pct.parse::<f64>() {
                return Some(((p / 100.0) * total as f64).ceil() as i64);
            }
        }
        if let Ok(n) = s.parse::<i64>() {
            return Some(n);
        }
    }
    None
}

/// Minimal label-selector match (matchLabels only; empty selector matches all).
pub fn selector_matches(selector: &Value, labels: &Value) -> bool {
    let ml = match selector.get("matchLabels").and_then(Value::as_object) {
        Some(m) => m,
        None => return selector.is_object(), // {} selects everything
    };
    let pod_labels = labels.as_object();
    ml.iter().all(|(k, v)| {
        pod_labels
            .and_then(|pl| pl.get(k))
            .map(|pv| pv == v)
            .unwrap_or(false)
    })
}

fn is_ready(pod: &Value) -> bool {
    pod["status"]["conditions"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .any(|c| c["type"] == "Ready" && c["status"] == "True")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_matches_semantics() {
        let sel = json!({"matchLabels": {"app": "web"}});
        assert!(selector_matches(&sel, &json!({"app": "web", "x": "y"})));
        assert!(!selector_matches(&sel, &json!({"app": "db"})));
        assert!(!selector_matches(&sel, &json!(null)));
        // Empty selector matches everything.
        assert!(selector_matches(&json!({}), &json!({"app": "web"})));
    }

    #[test]
    fn intstr_absolute_and_percent() {
        assert_eq!(intstr_to_count(&json!(2), 10), Some(2));
        assert_eq!(intstr_to_count(&json!("3"), 10), Some(3));
        assert_eq!(intstr_to_count(&json!("50%"), 10), Some(5));
        // ceil for minAvailable
        assert_eq!(intstr_to_count(&json!("25%"), 10), Some(3));
        assert_eq!(intstr_to_count(&json!(null), 10), None);
    }
}
