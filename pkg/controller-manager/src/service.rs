//! Service controller.
//!
//! Watches Services and Pods, and manages Endpoints objects.
//! For each Service with a selector, finds matching pods and creates/updates
//! the corresponding Endpoints resource with the pod IPs and ports.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

pub struct ServiceController {
    api: Arc<ApiClient>,
}

impl ServiceController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Service controller started");
        let mut interval = time::interval(Duration::from_secs(3));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Service reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("Service reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let svc_list: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/services"))
            .await?;
        let services = svc_list["items"].as_array().cloned().unwrap_or_default();

        let pod_list: Value = self
            .api
            .list(&format!("/api/v1/namespaces/{namespace}/pods"))
            .await?;
        let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

        for svc in &services {
            if let Err(e) = self.reconcile_service(namespace, svc, &pods).await {
                let name = svc["metadata"]["name"].as_str().unwrap_or("?");
                debug!("Failed to reconcile service {namespace}/{name}: {e}");
            }
        }
        Ok(())
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
        let resp = self.api.get(&ep_path).await;

        match resp {
            Ok(r) if r.status().is_success() => {
                // Update existing
                self.api.update(&ep_path, &endpoints).await?;
            }
            _ => {
                // Create new
                self.api
                    .create(
                        &format!("/api/v1/namespaces/{namespace}/endpoints"),
                        &endpoints,
                    )
                    .await?;
            }
        }

        Ok(())
    }
}
