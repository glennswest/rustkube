//! Controller manager — runs all controllers concurrently.

use crate::{deployment, namespace, node, replicaset, service};
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing::info;

/// HTTP client configuration for talking to the API server.
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

    /// GET a resource.
    pub async fn get(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
    }

    /// LIST resources (returns JSON body).
    pub async fn list(&self, path: &str) -> reqwest::Result<serde_json::Value> {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await?
            .json()
            .await
    }

    /// POST (create) a resource.
    pub async fn create(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> reqwest::Result<serde_json::Value> {
        self.client
            .post(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await?
            .json()
            .await
    }

    /// PUT (update) a resource.
    pub async fn update(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> reqwest::Result<serde_json::Value> {
        self.client
            .put(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await?
            .json()
            .await
    }

    /// PATCH a resource.
    pub async fn patch(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> reqwest::Result<serde_json::Value> {
        self.client
            .patch(format!("{}{}", self.base_url, path))
            .header("content-type", "application/strategic-merge-patch+json")
            .json(body)
            .send()
            .await?
            .json()
            .await
    }

    /// DELETE a resource.
    pub async fn delete(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        self.client
            .delete(format!("{}{}", self.base_url, path))
            .send()
            .await
    }
}

/// Controller manager — runs all controllers.
pub struct ControllerManager {
    api: Arc<ApiClient>,
}

impl ControllerManager {
    pub fn new(api_server_url: &str) -> Self {
        Self {
            api: Arc::new(ApiClient::new(api_server_url)),
        }
    }

    /// Start all controllers. Blocks forever.
    pub async fn run(&self) -> anyhow::Result<()> {
        info!("Starting controller manager");

        let mut tasks = JoinSet::new();

        let api = self.api.clone();
        tasks.spawn(async move {
            deployment::DeploymentController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            replicaset::ReplicaSetController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            service::ServiceController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            namespace::NamespaceController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            node::NodeLifecycleController::new(api).run().await;
        });

        info!("All controllers started");

        // Wait for any controller to exit (shouldn't happen)
        while let Some(result) = tasks.join_next().await {
            if let Err(e) = result {
                tracing::error!("Controller exited with error: {e}");
            }
        }

        Ok(())
    }
}
