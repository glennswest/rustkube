//! Node lifecycle controller.
//!
//! Monitors node heartbeats via Lease objects in kube-node-lease namespace.
//! A node whose Lease is stale or missing past the 40 s grace period — however
//! long ago, including before this controller started (#267) — gets every
//! condition `Unknown` and the `unreachable` NoSchedule/NoExecute taints, as
//! upstream; its pods are evicted once their toleration runs out (300 s by
//! DefaultTolerationSeconds). A fresh heartbeat with Ready=False gets
//! `not-ready` instead; Ready=True clears both.

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
                // No Lease: judged by the newer of the node's creation (time
                // for its first Lease) and its Ready heartbeat (a kubelet
                // that posts status but no Lease), as upstream falls back to
                // the status heartbeat.
                let heartbeat = node["status"]["conditions"]
                    .as_array()
                    .and_then(|c| c.iter().find(|c| c["type"] == "Ready"))
                    .and_then(|c| c["lastHeartbeatTime"].as_str());
                let last = [node["metadata"]["creationTimestamp"].as_str(), heartbeat]
                    .into_iter()
                    .flatten()
                    .filter_map(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .max();
                match last {
                    Some(t) => {
                        apimachinery::reactor::requeue_at_time(
                            t + chrono::Duration::seconds(NODE_MONITOR_GRACE_PERIOD.as_secs() as i64),
                        );
                        now.signed_duration_since(t).num_seconds() < NODE_MONITOR_GRACE_PERIOD.as_secs() as i64
                    }
                    None => false,
                }
            }
        };

        // What the node itself last said, and what it should carry now —
        // upstream's node lifecycle controller (#267):
        // - heartbeat lost (stale or missing Lease): every condition
        //   `Unknown` ("Kubelet stopped posting node status."), taints
        //   `unreachable` NoSchedule + NoExecute;
        // - heartbeat fresh, kubelet says Ready=False: `not-ready` taints;
        // - heartbeat fresh and Ready=True: neither taint.
        // A node already `Unknown` keeps its transition time, and
        // `lastHeartbeatTime` stays the kubelet's: it is when the node was
        // last heard from, which is what anyone looking at a dead node needs.
        let ready = node["status"]["conditions"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["type"] == "Ready"))
            .and_then(|c| c["status"].as_str())
            .unwrap_or("Unknown")
            .to_string();
        let mut updated = node.clone();
        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut changed = false;
        let (want, drop): (Option<&str>, Option<&str>) = if !is_healthy {
            if ready != "Unknown" || !has_ready(node) {
                info!("Node {node_name} stopped posting status (Ready was {ready}), marking it Unknown");
                mark_unknown(&mut updated, &stamp);
                changed = true;
            }
            (Some(UNREACHABLE), Some(NOT_READY))
        } else if ready == "False" {
            (Some(NOT_READY), Some(UNREACHABLE))
        } else {
            // Ready=True: both taints come off below. Still Unknown with a
            // fresh heartbeat: the kubelet posts its status next.
            (None, None)
        };
        for effect in ["NoSchedule", "NoExecute"] {
            if let Some(key) = want {
                changed |= apimachinery::taint::add_taint(&mut updated, key, effect, &stamp);
            }
            if let Some(key) = drop {
                changed |= apimachinery::taint::remove_taint(&mut updated, key, effect);
            }
            // Healthy and Ready: take both off. A taint left on a node that
            // recovered keeps it unschedulable — and the reason is in `spec`
            // while everyone reading a node looks at `status`.
            if is_healthy && ready == "True" {
                for key in [NOT_READY, UNREACHABLE] {
                    changed |= apimachinery::taint::remove_taint(&mut updated, key, effect);
                }
            }
        }
        if changed {
            // An error goes back to the worker, which retries with backoff and
            // logs it — a node that cannot be marked says why (#267).
            self.api
                .update(&format!("/api/v1/nodes/{node_name}"), &updated)
                .await
                .map_err(|e| anyhow::anyhow!("marking node {node_name}: {e:#}"))?;
        }

        // NoExecute means "get off", so something has to do the moving.
        if let Err(e) = self.evict_for_taints(&updated, node_name, pods).await {
            debug!("taint eviction on {node_name}: {e}");
        }
        Ok(())
    }
}

const UNREACHABLE: &str = "node.kubernetes.io/unreachable";
const NOT_READY: &str = "node.kubernetes.io/not-ready";

fn has_ready(node: &Value) -> bool {
    node["status"]["conditions"].as_array().is_some_and(|c| c.iter().any(|c| c["type"] == "Ready"))
}

/// Every condition `Unknown`, as upstream's controller writes for a node that
/// stopped posting status; a Ready condition is added if there is none.
/// `lastHeartbeatTime` is left as the kubelet last wrote it.
fn mark_unknown(node: &mut Value, now: &str) {
    if !node["status"].is_object() {
        node["status"] = json!({});
    }
    if !has_ready(node) {
        let conds = node["status"]["conditions"].as_array().cloned().unwrap_or_default();
        let mut conds = conds;
        conds.push(json!({"type": "Ready"}));
        node["status"]["conditions"] = json!(conds);
    }
    if let Some(conds) = node["status"]["conditions"].as_array_mut() {
        for c in conds.iter_mut() {
            if c["status"] != "Unknown" {
                c["status"] = json!("Unknown");
                c["lastTransitionTime"] = json!(now);
            }
            c["reason"] = json!("NodeStatusUnknown");
            c["message"] = json!("Kubelet stopped posting node status.");
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

    #[test]
    fn unknown_keeps_the_last_heartbeat() {
        let mut n = json!({"status": {"conditions": [
            {"type": "Ready", "status": "True", "lastHeartbeatTime": "2026-10-01T19:35:00Z", "lastTransitionTime": "t0"},
            {"type": "MemoryPressure", "status": "False", "lastHeartbeatTime": "2026-10-01T19:35:00Z", "lastTransitionTime": "t0"}]}});
        mark_unknown(&mut n, "now");
        for c in n["status"]["conditions"].as_array().unwrap() {
            assert_eq!(c["status"], "Unknown");
            assert_eq!(c["reason"], "NodeStatusUnknown");
            assert_eq!(c["lastHeartbeatTime"], "2026-10-01T19:35:00Z", "{c}");
            assert_eq!(c["lastTransitionTime"], "now");
        }
        // Already Unknown: the transition time stays.
        mark_unknown(&mut n, "later");
        assert_eq!(n["status"]["conditions"][0]["lastTransitionTime"], "now");
        // No conditions at all: a Ready Unknown is added.
        let mut bare = json!({});
        mark_unknown(&mut bare, "now");
        assert_eq!(bare["status"]["conditions"][0]["type"], "Ready");
        assert_eq!(bare["status"]["conditions"][0]["status"], "Unknown");
    }

    fn node(name: &str, created_ago: i64, heartbeat_ago: i64) -> Value {
        json!({"apiVersion": "v1", "kind": "Node",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "resourceVersion": "1", "creationTimestamp": ago(created_ago)},
            "spec": {},
            "status": {"conditions": [{"type": "Ready", "status": "True", "reason": "KubeletReady",
                "lastHeartbeatTime": ago(heartbeat_ago), "lastTransitionTime": ago(created_ago)}]}})
    }

    async fn nodes(Query(q): Query<HashMap<String, String>>) -> Response {
        list_or_watch(&q, json!([node("old", NINE_DAYS, NINE_DAYS - 1_560), node("live", 600, 5),
            node("nolease", NINE_DAYS, NINE_DAYS - 60)]))
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
    /// unreachable taints, and its pods (no toleration) evicted. So is one
    /// with no Lease and a nine-day-old heartbeat. A live Node beside them is
    /// left alone.
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

        let marked = |s: &Seen, name: &str| {
            s.node_puts.iter().any(|n| {
                n["metadata"]["name"] == name
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
                    if marked(&s, "old") && marked(&s, "nolease") && drained(&s) {
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
