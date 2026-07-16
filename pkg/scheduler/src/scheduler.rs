//! Core scheduler loop.
//!
//! Watches for pods without a nodeName, runs filter and score plugins,
//! then binds the pod to the best node via the API server.

use crate::filter::{self, FilterResult};
use crate::score;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

/// TLS/auth settings for talking to an HTTPS apiserver (mutual TLS or token).
#[derive(Default)]
pub struct ClientConfig {
    /// CA bundle (PEM) to verify the server.
    pub ca_pem: Option<Vec<u8>>,
    /// Client certificate (PEM) for mutual TLS.
    pub client_cert_pem: Option<Vec<u8>>,
    /// Client private key (PEM) for mutual TLS.
    pub client_key_pem: Option<Vec<u8>>,
    /// Bearer token.
    pub token: Option<String>,
    /// Skip server certificate verification.
    pub insecure: bool,
}

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

    /// Build a client with TLS + auth (for HTTPS apiservers / drop-in use).
    pub fn configured(base_url: &str, cfg: ClientConfig) -> anyhow::Result<Self> {
        let mut b = reqwest::Client::builder();
        if cfg.insecure {
            b = b.danger_accept_invalid_certs(true);
        }
        if let Some(ca) = &cfg.ca_pem {
            b = b.add_root_certificate(reqwest::Certificate::from_pem(ca)?);
        }
        if let (Some(cert), Some(key)) = (&cfg.client_cert_pem, &cfg.client_key_pem) {
            let mut pem = cert.clone();
            pem.push(b'\n');
            pem.extend_from_slice(key);
            b = b.identity(reqwest::Identity::from_pem(&pem)?);
        }
        if let Some(token) = &cfg.token {
            let mut headers = reqwest::header::HeaderMap::new();
            let mut val = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?;
            val.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, val);
            b = b.default_headers(headers);
        }
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: b.build()?,
        })
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

    /// Raw GET returning the response (so callers can distinguish 404).
    pub async fn get(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
    }

    /// POST (create) returning the decoded body.
    pub async fn create(&self, path: &str, body: &Value) -> reqwest::Result<Value> {
        self.client
            .post(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await?
            .json()
            .await
    }
}

/// Best-effort node/pod identity for the leader-election Lease holder.
fn default_identity() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "kube-scheduler".to_string())
}

/// The scheduler — assigns unscheduled pods to nodes.
pub struct Scheduler {
    api: Arc<ApiClient>,
    leader_elect: bool,
    identity: String,
}

impl Scheduler {
    pub fn new(api_server_url: &str) -> Self {
        Self {
            api: Arc::new(ApiClient::new(api_server_url)),
            leader_elect: true,
            identity: default_identity(),
        }
    }

    /// Connect with TLS + auth (HTTPS apiserver / mutual TLS or token).
    pub fn connect(api_server_url: &str, cfg: ClientConfig) -> anyhow::Result<Self> {
        Ok(Self {
            api: Arc::new(ApiClient::configured(api_server_url, cfg)?),
            leader_elect: true,
            identity: default_identity(),
        })
    }

    /// Enable/disable leader election (default on, upstream behavior).
    pub fn with_leader_election(mut self, enabled: bool) -> Self {
        self.leader_elect = enabled;
        self
    }

    /// Run the scheduler. With leader election, only the elected leader schedules
    /// — so 3 masters can each run a kube-scheduler without double-binding.
    pub async fn run(&self) -> anyhow::Result<()> {
        // Prometheus /metrics + /healthz (scraped by ironprom), upstream :10259.
        crate::metrics_server::spawn(10259);

        if !self.leader_elect {
            info!("Scheduler started (leader election disabled)");
            return self.scheduling_loop().await;
        }

        let elector = crate::leaderelection::LeaderElector::new(
            self.api.clone(),
            "kube-scheduler",
            "kube-system",
            &self.identity,
        );
        info!(
            "Scheduler leader election enabled (identity={})",
            self.identity
        );
        loop {
            elector.acquire().await;
            info!("Became leader; scheduling pods");
            let mut interval = time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                // Renew before each pass; step down immediately if we lost it.
                if !elector.try_acquire_or_renew().await {
                    warn!("Lost leadership; pausing scheduling");
                    break;
                }
                if let Err(e) = self.schedule_pending_pods().await {
                    error!("Scheduler error: {e}");
                }
            }
        }
    }

    /// The bare scheduling loop (no leader election).
    async fn scheduling_loop(&self) -> anyhow::Result<()> {
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

        // Collect all unscheduled, non-terminal pods across namespaces.
        let mut pending: Vec<(String, Value)> = Vec::new();
        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default").to_string();
            let pod_list: Value = self
                .api
                .list(&format!("/api/v1/namespaces/{ns_name}/pods"))
                .await?;
            for pod in pod_list["items"].as_array().cloned().unwrap_or_default() {
                if !pod["spec"]["nodeName"].as_str().unwrap_or("").is_empty() {
                    continue; // already scheduled
                }
                let phase = pod["status"]["phase"].as_str().unwrap_or("Pending");
                if phase == "Succeeded" || phase == "Failed" {
                    continue; // terminal
                }
                pending.push((ns_name.clone(), pod));
            }
        }

        // PrioritySort: highest priority first, ties broken by creationTimestamp.
        pending.sort_by(|a, b| {
            pod_priority(&b.1)
                .cmp(&pod_priority(&a.1))
                .then_with(|| creation_ts(&a.1).cmp(&creation_ts(&b.1)))
        });

        for (ns_name, pod) in &pending {
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            match self.schedule_pod(ns_name, pod, &nodes).await {
                Ok(chosen_node) => info!("Scheduled pod {ns_name}/{pod_name} -> {chosen_node}"),
                Err(e) => debug!("Failed to schedule pod {ns_name}/{pod_name}: {e}"),
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

/// Pod scheduling priority (`spec.priority`, resolved from PriorityClass by
/// admission upstream); default 0. Higher schedules first.
pub fn pod_priority(pod: &serde_json::Value) -> i64 {
    pod["spec"]["priority"].as_i64().unwrap_or(0)
}

fn creation_ts(pod: &serde_json::Value) -> String {
    pod["metadata"]["creationTimestamp"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn priority_sort_orders_high_first_then_by_creation() {
        let mk = |name: &str, prio: Option<i64>, ts: &str| {
            let mut spec = json!({});
            if let Some(p) = prio {
                spec["priority"] = json!(p);
            }
            ("default".to_string(),
             json!({"metadata":{"name":name,"creationTimestamp":ts},"spec":spec}))
        };
        let mut v = vec![
            mk("low", Some(0), "2026-01-01T00:00:02Z"),
            mk("high", Some(1000), "2026-01-01T00:00:03Z"),
            mk("old-default", None, "2026-01-01T00:00:00Z"),
            mk("new-default", None, "2026-01-01T00:00:01Z"),
        ];
        v.sort_by(|a, b| {
            pod_priority(&b.1)
                .cmp(&pod_priority(&a.1))
                .then_with(|| creation_ts(&a.1).cmp(&creation_ts(&b.1)))
        });
        let order: Vec<&str> = v.iter().map(|(_, p)| p["metadata"]["name"].as_str().unwrap()).collect();
        assert_eq!(order, ["high", "old-default", "new-default", "low"]);
    }
}
