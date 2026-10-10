//! Node lifecycle controller.
//!
//! Monitors node heartbeats via Lease objects in kube-node-lease namespace.
//! Marks nodes as NotReady when lease expires, and taints them for eviction.

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::Index;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

const NODE_MONITOR_GRACE_PERIOD: Duration = Duration::from_secs(40);

pub struct NodeLifecycleController {
    api: Arc<ApiClient>,
}

impl NodeLifecycleController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile_node(
        &self,
        node: &Value,
        leases: &[Value],
        pods: &[Value],
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let node_name = node["metadata"]["name"].as_str().unwrap_or("");
        if node_name.is_empty() {
            return Ok(());
        }

        // Find the corresponding lease
        let lease = leases
            .iter()
            .find(|l| l["metadata"]["name"].as_str() == Some(node_name));

        let is_healthy = match lease {
            Some(l) => {
                // Check renewTime
                if let Some(renew_time) = l["spec"]["renewTime"].as_str() {
                    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(renew_time) {
                        apimachinery::reactor::requeue_at_time(
                            t.with_timezone(&chrono::Utc)
                                + chrono::Duration::seconds(
                                    NODE_MONITOR_GRACE_PERIOD.as_secs() as i64
                                ),
                        );
                        let elapsed = now.signed_duration_since(t);
                        elapsed.num_seconds() < NODE_MONITOR_GRACE_PERIOD.as_secs() as i64
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            None => {
                // No lease found — check if node was just created
                // Give new nodes time to register their first lease
                if let Some(created) = node["metadata"]["creationTimestamp"].as_str() {
                    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(created) {
                        apimachinery::reactor::requeue_at_time(
                            t.with_timezone(&chrono::Utc)
                                + chrono::Duration::seconds(
                                    NODE_MONITOR_GRACE_PERIOD.as_secs() as i64
                                ),
                        );
                        let elapsed = now.signed_duration_since(t);
                        elapsed.num_seconds() < NODE_MONITOR_GRACE_PERIOD.as_secs() as i64
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        };

        // Check current Ready condition
        let conditions = node["status"]["conditions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let current_ready = conditions
            .iter()
            .any(|c| c["type"].as_str() == Some("Ready") && c["status"].as_str() == Some("True"));

        if !is_healthy && current_ready {
            // Node went unhealthy — mark NotReady
            info!("Node {node_name} is not responding, marking NotReady");
            let mut updated = node.clone();
            update_condition(
                &mut updated,
                "Ready",
                "False",
                "NodeStatusUnknown",
                "Kubelet stopped posting node status",
            );
            // Both effects, as upstream does. NoSchedule keeps new work
            // off; NoExecute is what moves work that is already there, and
            // without it a node can be unreachable for an hour with its
            // pods still nominally running on it.
            let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            apimachinery::taint::add_taint(
                &mut updated,
                "node.kubernetes.io/not-ready",
                "NoSchedule",
                &stamp,
            );
            apimachinery::taint::add_taint(
                &mut updated,
                "node.kubernetes.io/not-ready",
                "NoExecute",
                &stamp,
            );

            let _ = self
                .api
                .update(&format!("/api/v1/nodes/{node_name}"), &updated)
                .await;
        }

        // **And take them off again.** A taint added when a node goes bad
        // and never removed when it recovers leaves a healthy node
        // permanently unschedulable — and the reason is in `spec` while
        // everyone reading a node looks at `status`, so it presents as
        // "the scheduler is ignoring my node".
        if is_healthy {
            let mut updated = node.clone();
            let mut changed = false;
            for effect in ["NoSchedule", "NoExecute"] {
                changed |= apimachinery::taint::remove_taint(
                    &mut updated,
                    "node.kubernetes.io/not-ready",
                    effect,
                );
            }
            if changed {
                info!("Node {node_name} is responding again, clearing not-ready taints");
                let _ = self
                    .api
                    .update(&format!("/api/v1/nodes/{node_name}"), &updated)
                    .await;
            }
        }

        // NoExecute means "get off", so something has to do the moving.
        if let Err(e) = self.evict_for_taints(node, node_name, pods).await {
            debug!("taint eviction on {node_name}: {e}");
        }
        Ok(())
    }
}

fn update_condition(node: &mut Value, cond_type: &str, status: &str, reason: &str, message: &str) {
    let conditions = node["status"]["conditions"].as_array_mut();

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let new_condition = json!({
        "type": cond_type,
        "status": status,
        "reason": reason,
        "message": message,
        "lastHeartbeatTime": now,
        "lastTransitionTime": now
    });

    match conditions {
        Some(conds) => {
            if let Some(existing) = conds
                .iter_mut()
                .find(|c| c["type"].as_str() == Some(cond_type))
            {
                *existing = new_condition;
            } else {
                conds.push(new_condition);
            }
        }
        None => {
            node["status"]["conditions"] = json!([new_condition]);
        }
    }
}

impl NodeLifecycleController {
    /// Evict pods that do not tolerate this node's `NoExecute` taints.
    ///
    /// Without this a `NoExecute` taint is a `NoSchedule` taint with a
    /// different spelling: it keeps new pods away and leaves the existing ones
    /// exactly where they were, which is the opposite of what it means.
    ///
    /// `tolerationSeconds` is respected rather than rounded away — a pod that
    /// tolerates `not-ready:NoExecute` for 300s is saying "give the node five
    /// minutes to come back before moving me", and evicting at once turns a
    /// blip into an outage. Pods that are not yet due are simply left; the
    /// next pass reconsiders them, which is what makes the deadline work
    /// without a timer.
    async fn evict_for_taints(
        &self,
        node: &Value,
        node_name: &str,
        pods: &[Value],
    ) -> anyhow::Result<()> {
        let taints: Vec<Value> = node["spec"]["taints"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|t| t["effect"].as_str() == Some("NoExecute"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if taints.is_empty() {
            return Ok(());
        }

        let now = chrono::Utc::now();
        let age = |t: &Value| -> i64 {
            t["timeAdded"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|added| now.signed_duration_since(added).num_seconds())
                // No timeAdded means we cannot tell how long it has been
                // there. Treating that as "just now" is the safe reading: it
                // delays an eviction rather than bringing one forward.
                .unwrap_or(0)
        };

        for pod in pods {
            if pod["spec"]["nodeName"].as_str() != Some(node_name) {
                continue;
            }
            // A pod already going away is not evicted again.
            if !pod["metadata"]["deletionTimestamp"].is_null() {
                continue;
            }
            let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
            if phase == "Succeeded" || phase == "Failed" {
                continue;
            }
            if let apimachinery::taint::Verdict::EvictIn(seconds, _) =
                apimachinery::taint::verdict_for(&pod, &taints, age)
            {
                apimachinery::reactor::requeue_after(Duration::from_secs(seconds.max(1) as u64));
            }
            if let apimachinery::taint::Verdict::Evict(key) =
                apimachinery::taint::verdict_for(&pod, &taints, age)
            {
                let ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
                let name = pod["metadata"]["name"].as_str().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                info!("Evicting {ns}/{name} from {node_name}: does not tolerate {key}:NoExecute");
                let _ = self
                    .api
                    .delete_observed(&format!("/api/v1/namespaces/{ns}/pods/{name}"), pod)
                    .await;
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Controller for NodeLifecycleController {
    fn name(&self) -> &'static str {
        "node"
    }
    fn primary(&self) -> &'static str {
        "/api/v1/nodes"
    }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                path: "/apis/coordination.k8s.io/v1/leases".into(),
                route: Arc::new(|delta, primary| {
                    delta
                        .old
                        .iter()
                        .chain(delta.new.iter())
                        .filter(|l| l["metadata"]["namespace"] == "kube-node-lease")
                        .flat_map(|l| {
                            owned::keys_at(
                                primary,
                                Index::Name(
                                    "".into(),
                                    l["metadata"]["name"].as_str().unwrap_or("").into(),
                                ),
                            )
                        })
                        .collect()
                }),
            },
            Dependency {
                path: "/api/v1/pods".into(),
                route: Arc::new(|delta, primary| {
                    delta
                        .affected
                        .iter()
                        .flat_map(|i| match i {
                            Index::Node(name) => {
                                owned::keys_at(primary, Index::Name("".into(), name.clone()))
                            }
                            _ => Vec::new(),
                        })
                        .collect()
                }),
            },
        ]
    }
    async fn reconcile(&self, node: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        let name = node["metadata"]["name"].as_str().unwrap_or("");
        let leases = deps
            .feed(0)
            .select(&Index::Name("kube-node-lease".into(), name.into()))?;
        let pods = deps.feed(1).select(&Index::Node(name.into()))?;
        self.reconcile_node(node, &leases, &pods).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::{Path, Query, State},
        response::{IntoResponse, Response},
        routing::{delete, get},
        Json, Router,
    };
    use futures::StreamExt;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// What the fake apiserver was asked to write.
    #[derive(Default)]
    struct Seen {
        node_puts: Vec<Value>,
        pod_deletes: Vec<String>,
    }
    type Shared = Arc<Mutex<Seen>>;

    fn ago(secs: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    }

    fn list_or_watch(query: &HashMap<String, String>, items: Value) -> Response {
        if query.get("watch").is_some_and(|v| v == "true") {
            let body = Body::from_stream(
                futures::stream::once(async {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                        b"{\"type\":\"BOOKMARK\",\"object\":{\"metadata\":{\"resourceVersion\":\"1\"}}}\n",
                    ))
                })
                .chain(futures::stream::pending()),
            );
            Response::new(body)
        } else {
            Json(json!({"metadata": {"resourceVersion": "1"}, "items": items})).into_response()
        }
    }

    const NINE_DAYS: i64 = 9 * 86_400;

    fn node(name: &str, created_ago: i64, heartbeat_ago: i64) -> Value {
        json!({"apiVersion": "v1", "kind": "Node",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "resourceVersion": "1", "creationTimestamp": ago(created_ago)},
            "spec": {},
            "status": {"conditions": [{"type": "Ready", "status": "True", "reason": "KubeletReady",
                "lastHeartbeatTime": ago(heartbeat_ago), "lastTransitionTime": ago(created_ago)}]}})
    }

    async fn nodes(Query(q): Query<HashMap<String, String>>) -> Response {
        list_or_watch(&q, json!([node("old", NINE_DAYS, NINE_DAYS - 1_560), node("live", 600, 5)]))
    }
    async fn leases(Query(q): Query<HashMap<String, String>>) -> Response {
        let lease = |name: &str, renew: i64| json!({"apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
            "metadata": {"name": name, "namespace": "kube-node-lease", "uid": format!("l-{name}"), "resourceVersion": "1"},
            "spec": {"holderIdentity": name, "leaseDurationSeconds": 40, "renewTime": ago(renew)}});
        list_or_watch(&q, json!([lease("old", NINE_DAYS - 1_560), lease("live", 3)]))
    }
    async fn pods(Query(q): Query<HashMap<String, String>>) -> Response {
        let pod = |name: &str, node: &str| json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "kube-system", "uid": format!("p-{name}"), "resourceVersion": "1"},
            "spec": {"nodeName": node, "containers": [{"name": "c", "image": "i"}]},
            "status": {"phase": "Running"}});
        list_or_watch(&q, json!([pod("coredns-old", "old"), pod("coredns-live", "live")]))
    }
    async fn put_node(State(seen): State<Shared>, Path(name): Path<String>, Json(mut body): Json<Value>) -> Response {
        body["metadata"]["resourceVersion"] = json!("2");
        let _ = name;
        seen.lock().unwrap().node_puts.push(body.clone());
        Json(body).into_response()
    }
    async fn delete_pod(State(seen): State<Shared>, Path((ns, name)): Path<(String, String)>) -> Response {
        seen.lock().unwrap().pod_deletes.push(format!("{ns}/{name}"));
        Json(json!({"kind": "Status", "status": "Success"})).into_response()
    }

    /// #267: a Node dead for nine days when the controller starts — a stale
    /// Lease, Ready=True — is judged on the first pass: Ready Unknown,
    /// unreachable taints, and its pods (no toleration) evicted. A live Node
    /// beside it is left alone.
    #[tokio::test]
    async fn a_node_dead_before_the_controller_started_is_marked_and_drained() {
        let seen: Shared = Arc::default();
        let app = Router::new()
            .route("/api/v1/nodes", get(nodes))
            .route("/api/v1/nodes/{name}", axum::routing::put(put_node))
            .route("/apis/coordination.k8s.io/v1/leases", get(leases))
            .route("/api/v1/pods", get(pods))
            .route("/api/v1/namespaces/{ns}/pods/{name}", delete(delete_pod))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = Arc::new(ApiClient::new(&format!("http://{addr}")));
        let worker = tokio::spawn(async move { NodeLifecycleController::new(api).run().await });

        let marked = |s: &Seen| {
            s.node_puts.iter().any(|n| {
                n["metadata"]["name"] == "old"
                    && n["status"]["conditions"].as_array().is_some_and(|c| {
                        c.iter().any(|c| c["type"] == "Ready" && c["status"] == "Unknown")
                    })
                    && n["spec"]["taints"].as_array().is_some_and(|t| {
                        t.iter().any(|t| t["key"] == "node.kubernetes.io/unreachable" && t["effect"] == "NoExecute")
                    })
            })
        };
        let drained = |s: &Seen| s.pod_deletes.iter().any(|p| p == "kube-system/coredns-old");
        let ok = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                {
                    let s = seen.lock().unwrap();
                    if marked(&s) && drained(&s) {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok();
        let s = seen.lock().unwrap();
        assert!(ok, "not marked and drained in 5 s: puts {:#?}, deletes {:?}", s.node_puts, s.pod_deletes);
        assert!(
            !s.node_puts.iter().any(|n| n["metadata"]["name"] == "live"),
            "the live node was written: {:#?}",
            s.node_puts
        );
        assert!(!s.pod_deletes.iter().any(|p| p.ends_with("coredns-live")), "{:?}", s.pod_deletes);
        drop(s);
        worker.abort();
        server.abort();
    }
}
