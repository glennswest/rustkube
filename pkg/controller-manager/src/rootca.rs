//! Root CA publisher: the `kube-root-ca.crt` ConfigMap in every namespace.
//!
//! Upstream's `root-ca-cert-publisher`. A pod verifies the apiserver with the
//! CA in this ConfigMap — the kubelet projects it into every ServiceAccount
//! token volume as `ca.crt` — so a namespace without it has pods that cannot
//! trust the API they talk to. The conformance suite waits for it in every
//! namespace it creates, before any test runs (#67).
//!
//! The bundle is `--root-ca-file`, or, without one, the CA this process
//! already trusts the apiserver with (`--certificate-authority`). With
//! neither there is nothing true to publish, and the controller does not run.
//!
//! Indexed Namespace workers observe ConfigMap changes: every namespace that is not terminating
//! gets the ConfigMap if it has none, and has it put back if its `ca.crt` was
//! changed.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::info;
use crate::owned::{self, Controller, Dependency, Deps};
use apimachinery::informer::{Index, Key};

pub const NAME: &str = "kube-root-ca.crt";
const KEY: &str = "ca.crt";

pub struct RootCaPublisher {
    api: Arc<ApiClient>,
    ca_pem: String,
}

/// The ConfigMap as upstream writes it.
pub fn configmap(namespace: &str, ca_pem: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": NAME,
            "namespace": namespace,
            "annotations": {
                "kubernetes.io/description": "Contains a CA bundle that can be used to verify \
                    the kube-apiserver when using internal endpoints such as the internal \
                    service IP or kubernetes.default.svc. No other usage is guaranteed across \
                    distributions of Kubernetes clusters."
            }
        },
        "data": { KEY: ca_pem }
    })
}

impl RootCaPublisher {
    pub fn new(api: Arc<ApiClient>, ca_pem: String) -> Self {
        Self { api, ca_pem }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    async fn reconcile(&self, namespace: &str) -> anyhow::Result<()> {
        let path = format!("/api/v1/namespaces/{namespace}/configmaps/{NAME}");
        let resp = self.api.get(&path).await?;
        if resp.status().as_u16() == 404 {
            self.api
                .create(
                    &format!("/api/v1/namespaces/{namespace}/configmaps"),
                    &configmap(namespace, &self.ca_pem),
                )
                .await?;
            info!("Published {NAME} in {namespace}");
            return Ok(());
        }
        if !resp.status().is_success() {
            anyhow::bail!("GET {path}: {}", resp.status());
        }
        let current: Value = resp.json().await?;
        if current["data"][KEY].as_str() == Some(self.ca_pem.as_str()) {
            return Ok(());
        }
        // Changed or emptied: put the bundle back, conditional on the version
        // just read so a concurrent writer is not silently overwritten.
        let mut desired = configmap(namespace, &self.ca_pem);
        desired["metadata"]["resourceVersion"] = current["metadata"]["resourceVersion"].clone();
        self.api.update(&path, &desired).await?;
        info!("Restored {NAME} in {namespace}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configmap_is_upstreams() {
        let cm = configmap("demo", "-----BEGIN CERTIFICATE-----\n…");
        assert_eq!(cm["metadata"]["name"], "kube-root-ca.crt");
        assert_eq!(cm["metadata"]["namespace"], "demo");
        assert_eq!(cm["data"]["ca.crt"], "-----BEGIN CERTIFICATE-----\n…");
        assert!(cm["metadata"]["annotations"]["kubernetes.io/description"]
            .as_str()
            .unwrap()
            .starts_with("Contains a CA bundle"));
    }
}

#[async_trait::async_trait]
impl Controller for RootCaPublisher {
    fn name(&self) -> &'static str { "rootca" }
    fn primary(&self) -> &'static str { "/api/v1/namespaces" }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency { path: "/api/v1/configmaps".into(), route: Arc::new(|delta, primary| {
            let mut result = Vec::new();
            for cm in delta.old.iter().chain(delta.new.iter()) {
                if cm["metadata"]["name"] != NAME { continue; }
                let ns = cm["metadata"]["namespace"].as_str().unwrap_or("");
                for object in primary.select(&Index::Name("".into(), ns.into())).unwrap_or_default() {
                    if let Ok(key) = Key::of(&object) { result.push(key); }
                }
            }
            result
        }) }]
    }
    async fn reconcile(&self, ns: &Value, _children: &[Value], _deps: &Deps) -> anyhow::Result<()> {
        if !ns["metadata"]["deletionTimestamp"].is_null() || ns["status"]["phase"] == "Terminating" { return Ok(()); }
        self.reconcile(ns["metadata"]["name"].as_str().unwrap_or("")).await
    }
}
