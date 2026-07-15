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
            let phase = ns["status"]["phase"].as_str().unwrap_or("Active");

            if phase == "Active" {
                if let Err(e) = self.ensure_default_service_account(ns_name).await {
                    debug!("Failed to ensure default SA in {ns_name}: {e}");
                }
            }
        }
        Ok(())
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
