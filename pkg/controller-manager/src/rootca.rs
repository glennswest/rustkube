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
//! Polls like the other controllers: every namespace that is not terminating
//! gets the ConfigMap if it has none, and has it put back if its `ca.crt` was
//! changed.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

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
        info!("Root CA publisher started ({NAME} in every namespace)");
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Root CA publisher: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let list = self.api.list("/api/v1/namespaces").await?;
        for ns in list["items"].as_array().cloned().unwrap_or_default() {
            let terminating = !ns["metadata"]["deletionTimestamp"].is_null()
                || ns["status"]["phase"].as_str() == Some("Terminating");
            let Some(name) = ns["metadata"]["name"].as_str() else { continue };
            if terminating {
                continue;
            }
            if let Err(e) = self.reconcile(name).await {
                debug!("Root CA publisher: {name}: {e}");
            }
        }
        Ok(())
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
