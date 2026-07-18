//! Namespace controller.
//!
//! Ensures default ServiceAccount exists in each namespace.
//! Handles namespace deletion by cleaning up resources.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

pub struct NamespaceController {
    api: Arc<ApiClient>,
}

impl NamespaceController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Namespace controller started");
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Namespace reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            let terminating = !ns["metadata"]["deletionTimestamp"].is_null()
                || ns["status"]["phase"].as_str() == Some("Terminating");

            if terminating {
                // Cascade-delete everything in the namespace, then finalize (#28).
                if let Err(e) = self.terminate_namespace(ns_name).await {
                    debug!("Failed to terminate namespace {ns_name}: {e}");
                }
            } else if let Err(e) = self.ensure_default_service_account(ns_name).await {
                debug!("Failed to ensure default SA in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    /// Drive a Terminating namespace to deletion: purge every namespaced
    /// resource in it, and once nothing remains, clear the `kubernetes`
    /// finalizer via /finalize so the apiserver removes the namespace object.
    async fn terminate_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let resources = self.discover_namespaced_resources().await;

        let mut remaining = 0usize;
        for (api_root, resource) in &resources {
            let list_path = format!("{api_root}/namespaces/{namespace}/{resource}");
            let items = match self.api.list(&list_path).await {
                Ok(v) => v["items"].as_array().cloned().unwrap_or_default(),
                // A resource type we can't list — skip it rather than stall
                // termination forever.
                Err(_) => continue,
            };
            for item in &items {
                if let Some(name) = item["metadata"]["name"].as_str() {
                    let _ = self
                        .api
                        .delete(&format!("{api_root}/namespaces/{namespace}/{resource}/{name}"))
                        .await;
                }
            }
            remaining += items.len();
        }

        if remaining == 0 {
            // Empty — clear the finalizer; the apiserver then deletes the object.
            let body = json!({
                "apiVersion": "v1",
                "kind": "Namespace",
                "metadata": { "name": namespace },
                "spec": { "finalizers": [] }
            });
            let _ = self
                .api
                .update(&format!("/api/v1/namespaces/{namespace}/finalize"), &body)
                .await;
            info!("Namespace {namespace} terminated (finalized)");
        } else {
            debug!("Namespace {namespace} terminating: purged {remaining} resource(s) this pass");
        }
        Ok(())
    }

    /// Discover every namespaced (non-subresource) API resource from the
    /// apiserver's discovery documents, so termination purges CRDs and built-ins
    /// alike. Returns `(api_root, resource)` pairs, e.g. `("/apis/apps/v1",
    /// "deployments")`.
    async fn discover_namespaced_resources(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();

        // Core group is served at /api/v1.
        self.collect_namespaced("/api/v1", &mut out).await;

        // Named groups from /apis, each at its preferred version.
        if let Ok(groups) = self.api.list("/apis").await {
            if let Some(arr) = groups["groups"].as_array() {
                for g in arr {
                    if let Some(gv) = g["preferredVersion"]["groupVersion"].as_str() {
                        let root = format!("/apis/{gv}");
                        self.collect_namespaced(&root, &mut out).await;
                    }
                }
            }
        }
        out
    }

    /// Append namespaced, non-subresource resources listed at `api_root` to `out`.
    async fn collect_namespaced(&self, api_root: &str, out: &mut Vec<(String, String)>) {
        if let Ok(doc) = self.api.list(api_root).await {
            if let Some(list) = doc["resources"].as_array() {
                for r in list {
                    let name = r["name"].as_str().unwrap_or("");
                    let namespaced = r["namespaced"].as_bool().unwrap_or(false);
                    // Skip subresources (e.g. pods/status) and non-namespaced ones.
                    if namespaced && !name.is_empty() && !name.contains('/') {
                        out.push((api_root.to_string(), name.to_string()));
                    }
                }
            }
        }
    }

    async fn ensure_default_service_account(&self, namespace: &str) -> anyhow::Result<()> {
        let path = format!("/api/v1/namespaces/{namespace}/serviceaccounts/default");
        let resp = self.api.get(&path).await?;

        if resp.status().is_success() {
            return Ok(()); // Already exists
        }

        let sa = json!({
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": {
                "name": "default",
                "namespace": namespace
            }
        });

        self.api
            .create(
                &format!("/api/v1/namespaces/{namespace}/serviceaccounts"),
                &sa,
            )
            .await?;
        info!("Created default ServiceAccount in {namespace}");
        Ok(())
    }
}
