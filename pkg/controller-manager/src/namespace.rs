//! Namespace controller.
//!
//! Ensures default ServiceAccount exists in each namespace.
//! Handles namespace deletion by cleaning up resources.

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::Index;
use apimachinery::workqueue::WorkQueue;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info};

pub struct NamespaceController {
    api: Arc<ApiClient>,
}

impl NamespaceController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    /// Discovery changes replace the bounded worker scopes. Default SA
    /// provisioning has its own pool, independent of slow namespace teardown.
    pub async fn run(&self) {
        let changed = WorkQueue::new();
        let wake = changed.clone();
        let _discovery = self.api.informers.subscribe(
            &self.api.client,
            format!(
                "{}/apis/apiextensions.k8s.io/v1/customresourcedefinitions",
                self.api.base_url
            ),
            move |_, _| {
                wake.add(());
            },
        );
        loop {
            let resources = match self.discover_namespaced_resources().await {
                Ok(resources) => resources,
                Err(error) => {
                    tracing::warn!(%error,"namespace discovery failed; no cleanup authorized");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };
            let provision = Provision(self);
            let terminate = Terminate {
                controller: self,
                resources,
            };
            tokio::select! {
                _ = async { tokio::join!(owned::run(&self.api,&provision),owned::run(&self.api,&terminate)); } => {},
                work = changed.next() => { drop(work); },
            }
        }
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
    observed: &Value,
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
                    .delete_observed(
                        &format!("{api_root}/namespaces/{namespace}/{resource}/{name}"),
                        item,
                    )
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
            "metadata": { "name": namespace, "uid": observed["metadata"]["uid"], "resourceVersion": observed["metadata"]["resourceVersion"] },
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

    anyhow::ensure!(
        resp.status().as_u16() == 404,
        "cannot observe default ServiceAccount: {}",
        resp.status()
    );
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

fn route_namespace(
    delta: &apimachinery::informer::Delta,
    primary: &apimachinery::informers::Feed,
) -> Vec<apimachinery::informer::Key> {
    delta
        .old
        .iter()
        .chain(delta.new.iter())
        .flat_map(|object| {
            owned::keys_at(
                primary,
                Index::Name(
                    "".into(),
                    object["metadata"]["namespace"]
                        .as_str()
                        .unwrap_or("")
                        .into(),
                ),
            )
        })
        .collect()
}
fn terminating(ns: &Value) -> bool {
    !ns["metadata"]["deletionTimestamp"].is_null() || ns["status"]["phase"] == "Terminating"
}
struct Provision<'a>(&'a NamespaceController);
struct Terminate<'a> {
    controller: &'a NamespaceController,
    resources: Vec<(String, String)>,
}
#[async_trait::async_trait]
impl Controller for Provision<'_> {
    fn name(&self) -> &'static str {
        "namespace-provision"
    }
    fn primary(&self) -> &'static str {
        "/api/v1/namespaces"
    }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency {
            path: "/api/v1/serviceaccounts".into(),
            route: Arc::new(route_namespace),
        }]
    }
    async fn reconcile(&self, ns: &Value, _: &[Value], _: &Deps) -> anyhow::Result<()> {
        if !terminating(ns) {
            ensure_default_service_account(
                &self.0.api,
                ns["metadata"]["name"].as_str().unwrap_or(""),
            )
            .await?;
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl Controller for Terminate<'_> {
    fn name(&self) -> &'static str {
        "namespace-terminate"
    }
    fn primary(&self) -> &'static str {
        "/api/v1/namespaces"
    }
    fn dependencies(&self) -> Vec<Dependency> {
        self.resources
            .iter()
            .map(|(root, resource)| Dependency {
                path: format!("{root}/{resource}"),
                route: Arc::new(route_namespace),
            })
            .collect()
    }
    async fn reconcile(&self, ns: &Value, _: &[Value], deps: &Deps) -> anyhow::Result<()> {
        if !terminating(ns) {
            return Ok(());
        }
        let name = ns["metadata"]["name"].as_str().unwrap_or("");
        let mut remaining = 0;
        for (i, (root, resource)) in self.resources.iter().enumerate() {
            let items = deps.feed(i).select(&Index::Namespace(name.into()))?;
            remaining += items.len();
            for item in items {
                if !item["metadata"]["deletionTimestamp"].is_null() {
                    continue;
                }
                let item_name = item["metadata"]["name"].as_str().unwrap_or("");
                self.controller
                    .api
                    .delete_observed(
                        &format!("{root}/namespaces/{name}/{resource}/{item_name}"),
                        &item,
                    )
                    .await?;
            }
        }
        if remaining == 0 {
            // Confirm absence with authoritative paginated reads before removing
            // the Namespace finalizer; a synced watch may still trail new writes.
            terminate_namespace(&self.controller.api, name, &self.resources, ns).await?;
        }
        Ok(())
    }
}
