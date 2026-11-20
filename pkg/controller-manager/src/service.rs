//! Service controller.
//!
//! Indexed Service workers observe Pod membership and owned endpoint changes.
//! For each Service with a selector, finds matching pods and creates/updates
//! the corresponding Endpoints resource with the pod IPs and ports, and the
//! matching `discovery.k8s.io/v1` EndpointSlice. The Endpoints it writes are
//! labelled `endpointslice.kubernetes.io/skip-mirror: "true"`, as upstream's
//! endpoints controller labels its own.
//!
//! A Service with no selector gets EndpointSlices mirrored from the
//! Endpoints written for it by hand (#133, `endpointslicemirroring.rs`).

use crate::endpointslicemirroring as mirroring;
use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::{Delta, Index, Key};
use apimachinery::informers::Feed;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct ServiceController {
    api: Arc<ApiClient>,
}

impl ServiceController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile_service(
        &self,
        namespace: &str,
        svc: &Value,
        all_pods: &[Value],
        existing_slices: &[Value],
    ) -> anyhow::Result<()> {
        let svc_name = svc["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("service missing name"))?;
        let svc_uid = svc["metadata"]["uid"].as_str().unwrap_or("");

        // No selector: the Endpoints are someone else's, mirrored (#133).
        if !mirroring::has_selector(svc) {
            return Ok(());
        }
        let groups = backends(svc, all_pods);

        let owner = json!([{
            "apiVersion": "v1",
            "kind": "Service",
            "name": svc_name,
            "uid": svc_uid,
            "controller": true,
            "blockOwnerDeletion": true
        }]);
        let subsets: Vec<Value> = groups
            .iter()
            .map(|g| {
                let mut subset = json!({"ports": g.ports});
                let addr = |b: &Backend| b.address(namespace);
                let ready: Vec<Value> = g.backends.iter().filter(|b| b.in_endpoints && b.ready).map(addr).collect();
                let not_ready: Vec<Value> = g.backends.iter().filter(|b| b.in_endpoints && !b.ready).map(addr).collect();
                if !ready.is_empty() {
                    subset["addresses"] = json!(ready);
                }
                if !not_ready.is_empty() {
                    subset["notReadyAddresses"] = json!(not_ready);
                }
                subset
            })
            .filter(|s| s.get("addresses").is_some() || s.get("notReadyAddresses").is_some())
            .collect();

        let endpoints = json!({
            "apiVersion": "v1",
            "kind": "Endpoints",
            "metadata": {
                "name": svc_name,
                "namespace": namespace,
                "labels": {"endpointslice.kubernetes.io/skip-mirror": "true"},
                "ownerReferences": owner
            },
            "subsets": subsets
        });

        // Create or update the Endpoints object
        let ep_path = format!("/api/v1/namespaces/{namespace}/endpoints/{svc_name}");
        self.upsert(&ep_path, endpoints, &["subsets"], false).await?;

        // The same backends as EndpointSlices (discovery.k8s.io/v1) — Cilium /
        // kube-proxy-replacement read slices (#22). One slice per resolved
        // port set (#362), as upstream's: the first keeps the Service's name,
        // the others are named by their ports; with no backend, one empty
        // slice. IPv4 only (dual-stack would need a slice per addressType).
        let mut wanted: Vec<(String, Value)> = Vec::new();
        let slices: Vec<(&Vec<Value>, Vec<Value>)> = if groups.is_empty() {
            vec![(&EMPTY_PORTS, Vec::new())]
        } else {
            groups.iter().map(|g| (&g.ports, g.backends.iter().map(|b| b.slice_endpoint(namespace)).collect())).collect()
        };
        for (i, (ports, endpoints)) in slices.into_iter().enumerate() {
            let name = if i == 0 { svc_name.to_string() } else { format!("{svc_name}-{}", port_set_hash(ports)) };
            let slice = json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSlice",
                "metadata": {
                    "name": name,
                    "namespace": namespace,
                    // `managed-by` is how consumers (and the conformance suite)
                    // tell the controller's slices from hand-made ones.
                    "labels": {
                        "kubernetes.io/service-name": svc_name,
                        "endpointslice.kubernetes.io/managed-by": MANAGED_BY
                    },
                    "ownerReferences": owner
                },
                "addressType": "IPv4",
                "endpoints": endpoints,
                "ports": ports
            });
            wanted.push((name, slice));
        }
        // Slices it made for port sets no longer served.
        for slice in existing_slices {
            let name = slice["metadata"]["name"].as_str().unwrap_or("");
            let ours = slice["metadata"]["labels"]["endpointslice.kubernetes.io/managed-by"].as_str() == Some(MANAGED_BY)
                && slice["metadata"]["ownerReferences"].as_array().is_some_and(|refs| {
                    refs.iter().any(|r| r["controller"] == true && r["uid"].as_str() == Some(svc_uid))
                });
            if ours && !wanted.iter().any(|(n, _)| n == name) {
                self.api
                    .delete_observed(&format!("/apis/discovery.k8s.io/v1/namespaces/{namespace}/endpointslices/{name}"), slice)
                    .await?;
            }
        }
        for (name, slice) in wanted {
            let slice_path = format!("/apis/discovery.k8s.io/v1/namespaces/{namespace}/endpointslices/{name}");
            self.upsert(&slice_path, slice, &["addressType", "endpoints", "ports"], false).await?;
        }

        Ok(())
    }

    /// The mirroring controller's half (#133): the slices `svc`'s Endpoints
    /// should have, written; the rest of its mirrored slices deleted.
    async fn mirror(&self, svc: &Value, deps: &Deps) -> anyhow::Result<()> {
        let ns = svc["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = svc["metadata"]["name"].as_str().unwrap_or("");
        let current = mirrored_slices(deps.feed(1), ns, name)?;
        let endpoints = deps.feed(2).select(&Index::Name(ns.into(), name.into()))?;
        let want = match endpoints.first() {
            Some(ep) if !mirroring::has_selector(svc) && mirroring::mirrors(ep) => mirroring::desired(ep),
            _ => Vec::new(),
        };
        let owner = endpoints.first().map(|e| e["metadata"]["uid"].clone()).unwrap_or(Value::Null);
        let wanted: Vec<&Value> = want.iter().map(|s| &s["metadata"]["name"]).collect();
        for slice in &current {
            // Not wanted, or written for an Endpoints since replaced.
            let unwanted = !wanted.contains(&&slice["metadata"]["name"]);
            let slice_owner = &slice["metadata"]["ownerReferences"][0]["uid"];
            if unwanted || *slice_owner != owner {
                let n = slice["metadata"]["name"].as_str().unwrap_or("");
                // Said, not silent: a delete here re-creating at once was #272's
                // loop, and nothing named the reason.
                tracing::info!(
                    "service: deleting mirrored EndpointSlice {ns}/{n}: {} (slice owner {slice_owner}, Endpoints {owner}, wanted {:?})",
                    if unwanted { "not wanted" } else { "owner changed" },
                    wanted
                );
                self.api.delete_observed(&format!("/apis/discovery.k8s.io/v1/namespaces/{ns}/endpointslices/{n}"), slice).await?;
            }
        }
        for slice in want {
            let n = slice["metadata"]["name"].as_str().unwrap_or("").to_string();
            self.upsert(&format!("/apis/discovery.k8s.io/v1/namespaces/{ns}/endpointslices/{n}"), slice,
                        &["addressType", "endpoints", "ports"], true).await?;
        }
        Ok(())
    }

    async fn upsert(&self, path: &str, mut desired: Value, fields: &[&str], annotations: bool) -> anyhow::Result<()> {
        let response = self.api.get(path).await?;
        if response.status().as_u16() == 404 {
            self.api
                .create(path.rsplit_once('/').unwrap().0, &desired)
                .await?;
            return Ok(());
        }
        let current: Value = apimachinery::api_status::check(response).await?.json().await?;
        let owner = &desired["metadata"]["ownerReferences"][0]["uid"];
        anyhow::ensure!(
            current["metadata"]["ownerReferences"]
                .as_array()
                .is_some_and(|refs| refs
                    .iter()
                    .any(|r| &r["uid"] == owner && r["controller"] == true)),
            "refusing to overwrite endpoints owned by another Service UID"
        );
        if fields
            .iter()
            .all(|field| current[*field] == desired[*field])
            && current["metadata"]["labels"] == desired["metadata"]["labels"]
            && (!annotations || current["metadata"]["annotations"] == desired["metadata"]["annotations"])
        {
            return Ok(());
        }
        desired["metadata"]["resourceVersion"] = current["metadata"]["resourceVersion"].clone();
        self.api.update(path, &desired).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Controller for ServiceController {
    fn name(&self) -> &'static str {
        "service"
    }
    fn primary(&self) -> &'static str {
        "/api/v1/services"
    }
    fn children(&self) -> Option<&'static str> {
        Some("/api/v1/endpoints")
    }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency {
                path: "/api/v1/pods".into(),
                route: Arc::new(owned::pod_membership),
            },
            Dependency {
                path: "/apis/discovery.k8s.io/v1/endpointslices".into(),
                route: Arc::new(slice_services),
            },
            // A selectorless Service's Endpoints, by name (#133): written by
            // hand, so no owner reference leads back to the Service.
            Dependency {
                path: "/api/v1/endpoints".into(),
                route: Arc::new(|delta: &Delta, primary: &Feed| {
                    let mut keys = Vec::new();
                    for ep in delta.old.iter().chain(delta.new.iter()) {
                        let ns = ep["metadata"]["namespace"].as_str().unwrap_or("");
                        let name = ep["metadata"]["name"].as_str().unwrap_or("");
                        keys.extend(owned::keys_at(primary, Index::Name(ns.into(), name.into())));
                    }
                    keys
                }),
            },
        ]
    }
    async fn reconcile(&self, svc: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        if !svc["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
        // Mirrored slices exist only for a selectorless Service; this also
        // removes them when a selector is added.
        self.mirror(svc, deps).await?;
        if !mirroring::has_selector(svc) {
            return Ok(());
        }
        let mut pods = owned::selected_pods(svc, deps.feed(0))?;
        // Hash-index iteration is unordered; stable endpoint order avoids writes on every wake.
        pods.sort_by(|a, b| {
            a["metadata"]["uid"]
                .as_str()
                .cmp(&b["metadata"]["uid"].as_str())
        });
        // The slices it owns, so one for a port set no longer served goes.
        let uid = svc["metadata"]["uid"].as_str().unwrap_or("").to_string();
        let slices = deps.feed(1).select(&Index::Owner(uid))?;
        self.reconcile_service(
            svc["metadata"]["namespace"].as_str().unwrap_or("default"),
            svc,
            &pods,
            &slices,
        )
        .await
    }
    async fn deleted(&self, key: &Key, children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        // A stale informer must not authorize collection of a still-live owner.
        let path = format!("/api/v1/namespaces/{}/services/{}", key.namespace, key.name);
        let response = self.api.get(&path).await?;
        let name_free = response.status().as_u16() == 404;
        if !name_free {
            let current: Value = apimachinery::api_status::check(response).await?.json().await?;
            anyhow::ensure!(
                current["metadata"]["uid"].as_str() != Some(&key.uid),
                "Service still exists"
            );
        }
        // Mirrored slices name the Endpoints as owner, not the Service, and
        // a Service now holding the name decides on them in `mirror` (#272).
        let mirrored = if name_free { mirrored_slices(deps.feed(1), &key.namespace, &key.name)? } else { Vec::new() };
        for slice in mirrored {
            let name = slice["metadata"]["name"].as_str().unwrap_or("");
            self.api
                .delete_observed(
                    &format!("/apis/discovery.k8s.io/v1/namespaces/{}/endpointslices/{name}", key.namespace),
                    &slice,
                )
                .await?;
        }
        let slices = deps.feed(1).select(&Index::Owner(key.uid.clone()))?;
        for (plural, items) in [
            ("/api/v1", children),
            ("/apis/discovery.k8s.io/v1", slices.as_slice()),
        ] {
            for item in items {
                if !item["metadata"]["ownerReferences"]
                    .as_array()
                    .is_some_and(|refs| {
                        refs.iter().any(|r| {
                            r["kind"] == "Service"
                                && r["controller"] == true
                                && r["uid"].as_str() == Some(key.uid.as_str())
                        })
                    })
                {
                    continue;
                }
                let resource = if plural == "/api/v1" {
                    "endpoints"
                } else {
                    "endpointslices"
                };
                let name = item["metadata"]["name"].as_str().unwrap_or("");
                self.api
                    .delete_observed(
                        &format!("{plural}/namespaces/{}/{resource}/{name}", key.namespace),
                        item,
                    )
                    .await?;
            }
        }
        Ok(())
    }
}

/// The `managed-by` label of the slices this controller writes.
const MANAGED_BY: &str = "endpointslice-controller.k8s.io";
static EMPTY_PORTS: Vec<Value> = Vec::new();

/// One selected Pod as a Service backend (#362).
#[derive(Debug, Clone)]
struct Backend {
    ip: String,
    node: Value,
    name: String,
    uid: String,
    /// Published as ready: its Ready condition, or `publishNotReadyAddresses`.
    ready: bool,
    /// Its Ready condition alone: an EndpointSlice's `serving`.
    serving: bool,
    terminating: bool,
    /// In the Endpoints: a terminating Pod is only under
    /// `publishNotReadyAddresses` (it stays in the slices, `terminating`).
    in_endpoints: bool,
}

impl Backend {
    fn target_ref(&self, namespace: &str) -> Value {
        json!({"kind": "Pod", "name": self.name, "namespace": namespace, "uid": self.uid})
    }
    fn address(&self, namespace: &str) -> Value {
        json!({"ip": self.ip, "nodeName": self.node, "targetRef": self.target_ref(namespace)})
    }
    fn slice_endpoint(&self, namespace: &str) -> Value {
        json!({
            "addresses": [self.ip],
            "conditions": {"ready": self.ready, "serving": self.serving, "terminating": self.terminating},
            "nodeName": self.node,
            "targetRef": self.target_ref(namespace)
        })
    }
}

/// Backends that resolved the same ports.
#[derive(Debug)]
struct Group {
    ports: Vec<Value>,
    backends: Vec<Backend>,
}

/// The selected Pods as backends, grouped by the ports they resolve to, as
/// upstream's endpoints and EndpointSlice controllers (#362, review findings
/// 7a974852b70e, 7d20cd929fba, 7ada61477e9f):
///
/// - a Pod is a backend once it has an IP and is not finished (a
///   `restartPolicy: Never` Pod that Succeeded or Failed, an `OnFailure` one
///   that Succeeded); a terminating one only in the slices (`terminating`),
///   or in the Endpoints too under `publishNotReadyAddresses`;
/// - it is ready when its `Ready` condition is True (or the Service says
///   `publishNotReadyAddresses`), never while terminating unless that is set;
///   an unready one is a `notReadyAddresses` entry and `ready: false`;
/// - each Service port's `targetPort` is resolved per Pod: a number is
///   itself, a name is the container port of that name and protocol (a Pod
///   without one does not back that port), and none is the Service port.
///
/// Groups are ordered by their ports, backends by their Pod's order.
fn backends(svc: &Value, pods: &[Value]) -> Vec<Group> {
    let publish = svc["spec"]["publishNotReadyAddresses"].as_bool() == Some(true);
    let svc_ports = svc["spec"]["ports"].as_array().cloned().unwrap_or_default();
    let mut groups: std::collections::BTreeMap<String, Group> = std::collections::BTreeMap::new();
    for pod in pods {
        let Some(ip) = pod["status"]["podIP"].as_str().filter(|ip| !ip.is_empty()) else { continue };
        let phase = pod["status"]["phase"].as_str().unwrap_or("");
        let finished = match pod["spec"]["restartPolicy"].as_str() {
            Some("Never") => matches!(phase, "Succeeded" | "Failed"),
            Some("OnFailure") => phase == "Succeeded",
            _ => false,
        };
        if finished {
            continue;
        }
        let terminating = !pod["metadata"]["deletionTimestamp"].is_null();
        let serving = pod["status"]["conditions"]
            .as_array()
            .is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"));
        let ready = publish || (serving && !terminating);
        let ports: Vec<Value> = svc_ports.iter().filter_map(|p| resolve_port(p, pod)).collect();
        if ports.is_empty() && !svc_ports.is_empty() {
            continue; // backs none of the Service's ports
        }
        let backend = Backend {
            ip: ip.to_string(),
            node: pod["spec"]["nodeName"].clone(),
            name: pod["metadata"]["name"].as_str().unwrap_or("").to_string(),
            uid: pod["metadata"]["uid"].as_str().unwrap_or("").to_string(),
            ready,
            serving,
            terminating,
            in_endpoints: !terminating || publish,
        };
        let key = Value::Array(ports.clone()).to_string();
        groups.entry(key).or_insert_with(|| Group { ports, backends: Vec::new() }).backends.push(backend);
    }
    groups.into_values().collect()
}

/// A Service port as one Pod serves it: `{name, port, protocol}` (and
/// `appProtocol`), or None when its named `targetPort` is no container port
/// of the Pod with that protocol.
fn resolve_port(svc_port: &Value, pod: &Value) -> Option<Value> {
    let protocol = svc_port["protocol"].as_str().unwrap_or("TCP");
    let port = match &svc_port["targetPort"] {
        Value::Number(n) => n.as_u64()?,
        Value::String(name) => match name.parse::<u64>() {
            Ok(n) => n,
            Err(_) => pod["spec"]["containers"]
                .as_array()?
                .iter()
                .flat_map(|c| c["ports"].as_array().into_iter().flatten())
                .find(|cp| cp["name"].as_str() == Some(name) && cp["protocol"].as_str().unwrap_or("TCP") == protocol)?
                ["containerPort"]
                .as_u64()?,
        },
        _ => svc_port["port"].as_u64()?,
    };
    let mut out = json!({"name": svc_port["name"].as_str().unwrap_or(""), "port": port, "protocol": protocol});
    if let Some(app) = svc_port["appProtocol"].as_str() {
        out["appProtocol"] = json!(app);
    }
    Some(out)
}

/// A short, stable name suffix for a port set (FNV-1a).
fn port_set_hash(ports: &[Value]) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in Value::Array(ports.to_vec()).to_string().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// The mirroring controller's slices for Service `name` in `namespace`.
fn mirrored_slices(slices: &Feed, namespace: &str, name: &str) -> anyhow::Result<Vec<Value>> {
    Ok(slices
        .select(&Index::Label("kubernetes.io/service-name".into(), name.into()))?
        .into_iter()
        .filter(|s| mirroring::is_mirrored(s, namespace, name))
        .collect())
}

/// `delta` with only its objects' Service owner references.
///
/// A mirrored slice is owned by its Endpoints, which has the Service's name
/// and its own UID. Routed as an owner, that made a key for a "Service" with
/// the Endpoints' UID; no Service has it, so the runner called `deleted` for
/// it, which deleted the mirrored slices `mirror` had just written — and the
/// new slice was routed the same way (#272, #133: a create/delete loop).
fn service_owned(delta: &Delta) -> Delta {
    let keep = |object: &Option<Value>| {
        object.clone().map(|mut o| {
            if let Some(refs) = o["metadata"]["ownerReferences"].as_array_mut() {
                refs.retain(|r| r["kind"] == "Service");
            }
            o
        })
    };
    Delta { old: keep(&delta.old), new: keep(&delta.new), affected: delta.affected.clone() }
}

/// A slice change wakes its owning Service and, for a mirrored slice, the
/// Service it is labelled with.
fn slice_services(delta: &Delta, primary: &Feed) -> Vec<Key> {
    let mut keys = owned::owner_keys(&service_owned(delta), primary);
    for slice in delta.old.iter().chain(delta.new.iter()) {
        let labels = &slice["metadata"]["labels"];
        if labels["endpointslice.kubernetes.io/managed-by"].as_str() != Some(mirroring::MANAGED_BY) {
            continue;
        }
        let ns = slice["metadata"]["namespace"].as_str().unwrap_or("");
        if let Some(svc) = labels["kubernetes.io/service-name"].as_str() {
            keys.extend(owned::keys_at(primary, Index::Name(ns.into(), svc.into())));
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mirrored slice routes through its label only: its Endpoints owner
    /// is not a Service (#272, #133). A Service owner reference is kept.
    #[test]
    fn only_service_owners_route_a_slice() {
        let slice = json!({"metadata": {"name": "web-1a2b3c4d", "namespace": "ns", "ownerReferences": [
            {"apiVersion": "v1", "kind": "Endpoints", "name": "web", "uid": "ep-uid"},
            {"apiVersion": "v1", "kind": "Service", "name": "web", "uid": "svc-uid"}]}});
        let d = service_owned(&Delta { old: None, new: Some(slice), affected: Default::default() });
        let refs = d.new.unwrap()["metadata"]["ownerReferences"].clone();
        assert_eq!(refs, json!([{"apiVersion": "v1", "kind": "Service", "name": "web", "uid": "svc-uid"}]));
        let mirrored = json!({"metadata": {"ownerReferences": [{"kind": "Endpoints", "name": "web", "uid": "ep-uid"}]}});
        let d = service_owned(&Delta { old: Some(mirrored), new: None, affected: Default::default() });
        assert_eq!(d.old.unwrap()["metadata"]["ownerReferences"], json!([]));
    }

    fn svc(ports: Value, publish: bool) -> Value {
        json!({"spec": {"selector": {"app": "web"}, "ports": ports, "publishNotReadyAddresses": publish}})
    }
    fn pod(name: &str, ip: &str, ready: bool, http: Option<u64>) -> Value {
        let ports: Vec<Value> = http.map(|p| json!({"name": "http", "containerPort": p})).into_iter().collect();
        json!({"metadata": {"name": name, "uid": name},
               "spec": {"nodeName": "n1", "containers": [{"name": "c", "ports": ports}]},
               "status": {"phase": "Running", "podIP": ip,
                          "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}})
    }
    fn ips(g: &Group, pick: impl Fn(&Backend) -> bool) -> Vec<String> {
        g.backends.iter().filter(|b| pick(b)).map(|b| b.ip.clone()).collect()
    }

    /// Review findings 7a974852b70e / 7d20cd929fba: a named targetPort is
    /// each Pod's own container port, not the Service port.
    #[test]
    fn a_named_target_port_resolves_per_pod_and_groups_by_port() {
        let s = svc(json!([{"name": "web", "port": 80, "targetPort": "http"}]), false);
        let pods = [pod("a", "10.0.0.1", true, Some(8080)), pod("b", "10.0.0.2", true, Some(9090)),
                    pod("c", "10.0.0.3", true, Some(8080)), pod("none", "10.0.0.4", true, None)];
        let groups = backends(&s, &pods);
        let got: Vec<(u64, Vec<String>)> = groups.iter()
            .map(|g| (g.ports[0]["port"].as_u64().unwrap(), ips(g, |_| true))).collect();
        assert_eq!(got, vec![(8080, vec!["10.0.0.1".into(), "10.0.0.3".into()]), (9090, vec!["10.0.0.2".into()])],
                   "no Pod falls back to port 80, and one without the port backs nothing");
        // Numbers, numeric strings and no targetPort.
        let p = pod("a", "10.0.0.1", true, Some(8080));
        assert_eq!(resolve_port(&json!({"port": 80, "targetPort": 8443}), &p).unwrap()["port"], 8443);
        assert_eq!(resolve_port(&json!({"port": 80, "targetPort": "8443"}), &p).unwrap()["port"], 8443);
        assert_eq!(resolve_port(&json!({"port": 80}), &p).unwrap()["port"], 80);
        // The protocol must match the container port's.
        assert!(resolve_port(&json!({"port": 53, "targetPort": "http", "protocol": "UDP"}), &p).is_none());
    }

    /// Review finding 7ada61477e9f: readiness is the Pod's Ready condition.
    #[test]
    fn an_unready_pod_is_not_a_ready_backend() {
        let ports = json!([{"name": "web", "port": 80, "targetPort": 8080}]);
        let mut gone = pod("gone", "10.0.0.5", true, None);
        gone["metadata"]["deletionTimestamp"] = json!("2026-11-20T00:00:00Z");
        let mut done = pod("done", "10.0.0.6", false, None);
        done["spec"]["restartPolicy"] = json!("Never");
        done["status"]["phase"] = json!("Succeeded");
        let mut pending = pod("pending", "", false, None);
        pending["status"]["phase"] = json!("Pending");
        let pods = [pod("up", "10.0.0.1", true, None), pod("down", "10.0.0.2", false, None), gone, done, pending];
        let g = &backends(&svc(ports.clone(), false), &pods)[0];
        assert_eq!(ips(g, |b| b.in_endpoints && b.ready), ["10.0.0.1"]);
        assert_eq!(ips(g, |b| b.in_endpoints && !b.ready), ["10.0.0.2"], "notReadyAddresses");
        assert_eq!(ips(g, |_| true), ["10.0.0.1", "10.0.0.2", "10.0.0.5"], "the terminating one stays in the slices");
        let gone = g.backends.iter().find(|b| b.ip == "10.0.0.5").unwrap();
        assert_eq!(gone.slice_endpoint("ns")["conditions"], json!({"ready": false, "serving": true, "terminating": true}));
        let down = g.backends.iter().find(|b| b.ip == "10.0.0.2").unwrap();
        assert_eq!(down.slice_endpoint("ns")["conditions"]["ready"], false);
        // publishNotReadyAddresses: every one is ready, terminating included.
        let g = &backends(&svc(ports, true), &pods)[0];
        assert_eq!(ips(g, |b| b.in_endpoints && b.ready), ["10.0.0.1", "10.0.0.2", "10.0.0.5"]);
    }

    #[test]
    fn slice_names_for_port_sets_are_stable() {
        let a = vec![json!({"name": "web", "port": 8080, "protocol": "TCP"})];
        let b = vec![json!({"name": "web", "port": 9090, "protocol": "TCP"})];
        assert_eq!(port_set_hash(&a), port_set_hash(&a.clone()));
        assert_ne!(port_set_hash(&a), port_set_hash(&b));
        assert_eq!(port_set_hash(&a).len(), 8);
    }
}
