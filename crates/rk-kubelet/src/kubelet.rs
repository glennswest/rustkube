//! Kubelet — the main node agent loop.
//!
//! Registers the node, sends heartbeats, syncs pods, runs probes.

use crate::cri::{ImageService, RuntimeService};
use crate::node_status::NodeReporter;
use crate::pod_manager::PodManager;
use serde_json::Value;
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{error, info};

/// Kubelet configuration.
#[derive(Debug, Clone)]
pub struct KubeletConfig {
    pub node_name: String,
    pub api_server_url: String,
    pub heartbeat_interval: Duration,
    pub sync_interval: Duration,
}

impl Default for KubeletConfig {
    fn default() -> Self {
        Self {
            node_name: hostname(),
            api_server_url: "http://localhost:6443".into(),
            heartbeat_interval: Duration::from_secs(10),
            sync_interval: Duration::from_secs(2),
        }
    }
}

/// The kubelet node agent.
pub struct Kubelet {
    config: KubeletConfig,
    pod_manager: Arc<PodManager>,
    reporter: NodeReporter,
    api_client: reqwest::Client,
}

impl Kubelet {
    pub fn new(
        config: KubeletConfig,
        runtime: Arc<dyn RuntimeService>,
        images: Arc<dyn ImageService>,
    ) -> Self {
        let reporter = NodeReporter::new(&config.api_server_url, &config.node_name);
        let pod_manager = Arc::new(PodManager::new(runtime, images, &config.node_name));

        Self {
            config,
            pod_manager,
            reporter,
            api_client: reqwest::Client::new(),
        }
    }

    /// Run the kubelet. Blocks forever.
    pub async fn run(&self) -> anyhow::Result<()> {
        info!("Kubelet starting for node {}", self.config.node_name);

        // Register node
        self.reporter.register().await?;

        // Spawn heartbeat task
        let reporter_url = self.config.api_server_url.clone();
        let node_name = self.config.node_name.clone();
        let heartbeat_interval = self.config.heartbeat_interval;
        tokio::spawn(async move {
            let reporter = NodeReporter::new(&reporter_url, &node_name);
            let mut interval = time::interval(heartbeat_interval);
            loop {
                interval.tick().await;
                if let Err(e) = reporter.heartbeat().await {
                    error!("Heartbeat failed: {e}");
                }
            }
        });

        // Main sync loop
        let mut interval = time::interval(self.config.sync_interval);
        loop {
            interval.tick().await;
            if let Err(e) = self.sync().await {
                error!("Pod sync failed: {e}");
            }
        }
    }

    /// Sync pods: fetch desired pods from API server, reconcile with actual.
    async fn sync(&self) -> anyhow::Result<()> {
        // List all pods across all namespaces
        let resp: Value = self
            .api_client
            .get(format!("{}/api/v1/pods", self.config.api_server_url))
            .send()
            .await?
            .json()
            .await?;

        let pods = resp["items"].as_array().cloned().unwrap_or_default();

        // Filter to pods scheduled to this node
        let my_pods: Vec<Value> = pods
            .into_iter()
            .filter(|p| {
                p["spec"]["nodeName"].as_str() == Some(&self.config.node_name)
            })
            .collect();

        // Sync pod states
        let updates = self.pod_manager.sync_pods(&my_pods).await;

        // Report status updates back to API server
        for update in &updates {
            if let Err(e) = self.report_pod_status(update).await {
                error!(
                    "Failed to report status for {}/{}: {e}",
                    update.namespace, update.name
                );
            }
        }

        Ok(())
    }

    async fn report_pod_status(
        &self,
        update: &crate::pod_manager::PodStatusUpdate,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let container_statuses: Vec<Value> = update
            .container_statuses
            .iter()
            .map(|cs| {
                let state_obj = match cs.state.as_str() {
                    "running" => serde_json::json!({
                        "running": {"startedAt": &now}
                    }),
                    "terminated" => serde_json::json!({
                        "terminated": {"exitCode": 0, "finishedAt": &now}
                    }),
                    _ => serde_json::json!({
                        "waiting": {"reason": "ContainerCreating"}
                    }),
                };

                serde_json::json!({
                    "name": cs.name,
                    "state": state_obj,
                    "ready": cs.ready,
                    "restartCount": cs.restart_count,
                    "image": cs.image,
                    "imageID": cs.image_ref,
                    "containerID": format!("containerd://{}", cs.container_id)
                })
            })
            .collect();

        let mut conditions = vec![
            serde_json::json!({
                "type": "PodScheduled",
                "status": "True"
            }),
            serde_json::json!({
                "type": "Initialized",
                "status": "True"
            }),
        ];

        let all_ready = update.container_statuses.iter().all(|cs| cs.ready);
        conditions.push(serde_json::json!({
            "type": "ContainersReady",
            "status": if all_ready { "True" } else { "False" }
        }));
        conditions.push(serde_json::json!({
            "type": "Ready",
            "status": if all_ready { "True" } else { "False" }
        }));

        let mut status = serde_json::json!({
            "phase": &update.phase,
            "conditions": conditions,
            "containerStatuses": container_statuses,
            "hostIP": "127.0.0.1",
            "startTime": &now
        });

        if let Some(ref ip) = update.pod_ip {
            status["podIP"] = serde_json::json!(ip);
            status["podIPs"] = serde_json::json!([{"ip": ip}]);
        }

        // Fetch current pod, merge status, update
        let path = format!(
            "{}/api/v1/namespaces/{}/pods/{}",
            self.config.api_server_url, update.namespace, update.name
        );

        if let Ok(resp) = self.api_client.get(&path).send().await {
            if resp.status().is_success() {
                if let Ok(mut pod) = resp.json::<Value>().await {
                    pod["status"] = status;
                    let _ = self.api_client.put(&path).json(&pod).send().await;
                }
            }
        }

        Ok(())
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("NODE_NAME"))
        .unwrap_or_else(|_| "localhost".to_string())
}
