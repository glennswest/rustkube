//! Core scheduler loop.
//!
//! Watches for pods without a nodeName, runs filter and score plugins,
//! then binds the pod to the best node via the API server.

use crate::filter::{self, FilterResult};
use crate::score;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

/// HTTP client for API server communication (same as controller manager).
#[derive(Clone)]
pub struct ApiClient {
    pub base_url: String,
    pub client: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn list(&self, path: &str) -> reqwest::Result<Value> {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await?
            .json()
            .await
    }

    pub async fn update(&self, path: &str, body: &Value) -> reqwest::Result<Value> {
        self.client
            .put(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await?
            .json()
            .await
    }
}

/// The scheduler — assigns unscheduled pods to nodes.
pub struct Scheduler {
    api: Arc<ApiClient>,
}

impl Scheduler {
    pub fn new(api_server_url: &str) -> Self {
        Self {
            api: Arc::new(ApiClient::new(api_server_url)),
        }
    }

    /// Run the scheduler loop forever.
    pub async fn run(&self) -> anyhow::Result<()> {
        info!("Scheduler started");
        // Prometheus /metrics + /healthz (scraped by ironprom), upstream :10259.
        crate::metrics_server::spawn(10259);
        let mut interval = time::interval(Duration::from_secs(1));

        loop {
            interval.tick().await;
            if let Err(e) = self.schedule_pending_pods().await {
                error!("Scheduler error: {e}");
            }
        }
    }

    async fn schedule_pending_pods(&self) -> anyhow::Result<()> {
        // Get all nodes
        let node_list: Value = self.api.list("/api/v1/nodes").await?;
        let nodes = node_list["items"].as_array().cloned().unwrap_or_default();

        if nodes.is_empty() {
            return Ok(()); // No nodes to schedule onto
        }

        // Get all namespaces, then check each for unscheduled pods
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            let pod_list: Value = self
                .api
                .list(&format!("/api/v1/namespaces/{ns_name}/pods"))
                .await?;
            let pods = pod_list["items"].as_array().cloned().unwrap_or_default();

            for pod in &pods {
                let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
                let node_name = pod["spec"]["nodeName"].as_str().unwrap_or("");

                // Skip already-scheduled pods
                if !node_name.is_empty() {
                    continue;
                }

                // Skip terminated pods
                let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
                if phase == "Succeeded" || phase == "Failed" {
                    continue;
                }

                // Schedule this pod
                match self.schedule_pod(ns_name, pod, &nodes).await {
                    Ok(chosen_node) => {
                        info!("Scheduled pod {ns_name}/{pod_name} -> {chosen_node}");
                    }
                    Err(e) => {
                        debug!("Failed to schedule pod {ns_name}/{pod_name}: {e}");
                    }
                }
            }
        }

        Ok(())
    }

    async fn schedule_pod(
        &self,
        namespace: &str,
        pod: &Value,
        nodes: &[Value],
    ) -> anyhow::Result<String> {
        let pod_name = pod["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("pod missing name"))?;

        // Phase 1: Filter — find nodes that can run this pod
        let feasible: Vec<&Value> = nodes
            .iter()
            .filter(|node| {
                let result = filter::run_filters(pod, node);
                matches!(result, FilterResult::Pass)
            })
            .collect();

        if feasible.is_empty() {
            return Err(anyhow::anyhow!(
                "no feasible nodes for pod {namespace}/{pod_name}"
            ));
        }

        // Phase 2: Score — rank feasible nodes
        let mut scored: Vec<(&Value, i64)> = feasible
            .iter()
            .map(|node| (*node, score::score_node(pod, node)))
            .collect();

        // Sort by score descending
        scored.sort_by(|a, b| b.1.cmp(&a.1));

        let chosen = scored[0].0;
        let chosen_name = chosen["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("node missing name"))?;

        // Phase 3: Bind — update the pod with the chosen node
        let mut bound_pod = pod.clone();
        bound_pod["spec"]["nodeName"] = json!(chosen_name);
        bound_pod["status"]["phase"] = json!("Pending");
        bound_pod["status"]["conditions"] = json!([
            {
                "type": "PodScheduled",
                "status": "True",
                "reason": "Scheduled",
                "message": format!("Bound to node {chosen_name}"),
                "lastTransitionTime": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
            }
        ]);

        self.api
            .update(
                &format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
                &bound_pod,
            )
            .await?;

        Ok(chosen_name.to_string())
    }
}
