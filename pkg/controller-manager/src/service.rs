//! Service controller.
//!
//! Indexed Service workers observe Pod membership and owned endpoint changes.
//! For each Service with a selector, finds matching pods and creates/updates
//! the corresponding Endpoints resource with the pod IPs and ports, and the
//! matching `discovery.k8s.io/v1` EndpointSlice.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use apimachinery::informer::{Index, Key};
use crate::owned::{self, Controller, Dependency, Deps};

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

        // Get the selector
        let selector = &svc["spec"]["selector"];
        if selector.is_null() || !selector.is_object() {
            return Ok(()); // No selector = no endpoints (e.g., ExternalName)
        }

        let selector_map = selector.as_object().unwrap();

        // Find matching pods
        let matching_pods: Vec<&Value> = all_pods
            .iter()
            .filter(|pod| {
                let labels = pod["metadata"]["labels"].as_object();
                match labels {
                    Some(pod_labels) => selector_map.iter().all(|(k, v)| {
                        pod_labels.get(k) == Some(v)
                    }),
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
        self.upsert(&ep_path, endpoints, &["subsets"]).await?;

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
        self.upsert(&slice_path, slice, &["addressType", "endpoints", "ports"]).await?;

        Ok(())
    }
    async fn upsert(&self, path: &str, mut desired: Value, fields: &[&str]) -> anyhow::Result<()> {
        let response = self.api.get(path).await?;
        if response.status().as_u16() == 404 {
            self.api.create(path.rsplit_once('/').unwrap().0, &desired).await?;
            return Ok(());
        }
        let current: Value = response.error_for_status()?.json().await?;
        let owner = &desired["metadata"]["ownerReferences"][0]["uid"];
        anyhow::ensure!(current["metadata"]["ownerReferences"].as_array().is_some_and(|refs|
            refs.iter().any(|r| &r["uid"] == owner && r["controller"] == true)),
            "refusing to overwrite endpoints owned by another Service UID");
        if fields.iter().all(|field| current[*field] == desired[*field])
            && current["metadata"]["labels"] == desired["metadata"]["labels"] { return Ok(()); }
        desired["metadata"]["resourceVersion"] = current["metadata"]["resourceVersion"].clone();
        self.api.update(path, &desired).await?;
        Ok(())
    }

}

#[async_trait::async_trait]
impl Controller for ServiceController {
    fn name(&self) -> &'static str { "service" }
    fn primary(&self) -> &'static str { "/api/v1/services" }
    fn children(&self) -> Option<&'static str> { Some("/api/v1/endpoints") }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency { path: "/api/v1/pods".into(), route: Arc::new(owned::pod_membership) },
            Dependency { path: "/apis/discovery.k8s.io/v1/endpointslices".into(), route: Arc::new(owned::owner_keys) },
        ]
    }
    async fn reconcile(&self, svc: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        if !svc["metadata"]["deletionTimestamp"].is_null() { return Ok(()); }
        let mut pods = owned::selected_pods(svc, deps.feed(0))?;
        // Hash-index iteration is unordered; stable endpoint order avoids writes on every wake.
        pods.sort_by(|a,b| a["metadata"]["uid"].as_str().cmp(&b["metadata"]["uid"].as_str()));
        self.reconcile_service(svc["metadata"]["namespace"].as_str().unwrap_or("default"), svc, &pods).await
    }
    async fn deleted(&self, key: &Key, children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        // A stale informer must not authorize collection of a still-live owner.
        let path = format!("/api/v1/namespaces/{}/services/{}", key.namespace, key.name);
        let response = self.api.get(&path).await?;
        if response.status().as_u16() != 404 {
            let current: Value = response.error_for_status()?.json().await?;
            anyhow::ensure!(current["metadata"]["uid"].as_str() != Some(&key.uid), "Service still exists");
        }
        let slices = deps.feed(1).select(&Index::Owner(key.uid.clone()))?;
        for (plural, items) in [("/api/v1", children), ("/apis/discovery.k8s.io/v1", slices.as_slice())] {
            for item in items {
                if !item["metadata"]["ownerReferences"].as_array().is_some_and(|refs|
                    refs.iter().any(|r| r["kind"] == "Service" && r["controller"] == true
                        && r["uid"].as_str() == Some(key.uid.as_str()))) { continue; }
                let resource = if plural == "/api/v1" { "endpoints" } else { "endpointslices" };
                let name = item["metadata"]["name"].as_str().unwrap_or("");
                self.api.delete_observed(&format!("{plural}/namespaces/{}/{resource}/{name}", key.namespace), item).await?;
            }
        }
        Ok(())
    }
}
