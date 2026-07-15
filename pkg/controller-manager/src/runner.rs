//! Controller manager — runs all controllers concurrently.

use crate::{cronjob, daemonset, deployment, gateway, hpa, job, migration, namespace, node, replicaset, service, statefulset};
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing::{info, warn};

/// HTTP client configuration for talking to the API server.
#[derive(Clone)]
pub struct ApiClient {
    pub base_url: String,
    pub client: reqwest::Client,
}

/// Connection + auth config for the API server (kubeconfig-style).
#[derive(Clone, Default)]
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
    leader_elect: bool,
    identity: String,
}

impl ControllerManager {
    fn make_identity() -> String {
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("NODE_NAME"))
            .unwrap_or_else(|_| "kube-controller-manager".to_string());
        format!("{host}_{}", std::process::id())
    }

    pub fn new(api_server_url: &str) -> Self {
        Self {
            api: Arc::new(ApiClient::new(api_server_url)),
            leader_elect: true,
            identity: Self::make_identity(),
        }
    }

    /// Connect to a (possibly HTTPS) apiserver with TLS + auth config.
    pub fn connect(api_server_url: &str, cfg: ClientConfig) -> anyhow::Result<Self> {
        Ok(Self {
            api: Arc::new(ApiClient::configured(api_server_url, cfg)?),
            leader_elect: true,
            identity: Self::make_identity(),
        })
    }

    /// Enable/disable leader election (upstream default: enabled).
    pub fn with_leader_election(mut self, enabled: bool) -> Self {
        self.leader_elect = enabled;
        self
    }

    /// Spawn all controllers into a JoinSet.
    fn spawn_all(&self) -> JoinSet<()> {
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

        let api = self.api.clone();
        tasks.spawn(async move {
            migration::MigrationController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            statefulset::StatefulSetController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            daemonset::DaemonSetController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            job::JobController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            cronjob::CronJobController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            hpa::HpaController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            gateway::GatewayController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            crate::gc::GarbageCollector::new(api).run().await;
        });

        info!("All controllers started (13 controllers)");
        tasks
    }

    /// Run the controller manager. With leader election enabled (default),
    /// controllers run only while this instance holds the lease; on losing it,
    /// they stop and the manager stands by to re-acquire.
    pub async fn run(&self) -> anyhow::Result<()> {
        if !self.leader_elect {
            info!("Starting controller manager (leader election disabled)");
            let mut tasks = self.spawn_all();
            while let Some(result) = tasks.join_next().await {
                if let Err(e) = result {
                    tracing::error!("Controller exited with error: {e}");
                }
            }
            return Ok(());
        }

        let elector = crate::leaderelection::LeaderElector::new(
            self.api.clone(),
            "kube-controller-manager",
            "kube-system",
            &self.identity,
        );
        info!("Leader election enabled (identity={})", self.identity);
        loop {
            elector.acquire().await;
            info!("Became leader; starting controllers");
            let mut tasks = self.spawn_all();
            loop {
                tokio::time::sleep(elector.retry_period()).await;
                if !elector.try_acquire_or_renew().await {
                    warn!("Lost leadership; stopping controllers");
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
    }
}
