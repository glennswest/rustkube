//! Pod Eviction subresource (policy/v1) with PodDisruptionBudget gating (#7).
//!
//! `POST /api/v1/namespaces/{ns}/pods/{name}/eviction` deletes the pod, but only
//! if the PodDisruptionBudget selecting it permits one more disruption;
//! otherwise it returns `429 Too Many Requests`, exactly like upstream. This is
//! what makes `kubectl drain` safe — it cordons the node, then evicts each pod
//! through here, and a PDB that would be violated blocks the eviction.
//!
//! **The disruption is reserved before the pod is deleted** (review findings
//! 6d98e18d1259, b8d6824b0231; #360), as upstream's `checkAndDecrement`: one
//! compare-and-swap on the PDB in the datastore decrements
//! `status.disruptionsAllowed` and records the pod in `status.disruptedPods`.
//! Two evictions, on one apiserver or two, cannot both spend the last
//! disruption: the second's swap loses, re-reads 0, and is a 429. The PDB
//! controller counts a recorded pod as already gone until it is deleted or
//! two minutes pass. Each eviction used to count the healthy pods itself and
//! delete, so two at once both saw one disruption left and both deleted.

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
    // Body is a policy/v1 Eviction; only admission webhooks read it.
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let pod_key = ResourceStorage::namespaced_key("pods", &namespace, &name);
    let pod = state.storage.get(&pod_key).await?;
    // `pods/eviction` CREATE, as upstream admits it (#82).
    let mut eviction: Value = serde_json::from_slice(&body).unwrap_or_else(|_| {
        json!({"apiVersion": "policy/v1", "kind": "Eviction",
               "metadata": {"name": name, "namespace": namespace}})
    });
    crate::admission::admit(&state, crate::admission::Operation::Create, Some(&mut eviction), None).await?;
    let evicted = || {
        let ok = json!({
            "apiVersion": "policy/v1",
            "kind": "Eviction",
            "metadata": { "name": name, "namespace": namespace },
        });
        Ok((StatusCode::CREATED, Json(ok)).into_response())
    };

    // A pod already going, or not running, disrupts nothing (upstream's
    // canIgnorePDB).
    if can_ignore_pdb(&pod) {
        state.storage.delete(&pod_key, None).await?;
        return evicted();
    }
    let pdbs: Vec<Value> = state
        .storage
        .list(&ResourceStorage::namespace_prefix("poddisruptionbudgets", &namespace), 1000, None)
        .await?
        .0
        .into_iter()
        .filter(|pdb| selects(pdb, &pod["metadata"]["labels"]))
        .collect();
    let pdb = match pdbs.as_slice() {
        [] => {
            state.storage.delete(&pod_key, None).await?;
            return evicted();
        }
        [one] => one,
        _ => {
            return Err(ApiError::internal(
                "This pod has more than one PodDisruptionBudget, which the eviction subresource does not support.",
            ))
        }
    };
    let pdb_name = pdb["metadata"]["name"].as_str().unwrap_or("").to_string();
    let pdb_key = ResourceStorage::namespaced_key("poddisruptionbudgets", &namespace, &pdb_name);

    // A pod that is not Ready is no healthy replica to lose: under the default
    // `IfHealthyBudget` it goes without spending a disruption when the budget
    // is met, and `AlwaysAllow` lets it go regardless (upstream's
    // unhealthyPodEvictionPolicy).
    if !is_ready(&pod) {
        let always = pdb["spec"]["unhealthyPodEvictionPolicy"].as_str() == Some("AlwaysAllow");
        let st = &pdb["status"];
        let met = processed(pdb)
            && st["currentHealthy"].as_i64().unwrap_or(0) >= st["desiredHealthy"].as_i64().unwrap_or(0);
        if always || met {
            state.storage.delete(&pod_key, None).await?;
            return evicted();
        }
        return Err(budget_exhausted(&namespace, pdb));
    }

    reserve(&state, &pdb_key, &namespace, &name).await?;
    if let Err(e) = state.storage.delete(&pod_key, None).await {
        // The pod stays, so the disruption it reserved is given back; the PDB
        // controller would expire it in two minutes anyway.
        if let Err(release_err) = release(&state, &pdb_key, &name).await {
            tracing::warn!(pdb = %pdb_key, pod = %name, "eviction failed and its reservation was not released: {}", release_err.message);
        }
        return Err(e);
    }
    evicted()
}

/// Upstream's `MaxDisruptedPodSize`: past it, evictions are refused until the
/// controller confirms some.
const MAX_DISRUPTED_PODS: usize = 2000;

/// Spend one disruption of the budget at `pdb_key` on `pod`, atomically: the
/// check and the decrement are one compare-and-swap in the datastore, retried
/// on a lost swap against a fresh read (`guaranteed_update`), so concurrent
/// evictions — through any apiserver — each see the count the last one left.
async fn reserve(state: &AppState, pdb_key: &str, namespace: &str, pod: &str) -> Result<(), ApiError> {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    crate::handlers::resource::guaranteed_update(state, pdb_key, None, |mut pdb| {
        reserve_in(&mut pdb, namespace, pod, &now)?;
        Ok(pdb)
    })
    .await
    .map(|_| ())
}

/// The decision and the decrement, on one read of the PDB.
fn reserve_in(pdb: &mut Value, namespace: &str, pod: &str, now: &str) -> Result<(), ApiError> {
    if !processed(pdb) {
        let name = pdb["metadata"]["name"].as_str().unwrap_or("");
        return Err(too_many_requests(
            format!("Cannot evict pod as it would violate the pod's disruption budget ({namespace}/{name}): the disruption budget {name} is still being processed by the server"),
            None,
        ));
    }
    let allowed = pdb["status"]["disruptionsAllowed"].as_i64().unwrap_or(0);
    if allowed <= 0 {
        return Err(budget_exhausted(namespace, pdb));
    }
    let disrupted = pdb["status"]["disruptedPods"].as_object().map_or(0, |m| m.len());
    if disrupted >= MAX_DISRUPTED_PODS {
        return Err(ApiError::forbidden("DisruptedPods map too big - too many evictions not confirmed by PDB controller"));
    }
    pdb["status"]["disruptionsAllowed"] = json!(allowed - 1);
    if !pdb["status"]["disruptedPods"].is_object() {
        pdb["status"]["disruptedPods"] = json!({});
    }
    pdb["status"]["disruptedPods"][pod] = json!(now);
    Ok(())
}

/// Give back what `reserve` took for `pod`, if it is still recorded.
async fn release(state: &AppState, pdb_key: &str, pod: &str) -> Result<(), ApiError> {
    crate::handlers::resource::guaranteed_update(state, pdb_key, None, |mut pdb| {
        release_in(&mut pdb, pod);
        Ok(pdb)
    })
    .await
    .map(|_| ())
}

fn release_in(pdb: &mut Value, pod: &str) {
    let removed = pdb["status"]["disruptedPods"].as_object_mut().and_then(|m| m.remove(pod)).is_some();
    if removed {
        let allowed = pdb["status"]["disruptionsAllowed"].as_i64().unwrap_or(0);
        pdb["status"]["disruptionsAllowed"] = json!(allowed + 1);
    }
}

/// The controller has computed this generation's status: until it has, the
/// budget's numbers are not this spec's.
fn processed(pdb: &Value) -> bool {
    let observed = pdb["status"]["observedGeneration"].as_i64();
    let generation = pdb["metadata"]["generation"].as_i64().unwrap_or(1);
    observed.is_some_and(|o| o >= generation)
}

/// Terminating, finished or not yet running: evicting it disrupts nothing.
fn can_ignore_pdb(pod: &Value) -> bool {
    !pod["metadata"]["deletionTimestamp"].is_null()
        || matches!(pod["status"]["phase"].as_str(), Some("Succeeded" | "Failed" | "Pending"))
}

/// Does the PDB select the pod (`matchLabels` and `matchExpressions`)? A
/// selector-less PDB selects nothing.
fn selects(pdb: &Value, labels: &Value) -> bool {
    let selector = &pdb["spec"]["selector"];
    selector.is_object() && apimachinery::selector::matches(selector, labels)
}

/// The 429 kubectl drain and client-go's eviction retry against.
fn budget_exhausted(namespace: &str, pdb: &Value) -> ApiError {
    let name = pdb["metadata"]["name"].as_str().unwrap_or("?");
    let st = &pdb["status"];
    too_many_requests(
        format!("Cannot evict pod as it would violate the pod's disruption budget ({namespace}/{name}): disruptionsAllowed=0"),
        Some(json!({"causes": [{
            "reason": "DisruptionBudget",
            "message": format!(
                "The disruption budget {name} needs {} healthy pods and has {} currently",
                st["desiredHealthy"].as_i64().unwrap_or(0),
                st["currentHealthy"].as_i64().unwrap_or(0)
            )
        }]})),
    )
}

fn too_many_requests(message: String, details: Option<Value>) -> ApiError {
    ApiError {
        status: StatusCode::TOO_MANY_REQUESTS,
        reason: "TooManyRequests".into(),
        message,
        continue_token: None,
        details,
    }
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
    use crate::crd::CrdRegistry;
    use crate::test_store::MemStore;
    use std::sync::Arc;

    fn pdb(allowed: i64) -> Value {
        json!({"apiVersion": "policy/v1", "kind": "PodDisruptionBudget",
               "metadata": {"name": "web", "namespace": "d", "generation": 1},
               "spec": {"minAvailable": 1, "selector": {"matchLabels": {"app": "web"}}},
               "status": {"observedGeneration": 1, "disruptionsAllowed": allowed,
                          "currentHealthy": allowed + 1, "desiredHealthy": 1}})
    }

    #[test]
    fn a_reservation_decrements_and_records_and_a_release_gives_it_back() {
        let mut p = pdb(1);
        reserve_in(&mut p, "d", "web-1", "2026-11-20T00:00:00Z").unwrap();
        assert_eq!(p["status"]["disruptionsAllowed"], 0);
        assert_eq!(p["status"]["disruptedPods"]["web-1"], "2026-11-20T00:00:00Z");
        // The next one on this read is refused: the 429 drain retries.
        let e = reserve_in(&mut p, "d", "web-2", "2026-11-20T00:00:01Z").unwrap_err();
        assert_eq!((e.status, e.reason.as_str()), (StatusCode::TOO_MANY_REQUESTS, "TooManyRequests"));
        assert_eq!(e.details.as_ref().unwrap()["causes"][0]["reason"], "DisruptionBudget");
        release_in(&mut p, "web-1");
        assert_eq!(p["status"]["disruptionsAllowed"], 1);
        assert!(p["status"]["disruptedPods"].as_object().unwrap().is_empty());
        // Releasing what was never reserved changes nothing.
        release_in(&mut p, "web-9");
        assert_eq!(p["status"]["disruptionsAllowed"], 1);
    }

    #[test]
    fn an_unprocessed_budget_refuses() {
        let mut p = pdb(5);
        p["metadata"]["generation"] = json!(2);
        let e = reserve_in(&mut p, "d", "web-1", "t").unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(e.message.contains("still being processed"), "{}", e.message);
        let mut p = pdb(5);
        p["status"].as_object_mut().unwrap().remove("observedGeneration");
        assert!(reserve_in(&mut p, "d", "web-1", "t").is_err());
    }

    #[test]
    fn what_needs_no_budget_and_what_a_budget_selects() {
        assert!(can_ignore_pdb(&json!({"status": {"phase": "Pending"}})));
        assert!(can_ignore_pdb(&json!({"status": {"phase": "Succeeded"}})));
        assert!(can_ignore_pdb(&json!({"metadata": {"deletionTimestamp": "x"}, "status": {"phase": "Running"}})));
        assert!(!can_ignore_pdb(&json!({"status": {"phase": "Running"}})));
        let p = pdb(1);
        assert!(selects(&p, &json!({"app": "web", "x": "y"})));
        assert!(!selects(&p, &json!({"app": "db"})));
        let expr = json!({"spec": {"selector": {"matchExpressions": [{"key": "app", "operator": "In", "values": ["web"]}]}}});
        assert!(selects(&expr, &json!({"app": "web"})), "matchExpressions are honoured");
        assert!(!selects(&json!({"spec": {}}), &json!({"app": "web"})), "no selector selects nothing");
    }

    fn state() -> AppState {
        AppState {
            storage: Arc::new(ResourceStorage::new(Arc::new(MemStore::default()))),
            crd_registry: Arc::new(CrdRegistry::new()),
            service_cidr: "10.96.0.0/12".into(),
            admission: Default::default(),
            aggregator: Arc::new(crate::aggregation::Aggregator::new(None)),
            resource_metrics: Arc::new(crate::resource_metrics::ResourceMetrics::new("http", 9096, None, None).unwrap()),
        }
    }

    /// Review findings 6d98e18d1259 / b8d6824b0231: evictions racing for one
    /// budget. Every one of them saw the budget's disruptions before any
    /// deleted, and each used to delete; now exactly `allowed` get through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_evictions_spend_each_disruption_once() {
        for allowed in [1_i64, 3] {
            let state = state();
            let pods = 8;
            state.storage.create(&ResourceStorage::namespaced_key("poddisruptionbudgets", "d", "web"), pdb(allowed)).await.unwrap();
            for i in 0..pods {
                state.storage.create(&ResourceStorage::namespaced_key("pods", "d", &format!("web-{i}")), json!({
                    "metadata": {"name": format!("web-{i}"), "namespace": "d", "labels": {"app": "web"}},
                    "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}})).await.unwrap();
            }
            let tasks: Vec<_> = (0..pods)
                .map(|i| {
                    let state = state.clone();
                    tokio::spawn(async move {
                        create_eviction(State(state), Path(("d".to_string(), format!("web-{i}"))), axum::body::Bytes::new())
                            .await
                            .map(|r| r.status())
                            .unwrap_or_else(|e| e.status)
                    })
                })
                .collect();
            let mut codes = Vec::new();
            for t in tasks {
                codes.push(t.await.unwrap());
            }
            let evicted = codes.iter().filter(|c| **c == StatusCode::CREATED).count() as i64;
            let refused = codes.iter().filter(|c| **c == StatusCode::TOO_MANY_REQUESTS).count() as i64;
            assert_eq!((evicted, refused), (allowed, pods - allowed), "budget {allowed}: {codes:?}");
            let left = state.storage.list(&ResourceStorage::namespace_prefix("pods", "d"), 100, None).await.unwrap().0;
            assert_eq!(left.len() as i64, pods - allowed, "budget {allowed}");
            let p = state.storage.get(&ResourceStorage::namespaced_key("poddisruptionbudgets", "d", "web")).await.unwrap();
            assert_eq!(p["status"]["disruptionsAllowed"], 0);
            assert_eq!(p["status"]["disruptedPods"].as_object().unwrap().len() as i64, allowed);
        }
    }

    #[tokio::test]
    async fn a_pod_with_two_budgets_is_refused_and_an_unready_one_needs_none_spent() {
        let state = state();
        let key = |n: &str| ResourceStorage::namespaced_key("pods", "d", n);
        state.storage.create(&ResourceStorage::namespaced_key("poddisruptionbudgets", "d", "web"), pdb(0)).await.unwrap();
        // Not Ready, budget met (1 healthy of 1 desired): goes, spends nothing.
        state.storage.create(&key("sick"), json!({"metadata": {"name": "sick", "namespace": "d", "labels": {"app": "web"}},
            "status": {"phase": "Running"}})).await.unwrap();
        let r = create_eviction(State(state.clone()), Path(("d".into(), "sick".into())), axum::body::Bytes::new()).await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        let p = state.storage.get(&ResourceStorage::namespaced_key("poddisruptionbudgets", "d", "web")).await.unwrap();
        assert!(p["status"].get("disruptedPods").is_none());
        // Ready, no disruption left: 429.
        state.storage.create(&key("ok"), json!({"metadata": {"name": "ok", "namespace": "d", "labels": {"app": "web"}},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}})).await.unwrap();
        let e = create_eviction(State(state.clone()), Path(("d".into(), "ok".into())), axum::body::Bytes::new()).await.unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        // A second budget over the same pod: upstream's 500.
        let mut other = pdb(5);
        other["metadata"]["name"] = json!("web2");
        state.storage.create(&ResourceStorage::namespaced_key("poddisruptionbudgets", "d", "web2"), other).await.unwrap();
        let e = create_eviction(State(state.clone()), Path(("d".into(), "ok".into())), axum::body::Bytes::new()).await.unwrap_err();
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(e.message.contains("more than one PodDisruptionBudget"));
    }
}
