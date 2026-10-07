//! Core scheduler loop.
//!
//! Dependency changes enqueue pods without a nodeName (and unplaced
//! VirtualMachineInstances), runs the fixed filter and score functions, then
//! binds each to the best node via the API server.

use crate::filter::{self, FilterResult, NodeUsage};
use crate::score;
use crate::virtualmachine;
use crate::volumebinding;
use apimachinery::informer::{Delta, Index, Key};
use apimachinery::informers::{Hub, Subscription};
use apimachinery::workqueue::WorkQueue;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::time::Duration;
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
    /// Where the client certificate and key were read from. When set, the
    /// files are followed and a renewed pair is presented from the next
    /// connection on (#105).
    pub client_files: Option<(std::path::PathBuf, std::path::PathBuf)>,
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
    pub watches: apimachinery::reactor::WatchHub,
    write_gate: apimachinery::lease::WriteGate,
    informers: Hub,
}

impl ApiClient {
    fn election_client(&self) -> Self {
        let mut client = self.clone();
        client.write_gate = Default::default();
        client
    }

    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            watches: Default::default(),
            write_gate: Default::default(),
            informers: Default::default(),
        }
    }

    /// Build a client with TLS + auth (for HTTPS apiservers / drop-in use).
    pub fn configured(base_url: &str, cfg: ClientConfig) -> anyhow::Result<Self> {
        let pair = match (&cfg.client_cert_pem, &cfg.client_key_pem) {
            (Some(cert), Some(key)) => Some((cert.as_slice(), key.as_slice())),
            _ => None,
        };
        let b = apimachinery::tls_reload::api_client_builder(
            cfg.ca_pem.as_deref(),
            cfg.insecure,
            pair,
            cfg.client_files.as_ref().map(|(c, k)| (c.as_path(), k.as_path())),
            cfg.token.as_deref(),
        )?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: b.build()?,
            watches: Default::default(),
            write_gate: Default::default(),
            informers: Default::default(),
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

    pub async fn list(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}{}", self.base_url, path);
        let segments: Vec<_> = path.trim_matches('/').split('/').collect();
        let discovery = path == "/apis"
            || path == "/api"
            || (segments.first() == Some(&"api") && segments.len() == 2)
            || (segments.first() == Some(&"apis") && segments.len() == 3);
        if discovery {
            self.watches.observe(
                &self.client,
                format!(
                    "{}/apis/apiextensions.k8s.io/v1/customresourcedefinitions",
                    self.base_url
                ),
            );
            let result = async {
                Ok(self
                    .client
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(30))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?)
            }
            .await;
            return apimachinery::reactor::check(result);
        }
        self.watches.observe(&self.client, url.clone());
        apimachinery::reactor::check(apimachinery::reflector::list(&self.client, &url).await)
    }

    pub async fn update(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let started = std::time::Instant::now();
        let budget = apimachinery::reactor::check(self.write_gate.budget())?;
        let result: reqwest::Result<serde_json::Value> = async {
            self.client
                .put(format!("{}{}", self.base_url, path))
                .timeout(budget)
                .json(body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
        }
        .await;
        if let Ok(value) = &result {
            self.informers
                .acknowledge(&format!("{}{}", self.base_url, path), value, started);
            if value["kind"] == "Status" && value["code"].as_u64().unwrap_or(0) >= 400 {
                apimachinery::reactor::failed();
            }
        }
        apimachinery::reactor::check(result.map_err(anyhow::Error::from))
    }

    /// Raw GET returning the response (so callers can distinguish 404).
    pub async fn get(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        if let Some((parent, _)) = path.split('?').next().unwrap_or(path).rsplit_once('/') {
            if parent.starts_with("/api/") || parent.starts_with("/apis/") {
                self.watches
                    .observe(&self.client, format!("{}{}", self.base_url, parent));
            }
        }
        let result: reqwest::Result<reqwest::Response> = async {
            self.client
                .get(format!("{}{}", self.base_url, path))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
        }
        .await;
        if let Ok(response) = &result {
            if !response.status().is_success() && response.status().as_u16() != 404 {
                apimachinery::reactor::failed();
            }
        }
        apimachinery::reactor::check(result)
    }

    /// PATCH a resource with a strategic-merge patch.
    pub async fn patch(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let started = std::time::Instant::now();
        let budget = apimachinery::reactor::check(self.write_gate.budget())?;
        let result: reqwest::Result<serde_json::Value> = async {
            self.client
                .patch(format!("{}{}", self.base_url, path))
                .timeout(budget)
                .header("content-type", "application/strategic-merge-patch+json")
                .json(body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
        }
        .await;
        if let Ok(value) = &result {
            self.informers
                .acknowledge(&format!("{}{}", self.base_url, path), value, started);
            if value["kind"] == "Status" && value["code"].as_u64().unwrap_or(0) >= 400 {
                apimachinery::reactor::failed();
            }
        }
        apimachinery::reactor::check(result.map_err(anyhow::Error::from))
    }

    /// PATCH with a **merge** patch (RFC 7386).
    ///
    /// Separate from `patch` because a CustomResourceDefinition does not
    /// accept a strategic-merge patch — strategic merge needs the Go struct
    /// tags that built-in types have and a CRD has not. rustkube-node's
    /// kubelet already patches a VMI's status this way; the scheduler writes
    /// to the same subresource and has to speak the same content type.
    pub async fn patch_merge(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let started = std::time::Instant::now();
        let budget = apimachinery::reactor::check(self.write_gate.budget())?;
        let result: reqwest::Result<serde_json::Value> = async {
            self.client
                .patch(format!("{}{}", self.base_url, path))
                .timeout(budget)
                .header("content-type", "application/merge-patch+json")
                .json(body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
        }
        .await;
        if let Ok(value) = &result {
            self.informers
                .acknowledge(&format!("{}{}", self.base_url, path), value, started);
            if value["kind"] == "Status" && value["code"].as_u64().unwrap_or(0) >= 400 {
                apimachinery::reactor::failed();
            }
        }
        apimachinery::reactor::check(result.map_err(anyhow::Error::from))
    }

    /// POST (create) returning the decoded body.
    pub async fn create(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
        let started = std::time::Instant::now();
        let budget = apimachinery::reactor::check(self.write_gate.budget())?;
        let result: reqwest::Result<serde_json::Value> = async {
            self.client
                .post(format!("{}{}", self.base_url, path))
                .timeout(budget)
                .json(body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
        }
        .await;
        if let Ok(value) = &result {
            self.informers
                .acknowledge(&format!("{}{}", self.base_url, path), value, started);
            if value["kind"] == "Status" && value["code"].as_u64().unwrap_or(0) >= 400 {
                apimachinery::reactor::failed();
            }
        }
        apimachinery::reactor::check(result.map_err(anyhow::Error::from))
    }
}

/// Best-effort node/pod identity for the leader-election Lease holder.
fn default_identity() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "kube-scheduler".to_string());
    format!("{host}_{}", uuid::Uuid::new_v4())
}

/// What the rest of the cluster looks like, for one scheduling pass.
///
/// Placement is not a property of a pod and a node alone: resource fit needs
/// what a node has already promised, and affinity, anti-affinity and topology
/// spread all need the pods that are already placed and where they landed.
/// Passing one object keeps those from being re-derived per plugin, and keeps
/// every plugin looking at the same snapshot.
#[derive(Debug, Default, Clone)]
pub struct ClusterState {
    /// Per node name: what the pods bound to it have requested.
    pub usage: std::collections::HashMap<String, NodeUsage>,
    /// Every non-terminal pod already bound, paired with the node it is on.
    pub placed: Vec<(String, Value)>,
}

impl ClusterState {
    /// What a node has already promised.
    pub fn used(&self, node: &Value) -> NodeUsage {
        self.usage
            .get(node_name_of(node))
            .copied()
            .unwrap_or_default()
    }
}

/// A node's name, for looking up what it has already promised.
fn node_name_of(node: &Value) -> &str {
    node["metadata"]["name"].as_str().unwrap_or("")
}

/// The scheduler — assigns unscheduled pods to nodes.
pub struct Scheduler {
    api: Arc<ApiClient>,
    leader_elect: bool,
    identity: String,
    /// How long to wait for the apiserver to serve before running anyway.
    startup_timeout: Duration,
    /// Last "no target" message written per migration uid (#184), so a
    /// migration that stays unschedulable is not re-listed and re-written on
    /// every placement change.
    migration_reports: Mutex<HashMap<String, String>>,
}

impl Scheduler {
    pub fn new(api_server_url: &str) -> Self {
        Self {
            api: Arc::new(ApiClient::new(api_server_url)),
            leader_elect: true,
            identity: default_identity(),
            startup_timeout: apimachinery::startup::DEFAULT_STARTUP_TIMEOUT,
            migration_reports: Mutex::default(),
        }
    }

    /// Connect with TLS + auth (HTTPS apiserver / mutual TLS or token).
    pub fn connect(api_server_url: &str, cfg: ClientConfig) -> anyhow::Result<Self> {
        Ok(Self {
            api: Arc::new(ApiClient::configured(api_server_url, cfg)?),
            leader_elect: true,
            identity: default_identity(),
            startup_timeout: apimachinery::startup::DEFAULT_STARTUP_TIMEOUT,
            migration_reports: Mutex::default(),
        })
    }

    /// Enable/disable leader election (default on, upstream behavior).
    pub fn with_leader_election(mut self, enabled: bool) -> Self {
        self.leader_elect = enabled;
        self
    }

    /// How long to wait for the apiserver to start serving before proceeding
    /// into the retry loop anyway.
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Run the scheduler. With leader election, only the elected leader schedules
    /// — so 3 masters can each run a kube-scheduler without double-binding.
    pub async fn run(&self) -> anyhow::Result<()> {
        // Prometheus /metrics + /healthz (scraped by ironprom), upstream :10259.
        crate::metrics_server::spawn(10259);

        // Started alongside the apiserver: wait for it rather than spraying
        // failed leases until it appears.
        self.api.wait_until_serving(self.startup_timeout).await;

        if !self.leader_elect {
            info!("Scheduler started (leader election disabled)");
            crate::metrics_server::set_leader(true);
            return self.scheduling_loop().await;
        }

        let elector = crate::leaderelection::LeaderElector::new(
            Arc::new(self.api.election_client()),
            "kube-scheduler",
            "kube-system",
            &self.identity,
        );
        info!(
            "Scheduler leader election enabled (identity={})",
            self.identity
        );
        crate::metrics_server::set_leader(false);
        loop {
            self.api.write_gate.close();
            let attempted = elector.acquire().await;
            self.api.write_gate.start(attempted);
            info!("Became leader; scheduling pods");
            crate::metrics_server::set_leader(true);
            // Lease maintenance is a timed obligation of its own, polled
            // alongside (never inside) scheduling; its end drops the whole
            // term — queue, feeds and in-memory reservations.
            let leadership = apimachinery::lease::hold(
                &self.api.write_gate,
                elector.retry_period(),
                || elector.try_acquire_or_renew(),
            );
            tokio::select! {
                biased;
                _ = leadership => {
                    warn!("Lost leadership; cancelling scheduling");
                    crate::metrics_server::set_leader(false);
                },
                result = self.scheduling_loop() => { result?; },
            }
        }
    }

    /// One serialized placement executor; acknowledged writes and outstanding
    /// assumptions charge capacity before another Pod or VMI may choose it.
    async fn scheduling_loop(&self) -> anyhow::Result<()> {
        let ready = WorkQueue::<ScheduleKey>::new();
        let observed = Arc::new(Mutex::new(SchedulingState::default()));
        let pods = scheduling_feed(&self.api, "/api/v1/pods", false, &ready, &observed);
        let wake = ready.clone();
        let state = observed.clone();
        let crds = self.api.informers.subscribe(
            &self.api.client,
            format!(
                "{}/apis/apiextensions.k8s.io/v1/customresourcedefinitions",
                self.api.base_url
            ),
            move |_, _| {
                let keys: Vec<_> = state.lock().unwrap().pending.keys().cloned().collect();
                for key in keys {
                    wake.add(key);
                }
                wake.add((
                    false,
                    Key {
                        namespace: "".into(),
                        name: "".into(),
                        uid: "discovery".into(),
                    },
                ));
            },
        );
        let paths = [
            "/api/v1/nodes",
            "/api/v1/persistentvolumeclaims",
            "/api/v1/persistentvolumes",
            "/apis/storage.k8s.io/v1/storageclasses",
            "/apis/storage.k8s.io/v1/csidrivers",
            "/apis/storage.k8s.io/v1/csistoragecapacities",
        ];
        let dependencies: Vec<_> = paths
            .iter()
            .map(|path| {
                let wake = ready.clone();
                let state = observed.clone();
                let pod_feed = pods.feed.clone();
                let is_claim = *path == "/api/v1/persistentvolumeclaims";
                self.api.informers.subscribe(
                    &self.api.client,
                    format!("{}{}", self.api.base_url, path),
                    move |changes, reset| {
                        if is_claim && !reset {
                            for delta in changes {
                                for claim in delta.old.iter().chain(delta.new.iter()) {
                                    let ns = claim["metadata"]["namespace"].as_str().unwrap_or("");
                                    let name = claim["metadata"]["name"].as_str().unwrap_or("");
                                    for pod in pod_feed
                                        .select(&Index::Claim(ns.into(), name.into()))
                                        .unwrap_or_default()
                                    {
                                        if let Ok(key) = Key::of(&pod) {
                                            wake.add((false, key));
                                        }
                                    }
                                }
                            }
                        } else {
                            let keys: Vec<_> =
                                state.lock().unwrap().pending.keys().cloned().collect();
                            for key in keys {
                                wake.add(key);
                            }
                        }
                    },
                )
            })
            .collect();
        let mut vmis: Option<Subscription> = None;
        let mut failures: HashMap<ScheduleKey, u32> = HashMap::new();
        // Bind writes in flight. Placement stays serialized — each choice is
        // reserved before its write, so the next Pod sees the capacity — but
        // the next Pod no longer waits for the previous one's write (#190).
        // Each bind holds its Work, so its Pod is not placed again until the
        // write has answered; dropping the set with the loop (lost
        // leadership) aborts them, as dropping the loop aborted the one
        // inline write before.
        let mut binds: tokio::task::JoinSet<BindDone> = tokio::task::JoinSet::new();
        loop {
            let work = tokio::select! {
                biased;
                Some(done) = binds.join_next(), if !binds.is_empty() => {
                    let Ok(done) = done else { continue };
                    let BindDone { work, node, queued, ok } = done;
                    let key = work.key().clone();
                    if ok {
                        failures.remove(&key);
                        let waited = queued.map(|at| at.elapsed());
                        if let Some(waited) = waited {
                            crate::metrics_server::record_e2e_latency(
                                waited.as_secs_f64(),
                                "scheduled",
                            );
                        }
                        info!(
                            ?key,
                            %node,
                            ms = waited.map(|w| w.as_secs_f64() * 1000.0),
                            "workload bound"
                        );
                        crate::metrics_server::record_attempt("scheduled");
                        // `Scheduled`, as upstream's default-scheduler
                        // records it (#138) — off the loop: the bind is done.
                        if !key.0 {
                            let pod = json!({"metadata": {"namespace": key.1.namespace,
                                "name": key.1.name, "uid": key.1.uid}});
                            let api = self.api.clone();
                            tokio::spawn(async move {
                                let ev = crate::events::scheduled(&pod, &node);
                                let path = format!("/api/v1/namespaces/{}/events",
                                    pod["metadata"]["namespace"].as_str().unwrap_or("default"));
                                if let Err(e) = api.create(&path, &ev).await {
                                    debug!("could not record Scheduled for {}: {e}", pod["metadata"]["name"]);
                                }
                            });
                        }
                    } else {
                        crate::metrics_server::record_attempt("error");
                        let n = failures.entry(key.clone()).or_default();
                        *n = n.saturating_add(1);
                        ready.add_at(
                            key,
                            tokio::time::Instant::now()
                                + Duration::from_millis((100_u64 << (*n).min(8)).min(30_000)),
                        );
                    }
                    drop(work);
                    continue;
                }
                work = ready.next_by(|a, b| {
                    let state = observed.lock().unwrap();
                    let a = state.pending.get(a);
                    let b = state.pending.get(b);
                    b.map(pod_priority)
                        .unwrap_or(0)
                        .cmp(&a.map(pod_priority).unwrap_or(0))
                        .then_with(|| a.map(creation_ts).cmp(&b.map(creation_ts)))
                }), if binds.len() < MAX_BINDS_IN_FLIGHT => work,
            };
            ready.cancel_deadline(work.key());
            let key = work.key().clone();
            let wake = ready.clone();
            let retry = key.clone();
            let (result, failed) = apimachinery::reactor::scope_object(
                move |delay| {
                    wake.add_at(retry.clone(), tokio::time::Instant::now() + delay);
                },
                async {
                    crds.feed.ensure_synced()?;
                    if vmis.is_none()
                        && !crds
                            .feed
                            .select(&Index::Name(
                                "".into(),
                                "virtualmachineinstances.kubevirt.io".into(),
                            ))?
                            .is_empty()
                    {
                        vmis = Some(scheduling_feed(
                            &self.api,
                            virtualmachine::LIST_PATH,
                            true,
                            &ready,
                            &observed,
                        ));
                    }
                    pods.feed.ensure_synced()?;
                    for feed in &dependencies {
                        feed.feed.ensure_synced()?;
                    }
                    if let Some(feed) = &vmis {
                        feed.feed.ensure_synced()?;
                    }
                    let source = if key.0 {
                        vmis.as_ref().map(|f| &f.feed)
                    } else {
                        Some(&pods.feed)
                    };
                    let Some(object) = source.map(|feed| feed.get(&key.1)).transpose()?.flatten()
                    else {
                        return Ok::<Option<Bind>, anyhow::Error>(None);
                    };
                    observed.lock().unwrap().observe(
                        key.0,
                        &Delta {
                            old: None,
                            new: Some(object.clone()),
                            affected: Default::default(),
                        },
                    );
                    if !pending_workload(key.0, &object) {
                        return Ok(None);
                    }
                    let mut nodes = dependencies[0].feed.list()?;
                    let (mut state, assumed) = observed.lock().unwrap().snapshot(&key);
                    if let Some(node) = assumed {
                        nodes.retain(|n| n["metadata"]["name"] == node);
                    }
                    if key.0 && virtualmachine::node_of(&object).is_some() {
                        // Placed and pending: a migration wants its target (#184).
                        self.schedule_migration_target(&object, &nodes, &state, &observed, &key)
                            .await;
                    } else if key.0 {
                        self.schedule_virtual_machine(&object, &nodes, &mut state, &observed, &key)
                            .await;
                    } else {
                        let volumes = indexed_volume_state(&object, &dependencies)?;
                        let ns = object["metadata"]["namespace"]
                            .as_str()
                            .unwrap_or("default");
                        match self
                            .schedule_pod(ns, &object, &nodes, &state, &volumes, &observed, &key)
                            .await
                        {
                            Ok(Placement::Bind(bind)) => return Ok(Some(bind)),
                            Ok(Placement::WaitingForVolumes(node)) => {
                                observed
                                    .lock()
                                    .unwrap()
                                    .reserve(key.clone(), &object, &node, true);
                                crate::metrics_server::record_attempt("unschedulable");
                            }
                            Ok(Placement::Unschedulable(why)) => {
                                crate::metrics_server::record_attempt("unschedulable");
                                debug!(%why, ?key, "pod not placed");
                                self.report_pod_unschedulable(ns, &object, &why).await;
                            }
                            Err(error) => {
                                debug!(%error,?key,"workload not placed");
                            }
                        }
                    }
                    Ok(None)
                },
            )
            .await;
            if let (Ok(Some(bind)), false) = (&result, failed) {
                let api = self.api.clone();
                let wake = ready.clone();
                let retry = key.clone();
                let Bind { node, path, body } = bind.clone();
                // Read before the write: the bound Pod's watch event clears it.
                let queued = observed.lock().unwrap().queued.get(&key).copied();
                binds.spawn(async move {
                    let (result, failed) = apimachinery::reactor::scope_object(
                        move |delay| {
                            wake.add_at(retry.clone(), tokio::time::Instant::now() + delay);
                        },
                        api.update(&path, &body),
                    )
                    .await;
                    if let Err(error) = &result {
                        debug!(%error, %path, "bind not written");
                    }
                    BindDone {
                        work,
                        node,
                        queued,
                        ok: result.is_ok() && !failed,
                    }
                });
                continue;
            }
            if result.is_err() || failed {
                let n = failures.entry(key.clone()).or_default();
                *n = n.saturating_add(1);
                ready.add_at(
                    key,
                    tokio::time::Instant::now()
                        + Duration::from_millis((100_u64 << (*n).min(8)).min(30_000)),
                );
            } else {
                failures.remove(&key);
            }
            drop(work);
        }
    }

    /// Place one VM, or say why it cannot be placed.
    ///
    /// Takes the state by mutable reference so a machine it places is charged
    /// to its node before the next machine is considered.
    async fn schedule_virtual_machine(
        &self,
        vmi: &Value,
        nodes: &[Value],
        state: &mut ClusterState,
        observed: &Mutex<SchedulingState>,
        key: &ScheduleKey,
    ) {
        let name = vmi["metadata"]["name"].as_str().unwrap_or("");
        let ns = vmi["metadata"]["namespace"].as_str().unwrap_or("default");
        if name.is_empty() {
            warn!("a VirtualMachineInstance with no name cannot be scheduled");
            return;
        }
        let shim = virtualmachine::scheduling_shim(vmi);

        // The same filters and the same scores a pod gets. That is the whole
        // point of the shim: taints, selectors, affinity, spread and resource
        // fit apply to a VM the day they are written, rather than being
        // reimplemented for machines and drifting from the pod path.
        let mut refused: Vec<String> = Vec::new();
        let feasible: Vec<&Value> = nodes
            .iter()
            .filter(
                |node| match filter::run_filters(&shim, node, state.used(node), state, nodes) {
                    FilterResult::Pass => true,
                    FilterResult::Fail(reason) => {
                        let n = node["metadata"]["name"].as_str().unwrap_or("?");
                        refused.push(format!("{n}: {reason}"));
                        false
                    }
                },
            )
            .collect();

        if feasible.is_empty() {
            // Said where somebody will see it. A VM that never starts and
            // never explains itself is the half of this bug that made it hard
            // to find: `kubectl get vmi` showed no node, no phase and no
            // reason, and the only trace was the absence of one.
            let why = if refused.is_empty() {
                "no nodes are registered".to_string()
            } else {
                refused.join("; ")
            };
            self.report_unschedulable(ns, name, vmi, &why).await;
            crate::metrics_server::record_attempt("unschedulable");
            debug!("No node can run VirtualMachineInstance {ns}/{name}: {why}");
            return;
        }

        let mut scored: Vec<(&Value, i64)> = feasible
            .iter()
            .map(|node| {
                (
                    *node,
                    score::score_node(&shim, node, state.used(node), state, nodes),
                )
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        let chosen = scored[0].0["metadata"]["name"].as_str().unwrap_or("");
        if chosen.is_empty() {
            warn!("the chosen node for {ns}/{name} has no name");
            return;
        }

        // `status.nodeName`, through the status subresource. Writing
        // `spec.nodeName` instead would be editing what the user declared,
        // and the kubelet reads status first for exactly that reason.
        let mut status = json!({"nodeName": chosen});
        // Phase only when there is nothing there yet: an unplaced VM cannot
        // be Running, but stamping Pending over whatever the kubelet may have
        // written is not this component's business.
        match vmi["status"]["phase"].as_str() {
            None | Some("") => status["phase"] = json!("Pending"),
            _ => {}
        }
        let body = json!({"metadata": {
            "uid": vmi["metadata"]["uid"], "resourceVersion": vmi["metadata"]["resourceVersion"]
        }, "status": status});
        observed
            .lock()
            .unwrap()
            .reserve(key.clone(), vmi, chosen, false);
        match self
            .api
            .patch_merge(&virtualmachine::status_path(ns, name), &body)
            .await
        {
            Ok(_) => {
                // Charged now, not next pass: the machine after this one must
                // see the memory this one just took.
                let (cpu, mem) = virtualmachine::requests(vmi);
                let e = state.usage.entry(chosen.to_string()).or_default();
                e.cpu_milli += cpu;
                e.mem_bytes += mem;
                state.placed.push((chosen.to_string(), shim));
                crate::metrics_server::record_attempt("scheduled");
                info!("Scheduled VirtualMachineInstance {ns}/{name} -> {chosen}");
            }
            Err(e) => {
                crate::metrics_server::record_attempt("error");
                error!("Could not place {ns}/{name} on {chosen}: {e}");
            }
        }
    }

    /// Choose where a migrating VMI goes (#184): the filters and scores its
    /// placement gets, the node it is on excluded, written as
    /// `status.migrationState.targetNode` — the migration's equivalent of a
    /// bind. The target is charged from the moment it is chosen until the
    /// migration ends, so nothing else is promised the memory the machine is
    /// moving into.
    async fn schedule_migration_target(
        &self,
        vmi: &Value,
        nodes: &[Value],
        state: &ClusterState,
        observed: &Mutex<SchedulingState>,
        key: &ScheduleKey,
    ) {
        let name = vmi["metadata"]["name"].as_str().unwrap_or("");
        let ns = vmi["metadata"]["namespace"].as_str().unwrap_or("default");
        let source = virtualmachine::node_of(vmi).unwrap_or("");
        let uid = vmi["status"]["migrationState"]["migrationUid"].as_str().unwrap_or("").to_string();
        let shim = virtualmachine::scheduling_shim(vmi);
        let (chosen, refused) = choose_migration_target(&shim, source, nodes, state);
        let Some(chosen) = chosen else {
            let why = if refused.is_empty() {
                "no other node is registered".to_string()
            } else {
                refused.join("; ")
            };
            crate::metrics_server::record_attempt("unschedulable");
            debug!("No node can take migrating VirtualMachineInstance {ns}/{name}: {why}");
            self.report_migration(ns, &uid, false, &format!("no node can take this migration: {why}"))
                .await;
            return;
        };
        let body = json!({
            "metadata": {"uid": vmi["metadata"]["uid"], "resourceVersion": vmi["metadata"]["resourceVersion"]},
            "status": {"migrationState": {"targetNode": chosen}},
        });
        observed.lock().unwrap().reserve_target(key.clone(), vmi, &chosen);
        match self.api.patch_merge(&virtualmachine::status_path(ns, name), &body).await {
            Ok(_) => {
                crate::metrics_server::record_attempt("scheduled");
                info!("Migration target for VirtualMachineInstance {ns}/{name}: {source} -> {chosen}");
                self.report_migration(ns, &uid, true, &format!("target node {chosen}")).await;
            }
            Err(e) => {
                observed.lock().unwrap().target_assumptions.remove(key);
                crate::metrics_server::record_attempt("error");
                error!("Could not record migration target {chosen} for {ns}/{name}: {e}");
            }
        }
    }

    /// Say on the VirtualMachineInstanceMigration whether its target could be
    /// placed: condition `TargetScheduled`. Written when the message changes;
    /// a scheduled one only if an unschedulable one was written before.
    async fn report_migration(&self, ns: &str, migration_uid: &str, scheduled: bool, message: &str) {
        if migration_uid.is_empty() {
            return;
        }
        {
            let mut reports = self.migration_reports.lock().unwrap();
            let last = reports.get(migration_uid);
            if last.map(String::as_str) == Some(message) || (scheduled && last.is_none()) {
                return;
            }
            if scheduled {
                reports.remove(migration_uid);
            } else {
                reports.insert(migration_uid.to_string(), message.to_string());
            }
        }
        let list = match self
            .api
            .list(&format!("/apis/kubevirt.io/v1/namespaces/{ns}/virtualmachineinstancemigrations"))
            .await
        {
            Ok(l) => l,
            Err(e) => {
                debug!("could not find the migration {migration_uid}: {e}");
                return;
            }
        };
        let Some(name) = list["items"].as_array().and_then(|items| {
            items
                .iter()
                .find(|m| m["metadata"]["uid"].as_str() == Some(migration_uid))
                .and_then(|m| m["metadata"]["name"].as_str().map(str::to_string))
        }) else {
            return;
        };
        let body = json!({"status": {"conditions": [{
            "type": "TargetScheduled",
            "status": if scheduled { "True" } else { "False" },
            "reason": if scheduled { "Scheduled" } else { "Unschedulable" },
            "message": message,
            "lastTransitionTime": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        }]}});
        if let Err(e) = self
            .api
            .patch_merge(
                &format!("/apis/kubevirt.io/v1/namespaces/{ns}/virtualmachineinstancemigrations/{name}/status"),
                &body,
            )
            .await
        {
            debug!("could not report on migration {ns}/{name}: {e}");
        }
    }

    /// Record why a VM could not be placed, without rewriting it every second.
    ///
    /// The loop runs at 1 Hz. A VM that cannot be scheduled stays that way
    /// for as long as the cluster is full, and patching it on every pass
    /// would be a write per second per stuck VM for hours — so the message is
    /// only sent when it differs from the one already there.
    async fn report_unschedulable(&self, ns: &str, name: &str, vmi: &Value, why: &str) {
        let message = format!("no node can run this VM: {why}");
        let unchanged = vmi["status"]["message"].as_str() == Some(message.as_str())
            && vmi["status"]["reason"].as_str() == Some("Unschedulable");
        if unchanged {
            return;
        }
        let body = json!({"status": {
            "phase": "Pending",
            "reason": "Unschedulable",
            "message": message,
        }});
        if let Err(e) = self
            .api
            .patch_merge(&virtualmachine::status_path(ns, name), &body)
            .await
        {
            debug!("could not report that {ns}/{name} is unschedulable: {e}");
        }
    }

    /// Set `PodScheduled=False`, reason `Unschedulable`, as upstream does,
    /// so `kubectl describe` says why a Pod is Pending (#194). Written only
    /// when the condition differs: every placement change re-tries every
    /// pending Pod, and a write per retry would be a write per pending Pod
    /// per finished Pod. The bind replaces it with `PodScheduled=True`.
    async fn report_pod_unschedulable(&self, ns: &str, pod: &Value, why: &str) {
        let name = pod["metadata"]["name"].as_str().unwrap_or("");
        let current = pod["status"]["conditions"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["type"] == "PodScheduled"));
        if current.is_some_and(|c| {
            c["status"] == "False" && c["reason"] == "Unschedulable" && c["message"] == why
        }) {
            return;
        }
        // The transition time moves only when the status does.
        let since = current
            .filter(|c| c["status"] == "False")
            .and_then(|c| c["lastTransitionTime"].as_str().map(str::to_string))
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
        let body = json!({
            "metadata": {"uid": pod["metadata"]["uid"]},
            "status": {"conditions": [{
                "type": "PodScheduled",
                "status": "False",
                "reason": "Unschedulable",
                "message": why,
                "lastTransitionTime": since,
            }]},
        });
        if let Err(e) = self
            .api
            .patch(&format!("/api/v1/namespaces/{ns}/pods/{name}/status"), &body)
            .await
        {
            debug!("could not report that {ns}/{name} is unschedulable: {e}");
        }
        // `FailedScheduling` with the same message, written exactly when the
        // condition is — when the reason changes — so a Pod waiting for
        // capacity is one Event per distinct reason, not one per retry (#138).
        let ev = crate::events::failed_scheduling(pod, why);
        if let Err(e) = self.api.create(&format!("/api/v1/namespaces/{ns}/events"), &ev).await {
            debug!("could not record FailedScheduling for {ns}/{name}: {e}");
        }
    }

    async fn schedule_pod(
        &self,
        namespace: &str,
        pod: &Value,
        nodes: &[Value],
        state: &ClusterState,
        volumes: &volumebinding::VolumeState,
        observed: &Mutex<SchedulingState>,
        key: &ScheduleKey,
    ) -> anyhow::Result<Placement> {
        let pod_name = pod["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("pod missing name"))?;

        // Phase 0: a ReadWriteOncePod claim somebody else holds.
        //
        // Before any node is looked at, because it is not a property of nodes:
        // if another pod holds the claim then no node will do, and running the
        // per-node filters first would report "no node was suitable" for a pod
        // that was never placeable anywhere (#65).
        if let Some(reason) = volumebinding::rwop_conflict(pod, namespace, volumes, &state.placed) {
            return Ok(Placement::Unschedulable(reason));
        }

        // Phase 1: Filter — find nodes that can run this pod, keeping why
        // each other node was refused for the Pod's PodScheduled condition.
        let mut refused: Vec<String> = Vec::new();
        let feasible: Vec<&Value> = nodes
            .iter()
            .filter(|node| {
                // Pod count after the others, as upstream's NodeResourcesFit
                // follows NodeAffinity: a full node the Pod's selector rules
                // out anyway is reported as the selector, not as full.
                let used = state.used(node);
                let result = match filter::run_filters(pod, node, used, state, nodes) {
                    FilterResult::Pass => filter::pod_count_filter(node, used),
                    fail => fail,
                };
                if let FilterResult::Fail(reason) = result {
                    refused.push(reason);
                    return false;
                }
                // Storage last: it is the filter that needs the extra listing,
                // and there is no point paying for it on a node that has
                // already been ruled out on CPU.
                match volumebinding::filter_node(pod, namespace, node, volumes) {
                    Ok(()) => true,
                    Err(reason) => {
                        debug!("node rejected for {namespace}/{pod_name}: {reason}");
                        refused.push(reason.to_string());
                        false
                    }
                }
            })
            .collect();

        if feasible.is_empty() {
            return Ok(Placement::Unschedulable(unschedulable_message(
                nodes.len(),
                &refused,
            )));
        }

        // Phase 2: Score — rank feasible nodes
        let mut scored: Vec<(&Value, i64)> = feasible
            .iter()
            .map(|node| {
                (
                    *node,
                    score::score_node(pod, node, state.used(node), state, nodes),
                )
            })
            .collect();

        // Sort by score descending
        scored.sort_by(|a, b| b.1.cmp(&a.1));

        let chosen = scored[0].0;
        let chosen_name = chosen["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("node missing name"))?;

        // Phase 3a: volumes before the pod.
        //
        // A `WaitForFirstConsumer` claim is provisioned where the pod is going
        // to run, so the node has to be recorded on the claim first — and the
        // pod must *not* be bound until the volume exists, or the kubelet
        // starts a pod whose mount cannot succeed yet. The next pass binds it.
        let unbound = volumebinding::unbound_claims(pod, namespace, volumes);
        if !unbound.is_empty() {
            for claim in &unbound {
                let path = format!("/api/v1/namespaces/{namespace}/persistentvolumeclaims/{claim}");
                let already = volumes
                    .claim(namespace, claim)
                    .and_then(|c| {
                        c["metadata"]["annotations"][volumebinding::ANN_SELECTED_NODE].as_str()
                    })
                    .unwrap_or("");
                if already == chosen_name {
                    continue;
                }
                let observed = volumes
                    .claim(namespace, claim)
                    .ok_or_else(|| anyhow::anyhow!("claim {namespace}/{claim} disappeared"))?;
                let patch = json!({"metadata": {
                    "uid": observed["metadata"]["uid"],
                    "resourceVersion": observed["metadata"]["resourceVersion"],
                    "annotations": {
                    volumebinding::ANN_SELECTED_NODE: chosen_name
                }}});
                if let Err(e) = self.api.patch(&path, &patch).await {
                    return Err(anyhow::anyhow!(
                        "could not select node {chosen_name} for claim {namespace}/{claim}: {e}"
                    ));
                }
                info!("Claim {namespace}/{claim} will be provisioned on {chosen_name}");
            }
            return Ok(Placement::WaitingForVolumes(chosen_name.to_string()));
        }

        // Phase 3b: Bind — update the pod with the chosen node.
        //
        // Charged before the write, whose outcome may be unknown; the claim
        // path above charges only once selected-node is written (the caller's
        // WaitingForVolumes reservation). Reserving before a claim that is
        // missing, or whose patch failed, pinned the Pod to that first choice
        // (`snapshot` restricts it to the assumed node) — even after the claim
        // appeared with a volume that lives elsewhere.
        observed
            .lock()
            .unwrap()
            .reserve(key.clone(), pod, chosen_name, false);
        let mut bound_pod = pod.clone();
        let now = chrono::Utc::now();
        bound_pod["spec"]["nodeName"] = json!(chosen_name);
        // PodScheduled's time is a metav1.Time, whole seconds as upstream
        // serializes it; a kubelet timing a subsecond start from it measured
        // the truncation (#190). The annotation is the same instant to the
        // microsecond.
        if !bound_pod["metadata"]["annotations"].is_object() {
            bound_pod["metadata"]["annotations"] = json!({});
        }
        bound_pod["metadata"]["annotations"][SCHEDULED_AT] =
            json!(now.to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
        bound_pod["status"]["phase"] = json!("Pending");
        bound_pod["status"]["conditions"] = json!([
            {
                "type": "PodScheduled",
                "status": "True",
                "reason": "Scheduled",
                "message": format!("Bound to node {chosen_name}"),
                "lastTransitionTime": now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
            }
        ]);

        Ok(Placement::Bind(Bind {
            node: chosen_name.to_string(),
            path: format!("/api/v1/namespaces/{namespace}/pods/{pod_name}"),
            body: bound_pod,
        }))
    }
}

/// Upstream's FailedScheduling shape: `0/3 nodes are available: 1 Too many
/// pods, 2 node is not Ready.` Reasons are counted, most common first, so the
/// message stays the same while the cluster does.
fn unschedulable_message(nodes: usize, refused: &[String]) -> String {
    if nodes == 0 {
        return "0/0 nodes are available: no nodes are registered.".into();
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for reason in refused {
        match counts.iter_mut().find(|(r, _)| r == reason) {
            Some((_, n)) => *n += 1,
            None => counts.push((reason.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let parts: Vec<String> = counts.iter().map(|(r, n)| format!("{n} {r}")).collect();
    format!("0/{nodes} nodes are available: {}.", parts.join(", "))
}

/// The best node for a migrating VMI's target other than `source`, and why
/// each other node was refused.
fn choose_migration_target(
    shim: &Value,
    source: &str,
    nodes: &[Value],
    state: &ClusterState,
) -> (Option<String>, Vec<String>) {
    let mut refused = Vec::new();
    let mut best: Option<(&Value, i64)> = None;
    for node in nodes {
        let n = node_name_of(node);
        if n == source {
            refused.push(format!("{n}: the VM is already there"));
            continue;
        }
        match filter::run_filters(shim, node, state.used(node), state, nodes) {
            FilterResult::Pass => {
                let score = score::score_node(shim, node, state.used(node), state, nodes);
                if best.is_none_or(|(_, b)| score > b) {
                    best = Some((node, score));
                }
            }
            FilterResult::Fail(reason) => refused.push(format!("{n}: {reason}")),
        }
    }
    (best.map(|(n, _)| node_name_of(n).to_string()).filter(|n| !n.is_empty()), refused)
}

/// Annotation on a bound Pod: when the scheduler wrote the binding, RFC3339
/// to the microsecond (`PodScheduled`'s lastTransitionTime is whole seconds).
pub const SCHEDULED_AT: &str = "storm.io/scheduled-at";

type ScheduleKey = (bool, Key); // false = Pod, true = VMI; one shared executor
struct Assumption {
    node: String,
    object: Value,
    waiting_for_volume: bool,
}
#[derive(Default)]
struct SchedulingState {
    pending: HashMap<ScheduleKey, Value>,
    placed: HashMap<ScheduleKey, (String, Value)>,
    usage: HashMap<String, NodeUsage>,
    assumptions: HashMap<ScheduleKey, Assumption>,
    /// Migrating VMIs charged to their target node as well (#184).
    targets: HashMap<ScheduleKey, (String, Value)>,
    /// Targets chosen whose write has not been seen back yet.
    target_assumptions: HashMap<ScheduleKey, (String, Value)>,
    /// When each pending workload was first seen pending, for
    /// `scheduler_e2e_scheduling_duration_seconds`.
    queued: HashMap<ScheduleKey, std::time::Instant>,
}
/// Charge a placed workload to its node: its requests, and a Pod slot
/// unless it is a VMI (#194).
fn charge(usage: &mut HashMap<String, NodeUsage>, vm: bool, node: String, pod: &Value) {
    let (cpu, mem) = filter::pod_requests(pod);
    let used = usage.entry(node).or_default();
    used.cpu_milli += cpu;
    used.mem_bytes += mem;
    used.pods += u64::from(!vm);
}
/// The inverse of `charge`.
fn uncharge(usage: &mut HashMap<String, NodeUsage>, vm: bool, node: String, pod: &Value) {
    let (cpu, mem) = filter::pod_requests(pod);
    let used = usage.entry(node).or_default();
    used.cpu_milli -= cpu;
    used.mem_bytes -= mem;
    used.pods -= u64::from(!vm);
}
fn pending_workload(vm: bool, object: &Value) -> bool {
    if !object["metadata"]["deletionTimestamp"].is_null() {
        return false;
    }
    if vm {
        !virtualmachine::is_terminal(object)
            && (virtualmachine::node_of(object).is_none()
                || apimachinery::kubevirt::wants_migration_target(object))
    } else {
        !matches!(
            object["status"]["phase"].as_str(),
            Some("Succeeded" | "Failed")
        ) && object["spec"]["nodeName"]
            .as_str()
            .is_none_or(|n| n.is_empty())
    }
}
fn placed_workload(vm: bool, object: &Value) -> Option<(String, Value)> {
    if vm {
        if virtualmachine::is_terminal(object) {
            return None;
        }
        virtualmachine::node_of(object).map(|n| (n.into(), virtualmachine::scheduling_shim(object)))
    } else {
        if matches!(
            object["status"]["phase"].as_str(),
            Some("Succeeded" | "Failed")
        ) {
            return None;
        }
        object["spec"]["nodeName"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(|n| (n.into(), object.clone()))
    }
}
/// A migrating VMI's target node, charged like a second placement (#184):
/// from when it is chosen until the migration fails, or succeeds *and* the
/// controller has moved `status.nodeName` there. Between the source's
/// `completed` and that move the machine is running on the target while the
/// VMI still names the source; releasing the target then would let another
/// workload take the memory the machine is in.
fn migration_target(vm: bool, object: &Value) -> Option<(String, Value)> {
    if !vm || virtualmachine::is_terminal(object) {
        return None;
    }
    let state = &object["status"]["migrationState"];
    let target = state["targetNode"].as_str().filter(|n| !n.is_empty())?;
    if state["migrationUid"].as_str().is_none_or(str::is_empty)
        || state["failed"].as_bool() == Some(true)
        || (state["completed"].as_bool() == Some(true) && virtualmachine::node_of(object) == Some(target))
    {
        return None;
    }
    Some((target.to_string(), virtualmachine::scheduling_shim(object)))
}
impl SchedulingState {
    fn remove(&mut self, key: &ScheduleKey) {
        self.pending.remove(key);
        self.queued.remove(key);
        self.assumptions.remove(key);
        self.target_assumptions.remove(key);
        if let Some((node, pod)) = self.placed.remove(key) {
            uncharge(&mut self.usage, key.0, node, &pod);
        }
        if let Some((node, pod)) = self.targets.remove(key) {
            uncharge(&mut self.usage, key.0, node, &pod);
        }
    }
    fn observe(&mut self, vm: bool, delta: &Delta) {
        if let Some(old) = &delta.old {
            if let Ok(key) = Key::of(old) {
                if delta.new.as_ref().and_then(|o| Key::of(o).ok()).as_ref() != Some(&key) {
                    self.remove(&(vm, key));
                }
            }
        }
        if let Some(object) = &delta.new {
            let Ok(key) = Key::of(object) else {
                return;
            };
            let key = (vm, key);
            if let Some((node, old)) = self.placed.remove(&key) {
                uncharge(&mut self.usage, vm, node, &old);
            }
            self.pending.remove(&key);
            if pending_workload(vm, object) {
                self.queued
                    .entry(key.clone())
                    .or_insert_with(std::time::Instant::now);
                self.pending.insert(key.clone(), object.clone());
                if self.assumptions.get(&key).is_some_and(|a| {
                    !a.waiting_for_volume
                        && a.object["metadata"]["resourceVersion"]
                            != object["metadata"]["resourceVersion"]
                }) {
                    // A later durable object version fences any delayed CAS bind
                    // issued with the assumed version, so its reservation can go.
                    self.assumptions.remove(&key);
                }
            } else {
                self.assumptions.remove(&key);
                self.queued.remove(&key);
            }
            if let Some((node, old)) = self.targets.remove(&key) {
                uncharge(&mut self.usage, vm, node, &old);
            }
            let target = migration_target(vm, object);
            if target.is_some() || !apimachinery::kubevirt::wants_migration_target(object) {
                self.target_assumptions.remove(&key);
            }
            if let Some((node, shim)) = target {
                charge(&mut self.usage, vm, node.clone(), &shim);
                self.targets.insert(key.clone(), (node, shim));
            }
            if let Some((node, pod)) = placed_workload(vm, object) {
                charge(&mut self.usage, vm, node.clone(), &pod);
                self.placed.insert(key, (node, pod));
            }
        }
    }
    fn reserve_target(&mut self, key: ScheduleKey, object: &Value, node: &str) {
        let mut shim = virtualmachine::scheduling_shim(object);
        shim["spec"]["nodeName"] = json!(node);
        self.target_assumptions.insert(key, (node.into(), shim));
    }
    fn reserve(&mut self, key: ScheduleKey, object: &Value, node: &str, waiting: bool) {
        self.assumptions.insert(
            key,
            Assumption {
                node: node.into(),
                object: object.clone(),
                waiting_for_volume: waiting,
            },
        );
    }
    fn snapshot(&self, current: &ScheduleKey) -> (ClusterState, Option<String>) {
        let mut state = ClusterState {
            usage: self.usage.clone(),
            placed: self.placed.values().chain(self.targets.values()).cloned().collect(),
        };
        for (key, (node, shim)) in &self.target_assumptions {
            if key == current || self.targets.contains_key(key) {
                continue;
            }
            charge(&mut state.usage, key.0, node.clone(), shim);
            state.placed.push((node.clone(), shim.clone()));
        }
        for (key, reserved) in &self.assumptions {
            if key == current || self.placed.contains_key(key) {
                continue;
            }
            let mut pod = if key.0 {
                virtualmachine::scheduling_shim(&reserved.object)
            } else {
                reserved.object.clone()
            };
            pod["spec"]["nodeName"] = json!(reserved.node);
            charge(&mut state.usage, key.0, reserved.node.clone(), &pod);
            state.placed.push((reserved.node.clone(), pod));
        }
        (state, self.assumptions.get(current).map(|a| a.node.clone()))
    }
}
fn scheduling_feed(
    api: &ApiClient,
    path: &str,
    vm: bool,
    ready: &Arc<WorkQueue<ScheduleKey>>,
    state: &Arc<Mutex<SchedulingState>>,
) -> Subscription {
    let wake = ready.clone();
    let state = state.clone();
    api.informers.subscribe(
        &api.client,
        format!("{}{}", api.base_url, path),
        move |changes, reset| {
            let keys = {
                let mut state = state.lock().unwrap();
                let mut keys = HashSet::new();
                let mut placement_changed = reset;
                for delta in changes {
                    let before = delta.old.as_ref().and_then(|o| placed_workload(vm, o));
                    let after = delta.new.as_ref().and_then(|o| placed_workload(vm, o));
                    placement_changed |= before != after;
                    // A migration target taken or released moves capacity too.
                    placement_changed |= delta.old.as_ref().and_then(|o| migration_target(vm, o))
                        != delta.new.as_ref().and_then(|o| migration_target(vm, o));
                    state.observe(vm, delta);
                    for object in delta.old.iter().chain(delta.new.iter()) {
                        if let Ok(key) = Key::of(object) {
                            keys.insert((vm, key));
                        }
                    }
                }
                if placement_changed {
                    keys.extend(state.pending.keys().cloned());
                }
                crate::metrics_server::set_pending_pods(
                    state.pending.keys().filter(|k| !k.0).count(),
                );
                crate::metrics_server::set_pending_virtual_machines(
                    state.pending.keys().filter(|k| k.0).count(),
                );
                keys
            };
            for key in keys {
                wake.add(key);
            }
        },
    )
}
fn indexed_volume_state(
    pod: &Value,
    feeds: &[Subscription],
) -> anyhow::Result<volumebinding::VolumeState> {
    let mut state = volumebinding::VolumeState::default();
    let ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    for name in volumebinding::pod_claims(pod) {
        for pvc in feeds[1]
            .feed
            .select(&Index::Name(ns.into(), name.clone()))?
        {
            if let Some(volume) = pvc["spec"]["volumeName"].as_str() {
                for pv in feeds[2]
                    .feed
                    .select(&Index::Name("".into(), volume.into()))?
                {
                    state.volumes.insert(volume.into(), pv);
                }
            }
            let class = pvc["spec"]["storageClassName"].as_str().unwrap_or("");
            for pv in feeds[2].feed.select(&Index::StorageClass(class.into()))? {
                if let Some(name) = pv["metadata"]["name"].as_str() {
                    state.volumes.insert(name.into(), pv.clone());
                }
            }
            for sc in feeds[3]
                .feed
                .select(&Index::Name("".into(), class.into()))?
            {
                let driver = sc["provisioner"].as_str().unwrap_or("");
                if feeds[4]
                    .feed
                    .select(&Index::Name("".into(), driver.into()))?
                    .iter()
                    .any(|d| d["spec"]["storageCapacity"] == true)
                {
                    state.capacity_tracking.push(driver.into());
                }
                state.classes.insert(class.into(), sc);
            }
            state
                .capacities
                .extend(feeds[5].feed.select(&Index::StorageClass(class.into()))?);
            state.claims.insert((ns.into(), name.clone()), pvc);
        }
    }
    Ok(state)
}

/// Most bind writes outstanding at once (#190).
const MAX_BINDS_IN_FLIGHT: usize = 16;

/// A bind write: the Pod with `spec.nodeName` set, PUT to `path`.
#[derive(Clone)]
struct Bind {
    node: String,
    path: String,
    body: Value,
}

/// A finished bind write, with the queue ownership it held.
struct BindDone {
    work: apimachinery::workqueue::Work<ScheduleKey>,
    node: String,
    queued: Option<std::time::Instant>,
    ok: bool,
}

/// What a scheduling pass decided for one pod.
enum Placement {
    /// Bind to this node: capacity is reserved, the write is the caller's.
    Bind(Bind),
    /// No node will do; the message is for `PodScheduled=False` (#194).
    Unschedulable(String),
    /// The node is chosen and written onto the pod's claims, but the pod is
    /// deliberately not bound until those claims are.
    WaitingForVolumes(String),
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
            (
                "default".to_string(),
                json!({"metadata":{"name":name,"creationTimestamp":ts},"spec":spec}),
            )
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
        let order: Vec<&str> = v
            .iter()
            .map(|(_, p)| p["metadata"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(order, ["high", "old-default", "new-default", "low"]);
    }
}

#[cfg(test)]
mod accounting_tests {
    use crate::filter::pod_requests;
    use serde_json::json;

    #[test]
    fn a_shrinking_pod_still_holds_the_larger_amount() {
        // In-place resize is asynchronous: the spec says 500m, the kubelet has
        // not actuated it, and the pod is still holding 2000m. Believing the
        // spec here hands 1500m to another pod that the node does not have.
        let pod = json!({
            "spec":{"containers":[{"resources":{"requests":{"cpu":"500m"}}}]},
            "status":{"containerStatuses":[
                {"resources":{"requests":{"cpu":"2"}}}]}});
        let (cpu, _) = pod_requests(&pod);
        assert_eq!(
            cpu, 2000,
            "must account the actuated size, not the desired one"
        );
    }

    #[test]
    fn pod_level_requests_replace_the_container_sum() {
        // spec.resources is the pod's total, not an addition to its containers.
        // Summing both double-counts every pod that sets it.
        let pod = json!({
            "spec":{"resources":{"requests":{"cpu":"1","memory":"1Gi"}},
                    "containers":[{"resources":{"requests":{"cpu":"500m"}}},
                                  {"resources":{"requests":{"cpu":"500m"}}}]}});
        let (cpu, _) = pod_requests(&pod);
        assert_eq!(cpu, 1000, "pod-level wins; 2000 would be double counting");
    }

    #[test]
    fn init_containers_count_as_the_largest_not_the_sum() {
        // They run one at a time and are done before the app starts, so summing
        // them reserves capacity the pod never holds at once — and can make a
        // pod unschedulable on a small node that would have run it.
        let pod = json!({"spec":{
            "containers":[{"resources":{"requests":{"cpu":"100m"}}}],
            "initContainers":[{"resources":{"requests":{"cpu":"400m"}}},
                              {"resources":{"requests":{"cpu":"300m"}}}]}});
        let (cpu, _) = pod_requests(&pod);
        assert_eq!(cpu, 400, "the largest init container, not 700m");
    }
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    fn pod(uid: &str, rv: &str) -> Value {
        json!({"metadata":{"namespace":"ns","name":uid,"uid":uid,"resourceVersion":rv},
            "spec":{"containers":[{"resources":{"requests":{"cpu":"600m","memory":"1Gi"}}}]}})
    }
    fn changed(object: Value) -> Delta {
        Delta {
            old: None,
            new: Some(object),
            affected: Default::default(),
        }
    }
    #[tokio::test]
    async fn a_missing_claim_leaves_no_assumption_pinning_the_first_node() {
        // No API is reachable: the claim path must fail before any write.
        let sched = Scheduler::new("http://127.0.0.1:1");
        let mut p = pod("a", "v1");
        p["spec"]["volumes"] = json!([{"name":"v","persistentVolumeClaim":{"claimName":"late"}}]);
        let node = json!({"metadata":{"name":"first"},"status":{
            "allocatable":{"cpu":"4","memory":"8Gi","pods":"10"},
            "conditions":[{"type":"Ready","status":"True"}]}});
        let key = (false, Key::of(&p).unwrap());
        let observed = Mutex::new(SchedulingState::default());
        let result = sched
            .schedule_pod(
                "ns",
                &p,
                &[node],
                &ClusterState::default(),
                &Default::default(),
                &observed,
                &key,
            )
            .await;
        let error = match result {
            Err(e) => e.to_string(),
            Ok(Placement::Unschedulable(why)) => why,
            Ok(_) => panic!("a missing claim cannot be placed"),
        };
        assert!(error.contains("claim ns/late"), "reached the claim path: {error}");
        let state = observed.lock().unwrap();
        assert!(state.assumptions.is_empty(), "no write, so no reservation");
        assert_eq!(state.snapshot(&key).1, None, "the Pod may choose any node later");
    }
    #[test]
    fn assumed_bind_is_charged_once_across_acknowledgement_and_watch_lag() {
        let mut state = SchedulingState::default();
        let p = pod("a", "opaque-a");
        let a = (false, Key::of(&p).unwrap());
        let other = (false, Key::of(&pod("b", "v")).unwrap());
        state.observe(false, &changed(p.clone()));
        state.reserve(a.clone(), &p, "node", false);
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli, 600);
        assert!(!state.snapshot(&a).0.usage.contains_key("node"));
        // A failed response followed by the unchanged cache retains the charge.
        state.observe(false, &changed(p.clone()));
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli, 600);
        let mut bound = p.clone();
        bound["spec"]["nodeName"] = json!("node");
        bound["metadata"]["resourceVersion"] = json!("opaque-b");
        state.observe(false, &changed(bound.clone()));
        state.observe(false, &changed(bound)); // duplicate observation never double counts
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli, 600);
        assert!(state.assumptions.is_empty());
        state.observe(
            false,
            &Delta {
                old: Some(p),
                new: None,
                affected: Default::default(),
            },
        );
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli, 0);
    }
    #[test]
    fn queued_from_first_pending_observation_until_bound() {
        let mut state = SchedulingState::default();
        let p = pod("a", "v1");
        let key = (false, Key::of(&p).unwrap());
        state.observe(false, &changed(p.clone()));
        let first = state.queued[&key];
        let mut again = p.clone();
        again["metadata"]["resourceVersion"] = json!("v2");
        state.observe(false, &changed(again));
        assert_eq!(state.queued[&key], first, "a later pending version keeps the first time");
        let mut bound = p;
        bound["spec"]["nodeName"] = json!("node");
        state.observe(false, &changed(bound));
        assert!(state.queued.is_empty(), "bound is no longer queued");
    }
    #[test]
    fn volume_wait_and_adopted_vmi_share_capacity_with_pods() {
        let mut state = SchedulingState::default();
        let p = pod("a", "v1");
        let key = (false, Key::of(&p).unwrap());
        state.reserve(key.clone(), &p, "node", true);
        let mut updated = p;
        updated["metadata"]["resourceVersion"] = json!("v2");
        state.observe(false, &changed(updated));
        assert!(state.assumptions.contains_key(&key));
        let vmi = json!({"metadata":{"name":"vm","namespace":"ns","uid":"vm","resourceVersion":"v"},
            "spec":{"domain":{"cpu":{"cores":1},"memory":{"guest":"2Gi"}}},"status":{"nodeName":"node","phase":"Running"}});
        state.observe(true, &changed(vmi));
        let other = (false, Key::of(&pod("b", "v")).unwrap());
        assert_eq!(
            state.snapshot(&other).0.usage["node"].mem_bytes,
            3 * 1024 * 1024 * 1024
        );
        let mut terminal = pod("a", "v3");
        terminal["status"]["phase"] = json!("Failed");
        state.observe(false, &changed(terminal));
        assert_eq!(
            state.snapshot(&other).0.usage["node"].mem_bytes,
            2 * 1024 * 1024 * 1024
        );
    }

    fn best_effort(name: &str) -> Value {
        json!({"metadata":{"namespace":"ns","name":name,"uid":name,"resourceVersion":"v1"},
            "spec":{"containers":[{"name":"c"}]}})
    }
    fn two_pod_node() -> Value {
        json!({"metadata":{"name":"small"},"status":{
            "allocatable":{"cpu":"4","memory":"8Gi","pods":"2"},
            "conditions":[{"type":"Ready","status":"True"}]}})
    }
    async fn place(state: &SchedulingState, p: &Value) -> Placement {
        let sched = Scheduler::new("http://127.0.0.1:1");
        let key = (false, Key::of(p).unwrap());
        let (snapshot, _) = state.snapshot(&key);
        sched
            .schedule_pod(
                "ns",
                p,
                &[two_pod_node()],
                &snapshot,
                &Default::default(),
                &Mutex::new(SchedulingState::default()),
                &key,
            )
            .await
            .expect("no API error on this path")
    }
    #[tokio::test]
    async fn a_full_node_leaves_the_third_pod_unscheduled_until_one_finishes() {
        // #194: BestEffort Pods request nothing, so resource fit passed them
        // all and 1,000 landed on a 110-pod node.
        let mut state = SchedulingState::default();
        let mut a = best_effort("a");
        a["spec"]["nodeName"] = json!("small");
        state.observe(false, &changed(a.clone()));
        // The second is only assumed (bind write in flight): it holds a slot.
        let b = best_effort("b");
        state.observe(false, &changed(b.clone()));
        state.reserve((false, Key::of(&b).unwrap()), &b, "small", false);
        let c = best_effort("c");
        state.observe(false, &changed(c.clone()));
        match place(&state, &c).await {
            Placement::Unschedulable(why) => assert_eq!(
                why, "0/1 nodes are available: 1 Too many pods.",
                "upstream's reason and shape"
            ),
            _ => panic!("a third Pod must not bind to a 2-pod node"),
        }
        // A Succeeded Pod no longer counts against allocatable.pods.
        a["status"]["phase"] = json!("Succeeded");
        a["metadata"]["resourceVersion"] = json!("v2");
        state.observe(false, &changed(a));
        assert!(matches!(place(&state, &c).await, Placement::Bind(b) if b.node == "small"));
    }
    #[test]
    fn a_vmi_takes_no_pod_slot_and_a_deleted_pod_frees_its_own() {
        let mut state = SchedulingState::default();
        let vmi = json!({"metadata":{"name":"vm","namespace":"ns","uid":"vm","resourceVersion":"v"},
            "spec":{"domain":{"memory":{"guest":"1Gi"}}},"status":{"nodeName":"small","phase":"Running"}});
        state.observe(true, &changed(vmi));
        let mut a = best_effort("a");
        a["spec"]["nodeName"] = json!("small");
        state.observe(false, &changed(a.clone()));
        state.observe(false, &changed(a.clone())); // re-observed: still one
        let other = (false, Key::of(&best_effort("z")).unwrap());
        assert_eq!(state.snapshot(&other).0.usage["small"].pods, 1);
        state.observe(false, &Delta { old: Some(a), new: None, affected: Default::default() });
        assert_eq!(state.snapshot(&other).0.usage["small"].pods, 0);
    }
    #[test]
    fn the_unschedulable_message_counts_reasons_most_common_first() {
        let refused = ["node is not Ready", "Too many pods", "Too many pods"].map(String::from);
        assert_eq!(
            unschedulable_message(3, &refused),
            "0/3 nodes are available: 2 Too many pods, 1 node is not Ready."
        );
    }

    fn node(name: &str, memory: &str) -> Value {
        json!({"metadata":{"name":name},"status":{
            "allocatable":{"cpu":"8","memory":memory,"pods":"110"},
            "conditions":[{"type":"Ready","status":"True"}]}})
    }

    fn migrating(target: Option<&str>) -> Value {
        let mut state = json!({"migrationUid":"m1","sourceNode":"a","completed":false,"failed":false});
        if let Some(t) = target {
            state["targetNode"] = json!(t);
        }
        json!({"metadata":{"name":"vm","namespace":"ns","uid":"vm","resourceVersion":"v"},
            "spec":{"domain":{"cpu":{"cores":1},"memory":{"guest":"2Gi"}}},
            "status":{"nodeName":"a","phase":"Running","migrationState":state}})
    }

    #[test]
    fn a_migration_target_is_never_the_source_and_must_fit() {
        let nodes = [node("a", "64Gi"), node("b", "1Gi"), node("c", "16Gi")];
        let shim = virtualmachine::scheduling_shim(&migrating(None));
        let (chosen, refused) = choose_migration_target(&shim, "a", &nodes, &ClusterState::default());
        assert_eq!(chosen.as_deref(), Some("c"));
        assert!(refused.iter().any(|r| r.starts_with("a: the VM is already there")), "{refused:?}");
        assert!(refused.iter().any(|r| r.starts_with("b: ")), "{refused:?}");
        let (chosen, refused) = choose_migration_target(&shim, "a", &nodes[..2], &ClusterState::default());
        assert_eq!(chosen, None);
        assert_eq!(refused.len(), 2);
    }

    #[test]
    fn a_vmi_waiting_for_a_target_is_pending_and_one_with_a_target_is_not() {
        assert!(pending_workload(true, &migrating(None)));
        assert!(!pending_workload(true, &migrating(Some("c"))));
        let mut done = migrating(Some("c"));
        done["status"]["migrationState"]["completed"] = json!(true);
        assert!(!pending_workload(true, &done));
    }

    #[test]
    fn a_migrating_vmi_is_charged_on_both_nodes_until_it_ends() {
        let gib = 1024 * 1024 * 1024;
        let mut state = SchedulingState::default();
        let other = (false, Key::of(&pod("b", "v")).unwrap());
        let vm_key = (true, Key::of(&migrating(None)).unwrap());
        state.observe(true, &changed(migrating(None)));
        assert_eq!(state.snapshot(&other).0.usage["a"].mem_bytes, 2 * gib);
        // Chosen, not yet seen back: the assumption holds the target.
        state.reserve_target(vm_key.clone(), &migrating(None), "c");
        assert_eq!(state.snapshot(&other).0.usage["c"].mem_bytes, 2 * gib);
        // Seen back: charged once, not twice.
        let mut seen = migrating(Some("c"));
        seen["metadata"]["resourceVersion"] = json!("v2");
        state.observe(true, &changed(seen.clone()));
        assert!(state.target_assumptions.is_empty());
        let usage = state.snapshot(&other).0.usage;
        assert_eq!(usage["a"].mem_bytes, 2 * gib);
        assert_eq!(usage["c"].mem_bytes, 2 * gib);
        // Completed, not yet moved: the machine is on c, the VMI names a.
        seen["status"]["migrationState"]["completed"] = json!(true);
        state.observe(true, &changed(seen.clone()));
        let usage = state.snapshot(&other).0.usage;
        assert_eq!(usage["c"].mem_bytes, 2 * gib, "the target stays charged until the move");
        // Moved: only the target holds it.
        seen["status"]["nodeName"] = json!("c");
        state.observe(true, &changed(seen));
        let usage = state.snapshot(&other).0.usage;
        assert_eq!(usage["a"].mem_bytes, 0);
        assert_eq!(usage["c"].mem_bytes, 2 * gib);
    }

    #[test]
    fn a_failed_migration_frees_its_target() {
        let gib = 1024 * 1024 * 1024;
        let mut state = SchedulingState::default();
        let other = (false, Key::of(&pod("b", "v")).unwrap());
        state.observe(true, &changed(migrating(Some("c"))));
        assert_eq!(state.snapshot(&other).0.usage["c"].mem_bytes, 2 * gib);
        let mut failed = migrating(Some("c"));
        failed["status"]["migrationState"]["failed"] = json!(true);
        state.observe(true, &changed(failed));
        let usage = state.snapshot(&other).0.usage;
        assert_eq!(usage["c"].mem_bytes, 0);
        assert_eq!(usage["a"].mem_bytes, 2 * gib);
    }
}
