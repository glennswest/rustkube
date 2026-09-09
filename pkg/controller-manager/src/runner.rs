//! Controller manager — runs all controllers concurrently.

use crate::{
    cronjob, daemonset, deployment, gateway, hpa, job, migration, namespace, node, pdb,
    persistentvolume, replicaset, service, statefulset, virtualmachine,
};
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

    /// Block until the apiserver answers, so a process started alongside it
    /// waits for it instead of failing.
    ///
    /// Returns as soon as `/readyz` is served. A connection that is refused is
    /// the apiserver not listening *yet*; a response that is not 2xx is an
    /// apiserver that is listening but not ready. Neither is fatal — after
    /// `timeout` this gives up waiting and returns anyway, leaving the caller's
    /// own retry loop to carry on, because a control-plane process that is up
    /// and reporting an unreachable apiserver is more useful than one that has
    /// exited.
    pub async fn wait_until_serving(&self, timeout: std::time::Duration) {
        let url = format!("{}/readyz", self.base_url);
        let start = std::time::Instant::now();
        let mut waiting = false;
        let mut last_report = start;
        loop {
            let why = match self.client.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    if waiting {
                        tracing::info!(
                            waited_secs = start.elapsed().as_secs_f32(),
                            "apiserver at {} is serving",
                            self.base_url,
                        );
                    }
                    return;
                }
                Ok(r) => format!("apiserver answered {} — listening, not ready", r.status()),
                Err(e) => format!("apiserver not reachable: {e}"),
            };
            if start.elapsed() >= timeout {
                tracing::warn!(
                    waited_secs = start.elapsed().as_secs(),
                    "{why}; continuing anyway and retrying in the background",
                );
                return;
            }
            if !waiting {
                tracing::info!(
                    url = %self.base_url,
                    timeout_secs = timeout.as_secs(),
                    "waiting for the apiserver: {why}",
                );
                waiting = true;
                last_report = std::time::Instant::now();
            } else if last_report.elapsed() >= std::time::Duration::from_secs(10) {
                tracing::warn!(
                    waited_secs = start.elapsed().as_secs(),
                    "still waiting for the apiserver: {why}",
                );
                last_report = std::time::Instant::now();
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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

    /// PUT a resource's **status only**, through the `/status` subresource.
    ///
    /// **Use this for every status write.** Writing status by PUTting the
    /// whole object sends a `spec` that was read some time ago, so a
    /// controller updating status silently reverts any spec change made since
    /// its last list — a Deployment controller scaling a ReplicaSet up and a
    /// ReplicaSet controller writing its status will fight, and the scale-up
    /// loses. The `/status` endpoint re-reads server-side, replaces only
    /// `status`, and applies the update with a compare-and-set on
    /// resourceVersion, so neither can happen.
    ///
    /// `path` is the resource's own path, without the `/status` suffix.
    pub async fn update_status(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> reqwest::Result<serde_json::Value> {
        self.client
            .put(format!("{}{}/status", self.base_url, path))
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

    /// PATCH with an RFC-7386 merge patch, which **replaces** lists rather
    /// than merging them by key.
    ///
    /// The difference matters for removal. A strategic merge patch merges
    /// `ownerReferences` by `uid` and `containers` by `name`, so sending a
    /// shorter list adds nothing and removes nothing — orphaning a dependent
    /// by strategic-merging its reference list away silently does nothing at
    /// all. Sending the intended list as a merge patch replaces it.
    pub async fn patch_merge(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> reqwest::Result<serde_json::Value> {
        self.client
            .patch(format!("{}{}", self.base_url, path))
            .header("content-type", "application/merge-patch+json")
            .json(body)
            .send()
            .await?
            .json()
            .await
    }

    /// DELETE a resource, carrying `meta/v1` DeleteOptions.
    ///
    /// The options are the difference between "delete this" and "delete this
    /// and everything under it, in order" — a DELETE without a body takes the
    /// server's default policy, which is Background, and a foreground cascade
    /// that propagates as Background stops being a foreground cascade one
    /// level down.
    pub async fn delete_with_options(
        &self,
        path: &str,
        options: &serde_json::Value,
    ) -> reqwest::Result<reqwest::Response> {
        self.client
            .delete(format!("{}{}", self.base_url, path))
            .header("content-type", "application/json")
            .json(options)
            .send()
            .await
    }
}

/// Controller manager — runs all controllers.
pub struct ControllerManager {
    api: Arc<ApiClient>,
    leader_elect: bool,
    identity: String,
    /// Cluster CA (cert PEM, key PEM) for signing approved CSRs.
    signing_ca: Option<(String, String)>,
    /// How long to wait for the apiserver to serve before running anyway.
    startup_timeout: std::time::Duration,
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
            signing_ca: None,
            startup_timeout: apimachinery::startup::DEFAULT_STARTUP_TIMEOUT,
        }
    }

    /// Connect to a (possibly HTTPS) apiserver with TLS + auth config.
    pub fn connect(api_server_url: &str, cfg: ClientConfig) -> anyhow::Result<Self> {
        Ok(Self {
            api: Arc::new(ApiClient::configured(api_server_url, cfg)?),
            leader_elect: true,
            identity: Self::make_identity(),
            signing_ca: None,
            startup_timeout: apimachinery::startup::DEFAULT_STARTUP_TIMEOUT,
        })
    }

    /// Enable/disable leader election (upstream default: enabled).
    pub fn with_leader_election(mut self, enabled: bool) -> Self {
        self.leader_elect = enabled;
        self
    }

    /// How long to wait for the apiserver to start serving before proceeding
    /// into the retry loop anyway.
    pub fn with_startup_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Provide the cluster CA (cert PEM, key PEM) so the CSR controller can sign
    /// approved requests. Without it, CSRs are approved but not signed.
    pub fn with_signing_ca(mut self, cert_pem: String, key_pem: String) -> Self {
        self.signing_ca = Some((cert_pem, key_pem));
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
        let api = self.api.clone();
        tasks.spawn(async move {
            virtualmachine::VirtualMachineController::new(api).run().await;
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
            pdb::PdbController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            gateway::GatewayController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            persistentvolume::PersistentVolumeController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            crate::attachdetach::AttachDetachController::new(api).run().await;
        });

        let api = self.api.clone();
        tasks.spawn(async move {
            crate::gc::GarbageCollector::new(api).run().await;
        });

        let api = self.api.clone();
        let ca = self.signing_ca.clone();
        tasks.spawn(async move {
            crate::csr::CsrController::new(api, ca).run().await;
        });

        info!("All controllers started (16 controllers)");
        tasks
    }

    /// Run the controller manager. With leader election enabled (default),
    /// controllers run only while this instance holds the lease; on losing it,
    /// they stop and the manager stands by to re-acquire.
    pub async fn run(&self) -> anyhow::Result<()> {
        // Prometheus /metrics + /healthz (scraped by ironprom), upstream :10257.
        crate::metrics_server::spawn(10257);

        // Started alongside the apiserver: wait for it rather than spraying
        // failed leases until it appears.
        self.api.wait_until_serving(self.startup_timeout).await;

        if !self.leader_elect {
            info!("Starting controller manager (leader election disabled)");
            crate::metrics_server::set_leader(true);
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
            crate::metrics_server::set_leader(true);
            let mut tasks = self.spawn_all();
            loop {
                tokio::time::sleep(elector.retry_period()).await;
                if !elector.try_acquire_or_renew().await {
                    warn!("Lost leadership; stopping controllers");
                    crate::metrics_server::set_leader(false);
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
    }
}
