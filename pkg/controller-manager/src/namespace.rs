//! Namespace controller.
//!
//! Ensures default ServiceAccount exists in each namespace.
//! Handles namespace deletion by cleaning up resources.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, error, info};

pub struct NamespaceController {
    api: Arc<ApiClient>,
}

impl NamespaceController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    /// Two loops, not one. Provisioning a new namespace's default
    /// ServiceAccount must not wait behind the purge of terminating ones: a
    /// purge lists every namespaced resource type in every terminating
    /// namespace, and with a few dozen of them — the conformance suite ends
    /// tests faster than that — one pass was thousands of sequential requests,
    /// new namespaces waited more than 30 s for their ServiceAccount, and the
    /// suite's namespace setup timed out (#67).
    pub async fn run(&self) {
        info!("Namespace controller started");
        tokio::join!(self.provision_loop(), self.terminate_loop());
    }

    /// On dependency changes: each live namespace has its default ServiceAccount.
    async fn provision_loop(&self) {
        let worker = self.api.watches.worker("namespace-provision");
        loop {
            let _work = worker.next().await;
            worker
                .run(async {
                    let namespaces = match self.namespaces().await {
                        Ok(n) => n,
                        Err(e) => {
                            error!("Namespace list failed: {e}");
                            return;
                        }
                    };
                    let limit = Arc::new(tokio::sync::Semaphore::new(16));
                    let mut tasks = tokio::task::JoinSet::new();
                    for (name, terminating) in namespaces {
                        if terminating {
                            continue;
                        }
                        let api = self.api.clone();
                        let limit = limit.clone();
                        tasks.spawn(apimachinery::reactor::inherit(async move {
                            let _permit = limit.acquire().await;
                            if let Err(e) = ensure_default_service_account(&api, &name).await {
                                debug!("Failed to ensure default SA in {name}: {e}");
                            }
                        }));
                    }
                    while tasks.join_next().await.is_some() {}
                })
                .await;
        }
    }

    /// On dependency changes: purge the terminating namespaces, several at once, with the
    /// resource types discovered once per pass rather than once per namespace.
    async fn terminate_loop(&self) {
        let worker = self.api.watches.worker("namespace-terminate");
        loop {
            let _work = worker.next().await;
            worker
                .run(async {
                    let terminating: Vec<String> = match self.namespaces().await {
                        Ok(n) => n.into_iter().filter(|(_, t)| *t).map(|(n, _)| n).collect(),
                        Err(e) => {
                            error!("Namespace list failed: {e}");
                            return;
                        }
                    };
                    if terminating.is_empty() {
                        return;
                    }
                    let resources = match self.discover_namespaced_resources().await {
                        Ok(resources) => Arc::new(resources),
                        Err(error) => {
                            apimachinery::reactor::failed();
                            error!("Namespace discovery failed: {error}");
                            return;
                        }
                    };
                    let limit = Arc::new(tokio::sync::Semaphore::new(8));
                    let mut tasks = tokio::task::JoinSet::new();
                    for name in terminating {
                        let api = self.api.clone();
                        let resources = resources.clone();
                        let limit = limit.clone();
                        tasks.spawn(apimachinery::reactor::inherit(async move {
                            let _permit = limit.acquire().await;
                            // Cascade-delete everything in the namespace, then finalize (#28).
                            if let Err(e) = terminate_namespace(&api, &name, &resources).await {
                                debug!("Failed to terminate namespace {name}: {e}");
                            }
                        }));
                    }
                    while tasks.join_next().await.is_some() {}
                })
                .await;
        }
    }

    /// `(name, terminating)` for every namespace.
    async fn namespaces(&self) -> anyhow::Result<Vec<(String, bool)>> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        Ok(ns_list["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|ns| {
                        let name = ns["metadata"]["name"].as_str()?.to_string();
                        let terminating = !ns["metadata"]["deletionTimestamp"].is_null()
                            || ns["status"]["phase"].as_str() == Some("Terminating");
                        Some((name, terminating))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Discover every namespaced (non-subresource) API resource from the
    /// apiserver's discovery documents, so termination purges CRDs and built-ins
    /// alike. Returns `(api_root, resource)` pairs, e.g. `("/apis/apps/v1",
    /// "deployments")`.
    async fn discover_namespaced_resources(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        self.collect_namespaced("/api/v1", &mut out).await?;
        let groups = self.api.list("/apis").await?;
        let groups = groups["groups"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid discovery groups"))?;
        for group in groups {
            if let Some(gv) = group["preferredVersion"]["groupVersion"].as_str() {
                self.collect_namespaced(&format!("/apis/{gv}"), &mut out)
                    .await?;
            }
        }
        Ok(out)
    }

    async fn collect_namespaced(
        &self,
        api_root: &str,
        out: &mut Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        let doc = self.api.list(api_root).await?;
        let resources = doc["resources"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid discovery resources"))?;
        for resource in resources {
            let name = resource["name"].as_str().unwrap_or("");
            let verbs = resource["verbs"].as_array();
            let can_list = verbs.is_some_and(|v| v.iter().any(|s| s == "list"));
            let can_delete = verbs.is_some_and(|v| v.iter().any(|s| s == "delete"));
            if resource["namespaced"].as_bool() == Some(true)
                && !name.is_empty()
                && !name.contains('/')
                && can_list
                && can_delete
            {
                out.push((api_root.to_string(), name.to_string()));
            }
        }
        Ok(())
    }
}

/// Drive a Terminating namespace to deletion: purge every namespaced
/// resource in it, and once nothing remains, clear the `kubernetes`
/// finalizer via /finalize so the apiserver removes the namespace object.
async fn terminate_namespace(
    api: &ApiClient,
    namespace: &str,
    resources: &[(String, String)],
) -> anyhow::Result<()> {
    let mut remaining = 0usize;
    for (api_root, resource) in resources {
        let list_path = format!("{api_root}/namespaces/{namespace}/{resource}");
        // Failure is unknown membership, never permission to finalize.
        let list = api.list(&list_path).await?;
        let items = list["items"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid collection {list_path}"))?;
        for item in items {
            if let Some(name) = item["metadata"]["name"].as_str() {
                let _ = api
                    .delete(&format!(
                        "{api_root}/namespaces/{namespace}/{resource}/{name}"
                    ))
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
        let _ = api
            .update(&format!("/api/v1/namespaces/{namespace}/finalize"), &body)
            .await;
        info!("Namespace {namespace} terminated (finalized)");
    } else {
        debug!("Namespace {namespace} terminating: purged {remaining} resource(s) this pass");
    }
    Ok(())
}

async fn ensure_default_service_account(api: &ApiClient, namespace: &str) -> anyhow::Result<()> {
    let path = format!("/api/v1/namespaces/{namespace}/serviceaccounts/default");
    let resp = api.get(&path).await?;

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

    api.create(
        &format!("/api/v1/namespaces/{namespace}/serviceaccounts"),
        &sa,
    )
    .await?;
    info!("Created default ServiceAccount in {namespace}");
    Ok(())
}
