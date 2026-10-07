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
    ) -> anyhow::Result<()> {
        let svc_name = svc["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("service missing name"))?;
        let svc_uid = svc["metadata"]["uid"].as_str().unwrap_or("");

        // No selector: the Endpoints are someone else's, mirrored (#133).
        if !mirroring::has_selector(svc) {
            return Ok(());
        }
        let selector = &svc["spec"]["selector"];

        let selector_map = selector.as_object().unwrap();

        // Find matching pods
        let matching_pods: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                let labels = pod["metadata"]["labels"].as_object();
                match labels {
                    Some(pod_labels) => selector_map
                        .iter()
                        .all(|(k, v)| pod_labels.get(k) == Some(v)),
                    None => false,
                }
            })
            .filter(|pod| {
                // Only include Running pods with a pod IP
                let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
                phase == "Running" && pod["status"]["podIP"].as_str().is_some()
            })
            .collect();

        // Build endpoints addresses
        let addresses: Vec<Value> = matching_pods
            .iter()
            .map(|pod| {
                json!({
                    "ip": pod["status"]["podIP"].as_str().unwrap_or(""),
                    "nodeName": pod["spec"]["nodeName"].as_str().unwrap_or(""),
                    "targetRef": {
                        "kind": "Pod",
                        "name": pod["metadata"]["name"].as_str().unwrap_or(""),
                        "namespace": namespace,
                        "uid": pod["metadata"]["uid"].as_str().unwrap_or("")
                    }
                })
            })
            .collect();

        // Build port list from the Service spec
        let ports: Vec<Value> = svc["spec"]["ports"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|port| {
                json!({
                    "name": port["name"].as_str().unwrap_or(""),
                    "port": port["targetPort"].as_u64()
                        .or_else(|| port["port"].as_u64())
                        .unwrap_or(0),
                    "protocol": port["protocol"].as_str().unwrap_or("TCP")
                })
            })
            .collect();

        let subsets = if addresses.is_empty() {
            vec![]
        } else {
            vec![json!({
                "addresses": addresses,
                "ports": ports
            })]
        };

        let endpoints = json!({
            "apiVersion": "v1",
            "kind": "Endpoints",
            "metadata": {
                "name": svc_name,
                "namespace": namespace,
                "labels": {"endpointslice.kubernetes.io/skip-mirror": "true"},
                "ownerReferences": [{
                    "apiVersion": "v1",
                    "kind": "Service",
                    "name": svc_name,
                    "uid": svc_uid,
                    "controller": true,
                    "blockOwnerDeletion": true
                }]
            },
            "subsets": subsets
        });

        // Create or update the Endpoints object
        let ep_path = format!("/api/v1/namespaces/{namespace}/endpoints/{svc_name}");
        self.upsert(&ep_path, endpoints, &["subsets"], false).await?;

        // Mirror the same backends into an EndpointSlice (discovery.k8s.io/v1) —
        // Cilium / kube-proxy-replacement use slices as the modern default (#22).
        // One IPv4 slice per service, labeled with the service name so consumers
        // find it. (Dual-stack would need a slice per addressType — follow-up.)
        let slice_endpoints: Vec<Value> = addresses
            .iter()
            .map(|a| {
                json!({
                    "addresses": [a["ip"].as_str().unwrap_or("")],
                    "conditions": { "ready": true },
                    "nodeName": a["nodeName"].clone(),
                    "targetRef": a["targetRef"].clone()
                })
            })
            .collect();
        let slice = json!({
            "apiVersion": "discovery.k8s.io/v1",
            "kind": "EndpointSlice",
            "metadata": {
                "name": svc_name,
                "namespace": namespace,
                // `managed-by` is how consumers (and the conformance suite)
                // tell the controller's slices from hand-made ones.
                "labels": {
                    "kubernetes.io/service-name": svc_name,
                    "endpointslice.kubernetes.io/managed-by": "endpointslice-controller.k8s.io"
                },
                "ownerReferences": [{
                    "apiVersion": "v1",
                    "kind": "Service",
                    "name": svc_name,
                    "uid": svc_uid,
                    "controller": true,
                    "blockOwnerDeletion": true
                }]
            },
            "addressType": "IPv4",
            "endpoints": slice_endpoints,
            "ports": ports
        });
        let slice_path =
            format!("/apis/discovery.k8s.io/v1/namespaces/{namespace}/endpointslices/{svc_name}");
        self.upsert(&slice_path, slice, &["addressType", "endpoints", "ports"], false)
            .await?;

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
            if !wanted.contains(&&slice["metadata"]["name"]) || slice["metadata"]["ownerReferences"][0]["uid"] != owner {
                let n = slice["metadata"]["name"].as_str().unwrap_or("");
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
        let current: Value = response.error_for_status()?.json().await?;
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
        self.reconcile_service(
            svc["metadata"]["namespace"].as_str().unwrap_or("default"),
            svc,
            &pods,
        )
        .await
    }
    async fn deleted(&self, key: &Key, children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        // A stale informer must not authorize collection of a still-live owner.
        let path = format!("/api/v1/namespaces/{}/services/{}", key.namespace, key.name);
        let response = self.api.get(&path).await?;
        if response.status().as_u16() != 404 {
            let current: Value = response.error_for_status()?.json().await?;
            anyhow::ensure!(
                current["metadata"]["uid"].as_str() != Some(&key.uid),
                "Service still exists"
            );
        }
        // Mirrored slices name the Endpoints as owner, not the Service.
        for slice in mirrored_slices(deps.feed(1), &key.namespace, &key.name)? {
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

/// The mirroring controller's slices for Service `name` in `namespace`.
fn mirrored_slices(slices: &Feed, namespace: &str, name: &str) -> anyhow::Result<Vec<Value>> {
    Ok(slices
        .select(&Index::Label("kubernetes.io/service-name".into(), name.into()))?
        .into_iter()
        .filter(|s| mirroring::is_mirrored(s, namespace, name))
        .collect())
}

/// A slice change wakes its owning Service and, for a mirrored slice, the
/// Service it is labelled with.
fn slice_services(delta: &Delta, primary: &Feed) -> Vec<Key> {
    let mut keys = owned::owner_keys(delta, primary);
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
