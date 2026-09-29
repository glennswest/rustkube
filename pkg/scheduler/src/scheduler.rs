//! Core scheduler loop.
//!
//! Dependency changes enqueue pods without a nodeName (and unplaced
//! VirtualMachineInstances), runs the fixed filter and score functions, then
//! binds each to the best node via the API server.

use crate::filter::{self, FilterResult, NodeUsage};
use crate::score;
use crate::virtualmachine;
use crate::volumebinding;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::collections::{HashMap, HashSet};
use apimachinery::informer::{Delta, Index, Key};
use apimachinery::informers::{Feed, Hub, Subscription};
use apimachinery::workqueue::WorkQueue;
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
            self.informers.acknowledge(&format!("{}{}",self.base_url,path),value);
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
            self.informers.acknowledge(&format!("{}{}",self.base_url,path),value);
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
            self.informers.acknowledge(&format!("{}{}",self.base_url,path),value);
            if value["kind"] == "Status" && value["code"].as_u64().unwrap_or(0) >= 400 {
                apimachinery::reactor::failed();
            }
        }
        apimachinery::reactor::check(result.map_err(anyhow::Error::from))
    }

    /// POST (create) returning the decoded body.
    pub async fn create(&self, path: &str, body: &Value) -> anyhow::Result<Value> {
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
            self.informers.acknowledge(&format!("{}{}",self.base_url,path),value);
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
}

impl Scheduler {
    pub fn new(api_server_url: &str) -> Self {
        Self {
            api: Arc::new(ApiClient::new(api_server_url)),
            leader_elect: true,
            identity: default_identity(),
            startup_timeout: apimachinery::startup::DEFAULT_STARTUP_TIMEOUT,
        }
    }

    /// Connect with TLS + auth (HTTPS apiserver / mutual TLS or token).
    pub fn connect(api_server_url: &str, cfg: ClientConfig) -> anyhow::Result<Self> {
        Ok(Self {
            api: Arc::new(ApiClient::configured(api_server_url, cfg)?),
            leader_elect: true,
            identity: default_identity(),
            startup_timeout: apimachinery::startup::DEFAULT_STARTUP_TIMEOUT,
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
            elector.acquire().await;
            self.api.write_gate.start();
            info!("Became leader; scheduling pods");
            crate::metrics_server::set_leader(true);
            let leadership = async {
                loop {
                    // Lease maintenance is an actual timed obligation.
                    tokio::time::sleep(elector.retry_period()).await;
                    if !tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        elector.try_acquire_or_renew(),
                    )
                    .await
                    .unwrap_or(false)
                        || !self.api.write_gate.renew()
                    {
                        break;
                    }
                }
            };
            tokio::select! {
                biased;
                _ = leadership => {
                    self.api.write_gate.close();
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
        let pods = scheduling_feed(&self.api,"/api/v1/pods",false,&ready,&observed);
        let wake = ready.clone(); let state = observed.clone();
        let crds = self.api.informers.subscribe(&self.api.client,
            format!("{}/apis/apiextensions.k8s.io/v1/customresourcedefinitions",self.api.base_url),
            move |_,_| {
                let keys: Vec<_> = state.lock().unwrap().pending.keys().cloned().collect();
                for key in keys { wake.add(key); }
                wake.add((false,Key {namespace:"".into(),name:"".into(),uid:"discovery".into()}));
            });
        let paths = ["/api/v1/nodes","/api/v1/persistentvolumeclaims","/api/v1/persistentvolumes",
            "/apis/storage.k8s.io/v1/storageclasses","/apis/storage.k8s.io/v1/csidrivers",
            "/apis/storage.k8s.io/v1/csistoragecapacities"];
        let dependencies: Vec<_> = paths.iter().map(|path| {
            let wake = ready.clone(); let state = observed.clone(); let pod_feed = pods.feed.clone(); let is_claim = *path == "/api/v1/persistentvolumeclaims";
            self.api.informers.subscribe(&self.api.client,format!("{}{}",self.api.base_url,path),move |changes,reset| {
                if is_claim && !reset {
                    for delta in changes {
                        for claim in delta.old.iter().chain(delta.new.iter()) {
                            let ns = claim["metadata"]["namespace"].as_str().unwrap_or("");
                            let name = claim["metadata"]["name"].as_str().unwrap_or("");
                            for pod in pod_feed.select(&Index::Claim(ns.into(),name.into())).unwrap_or_default() {
                                if let Ok(key) = Key::of(&pod) { wake.add((false,key)); }
                            }
                        }
                    }
                } else {
                    let keys: Vec<_> = state.lock().unwrap().pending.keys().cloned().collect();
                    for key in keys { wake.add(key); }
                }
            })
        }).collect();
        let mut vmis: Option<Subscription> = None;
        let mut failures: HashMap<ScheduleKey,u32> = HashMap::new();
        loop {
            let work = ready.next_by(|a,b| {
                let state = observed.lock().unwrap();
                let a = state.pending.get(a); let b = state.pending.get(b);
                b.map(pod_priority).unwrap_or(0).cmp(&a.map(pod_priority).unwrap_or(0))
                    .then_with(|| a.map(creation_ts).cmp(&b.map(creation_ts)))
            }).await;
            ready.cancel_deadline(work.key());
            let key = work.key().clone();
            let wake = ready.clone(); let retry = key.clone();
            let (result,failed) = apimachinery::reactor::scope_object(move |delay| {
                wake.add_at(retry.clone(),tokio::time::Instant::now()+delay);
            },async {
                crds.feed.ensure_synced()?;
                if vmis.is_none() && !crds.feed.select(&Index::Name("".into(),"virtualmachineinstances.kubevirt.io".into()))?.is_empty() {
                    vmis = Some(scheduling_feed(&self.api,virtualmachine::LIST_PATH,true,&ready,&observed));
                }
                pods.feed.ensure_synced()?;
                for feed in &dependencies { feed.feed.ensure_synced()?; }
                if let Some(feed) = &vmis { feed.feed.ensure_synced()?; }
                let source = if key.0 { vmis.as_ref().map(|f| &f.feed) } else { Some(&pods.feed) };
                let Some(object) = source.map(|feed| feed.get(&key.1)).transpose()?.flatten() else { return Ok::<(),anyhow::Error>(()); };
                observed.lock().unwrap().observe(key.0,&Delta { old: None, new: Some(object.clone()), affected: Default::default() });
                if !pending_workload(key.0,&object) { return Ok(()); }
                let mut nodes = dependencies[0].feed.list()?;
                let (mut state, assumed) = observed.lock().unwrap().snapshot(&key);
                if let Some(node) = assumed { nodes.retain(|n| n["metadata"]["name"] == node); }
                if key.0 {
                    self.schedule_virtual_machine(&object,&nodes,&mut state,&observed,&key).await;
                } else {
                    let volumes = indexed_volume_state(&object,&dependencies)?;
                    let ns = object["metadata"]["namespace"].as_str().unwrap_or("default");
                    match self.schedule_pod(ns,&object,&nodes,&state,&volumes,&observed,&key).await {
                        Ok(Placement::Bound(_)) => crate::metrics_server::record_attempt("scheduled"),
                        Ok(Placement::WaitingForVolumes(node)) => {
                            observed.lock().unwrap().reserve(key.clone(),&object,&node,true);
                            crate::metrics_server::record_attempt("unschedulable");
                        }
                        Err(error) => { debug!(%error,?key,"workload not placed"); }
                    }
                }
                Ok(())
            }).await;
            if result.is_err() || failed {
                let n = failures.entry(key.clone()).or_default(); *n = n.saturating_add(1);
                ready.add_at(key,tokio::time::Instant::now()+Duration::from_millis((100_u64 << (*n).min(8)).min(30_000)));
            } else { failures.remove(&key); }
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
        observed: &Mutex<SchedulingState>, key: &ScheduleKey,
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
        observed.lock().unwrap().reserve(key.clone(),vmi,chosen,false);
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

    async fn schedule_pod(
        &self,
        namespace: &str,
        pod: &Value,
        nodes: &[Value],
        state: &ClusterState,
        volumes: &volumebinding::VolumeState,
        observed: &Mutex<SchedulingState>, key: &ScheduleKey,
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
            return Err(anyhow::anyhow!("{reason}"));
        }

        // Phase 1: Filter — find nodes that can run this pod
        let feasible: Vec<&Value> = nodes
            .iter()
            .filter(|node| {
                let result = filter::run_filters(pod, node, state.used(node), state, nodes);
                if !matches!(result, FilterResult::Pass) {
                    return false;
                }
                // Storage last: it is the filter that needs the extra listing,
                // and there is no point paying for it on a node that has
                // already been ruled out on CPU.
                match volumebinding::filter_node(pod, namespace, node, volumes) {
                    Ok(()) => true,
                    Err(reason) => {
                        debug!("node rejected for {namespace}/{pod_name}: {reason}");
                        false
                    }
                }
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

        observed.lock().unwrap().reserve(key.clone(),pod,chosen_name,false);

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

        // Phase 3b: Bind — update the pod with the chosen node
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

        Ok(Placement::Bound(chosen_name.to_string()))
    }


}

type ScheduleKey = (bool, Key); // false = Pod, true = VMI; one shared executor
struct Assumption { node: String, object: Value, waiting_for_volume: bool }
#[derive(Default)]
struct SchedulingState {
    pending: HashMap<ScheduleKey,Value>,
    placed: HashMap<ScheduleKey,(String,Value)>,
    usage: HashMap<String,NodeUsage>,
    assumptions: HashMap<ScheduleKey,Assumption>,
}
fn pending_workload(vm: bool, object: &Value) -> bool {
    if !object["metadata"]["deletionTimestamp"].is_null() { return false; }
    if vm { !virtualmachine::is_terminal(object) && virtualmachine::node_of(object).is_none() }
    else { !matches!(object["status"]["phase"].as_str(),Some("Succeeded" | "Failed"))
        && object["spec"]["nodeName"].as_str().is_none_or(|n| n.is_empty()) }
}
fn placed_workload(vm: bool, object: &Value) -> Option<(String,Value)> {
    if vm {
        if virtualmachine::is_terminal(object) { return None; }
        virtualmachine::node_of(object).map(|n|(n.into(),virtualmachine::scheduling_shim(object)))
    } else {
        if matches!(object["status"]["phase"].as_str(),Some("Succeeded" | "Failed")) { return None; }
        object["spec"]["nodeName"].as_str().filter(|n| !n.is_empty()).map(|n|(n.into(),object.clone()))
    }
}
impl SchedulingState {
    fn remove(&mut self, key: &ScheduleKey) {
        self.pending.remove(key);
        self.assumptions.remove(key);
        if let Some((node,pod)) = self.placed.remove(key) {
            let (cpu,mem) = filter::pod_requests(&pod);
            let used = self.usage.entry(node).or_default();
            used.cpu_milli -= cpu; used.mem_bytes -= mem;
        }
    }
    fn observe(&mut self, vm: bool, delta: &Delta) {
        if let Some(old) = &delta.old {
            if let Ok(key) = Key::of(old) {
                if delta.new.as_ref().and_then(|o| Key::of(o).ok()).as_ref() != Some(&key) { self.remove(&(vm,key)); }
            }
        }
        if let Some(object) = &delta.new {
            let Ok(key) = Key::of(object) else { return; };
            let key = (vm,key);
            if let Some((node,old)) = self.placed.remove(&key) {
                let (cpu,mem) = filter::pod_requests(&old);
                let used = self.usage.entry(node).or_default(); used.cpu_milli -= cpu; used.mem_bytes -= mem;
            }
            self.pending.remove(&key);
            if pending_workload(vm,object) {
                self.pending.insert(key.clone(),object.clone());
                if self.assumptions.get(&key).is_some_and(|a| !a.waiting_for_volume
                    && a.object["metadata"]["resourceVersion"] != object["metadata"]["resourceVersion"]) {
                    // A later durable object version fences any delayed CAS bind
                    // issued with the assumed version, so its reservation can go.
                    self.assumptions.remove(&key);
                }
            } else { self.assumptions.remove(&key); }
            if let Some((node,pod)) = placed_workload(vm,object) {
                let (cpu,mem) = filter::pod_requests(&pod);
                let used = self.usage.entry(node.clone()).or_default(); used.cpu_milli += cpu; used.mem_bytes += mem;
                self.placed.insert(key,(node,pod));
            }
        }
    }
    fn reserve(&mut self, key: ScheduleKey, object: &Value, node: &str, waiting: bool) {
        self.assumptions.insert(key,Assumption {node:node.into(),object:object.clone(),waiting_for_volume:waiting});
    }
    fn snapshot(&self, current: &ScheduleKey) -> (ClusterState,Option<String>) {
        let mut state = ClusterState { usage: self.usage.clone(), placed: self.placed.values().cloned().collect() };
        for (key,reserved) in &self.assumptions {
            if key == current || self.placed.contains_key(key) { continue; }
            let mut pod = if key.0 { virtualmachine::scheduling_shim(&reserved.object) } else { reserved.object.clone() };
            pod["spec"]["nodeName"] = json!(reserved.node);
            let (cpu,mem) = filter::pod_requests(&pod);
            let used = state.usage.entry(reserved.node.clone()).or_default(); used.cpu_milli += cpu; used.mem_bytes += mem;
            state.placed.push((reserved.node.clone(),pod));
        }
        (state,self.assumptions.get(current).map(|a|a.node.clone()))
    }
}
fn scheduling_feed(api: &ApiClient, path: &str, vm: bool, ready: &Arc<WorkQueue<ScheduleKey>>, state: &Arc<Mutex<SchedulingState>>) -> Subscription {
    let wake = ready.clone(); let state = state.clone();
    api.informers.subscribe(&api.client,format!("{}{}",api.base_url,path),move |changes,reset| {
        let keys = {
            let mut state = state.lock().unwrap();
            let mut keys = HashSet::new();
            let mut placement_changed = reset;
            for delta in changes {
                let before = delta.old.as_ref().and_then(|o| placed_workload(vm,o));
                let after = delta.new.as_ref().and_then(|o| placed_workload(vm,o));
                placement_changed |= before != after;
                state.observe(vm,delta);
                for object in delta.old.iter().chain(delta.new.iter()) {
                    if let Ok(key) = Key::of(object) { keys.insert((vm,key)); }
                }
            }
            if placement_changed { keys.extend(state.pending.keys().cloned()); }
            crate::metrics_server::set_pending_pods(state.pending.keys().filter(|k| !k.0).count());
            crate::metrics_server::set_pending_virtual_machines(state.pending.keys().filter(|k| k.0).count());
            keys
        };
        for key in keys { wake.add(key); }
    })
}
fn indexed_volume_state(pod: &Value, feeds: &[Subscription]) -> anyhow::Result<volumebinding::VolumeState> {
    let mut state = volumebinding::VolumeState::default();
    let ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    for name in volumebinding::pod_claims(pod) {
        for pvc in feeds[1].feed.select(&Index::Name(ns.into(),name.clone()))? {
            if let Some(volume) = pvc["spec"]["volumeName"].as_str() {
                for pv in feeds[2].feed.select(&Index::Name("".into(),volume.into()))? { state.volumes.insert(volume.into(),pv); }
            }
            let class = pvc["spec"]["storageClassName"].as_str().unwrap_or("");
            for pv in feeds[2].feed.select(&Index::StorageClass(class.into()))? {
                if let Some(name) = pv["metadata"]["name"].as_str() { state.volumes.insert(name.into(),pv.clone()); }
            }
            for sc in feeds[3].feed.select(&Index::Name("".into(),class.into()))? {
                let driver = sc["provisioner"].as_str().unwrap_or("");
                if feeds[4].feed.select(&Index::Name("".into(),driver.into()))?.iter().any(|d| d["spec"]["storageCapacity"] == true) { state.capacity_tracking.push(driver.into()); }
                state.classes.insert(class.into(),sc);
            }
            state.capacities.extend(feeds[5].feed.select(&Index::StorageClass(class.into()))?);
            state.claims.insert((ns.into(),name.clone()),pvc);
        }
    }
    Ok(state)
}

/// What a scheduling pass decided for one pod.
enum Placement {
    /// Bound to this node.
    Bound(String),
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
    fn changed(object: Value) -> Delta { Delta { old:None,new:Some(object),affected:Default::default() } }
    #[test]
    fn assumed_bind_is_charged_once_across_acknowledgement_and_watch_lag() {
        let mut state = SchedulingState::default();
        let p = pod("a","opaque-a"); let a = (false,Key::of(&p).unwrap());
        let other = (false,Key::of(&pod("b","v")).unwrap());
        state.observe(false,&changed(p.clone())); state.reserve(a.clone(),&p,"node",false);
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli,600);
        assert!(!state.snapshot(&a).0.usage.contains_key("node"));
        // A failed response followed by the unchanged cache retains the charge.
        state.observe(false,&changed(p.clone()));
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli,600);
        let mut bound = p.clone(); bound["spec"]["nodeName"] = json!("node");
        bound["metadata"]["resourceVersion"] = json!("opaque-b");
        state.observe(false,&changed(bound.clone()));
        state.observe(false,&changed(bound)); // duplicate observation never double counts
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli,600);
        assert!(state.assumptions.is_empty());
        state.observe(false,&Delta {old:Some(p),new:None,affected:Default::default()});
        assert_eq!(state.snapshot(&other).0.usage["node"].cpu_milli,0);
    }
    #[test]
    fn volume_wait_and_adopted_vmi_share_capacity_with_pods() {
        let mut state = SchedulingState::default();
        let p = pod("a","v1"); let key = (false,Key::of(&p).unwrap());
        state.reserve(key.clone(),&p,"node",true);
        let mut updated = p; updated["metadata"]["resourceVersion"] = json!("v2");
        state.observe(false,&changed(updated));
        assert!(state.assumptions.contains_key(&key));
        let vmi = json!({"metadata":{"name":"vm","namespace":"ns","uid":"vm","resourceVersion":"v"},
            "spec":{"domain":{"cpu":{"cores":1},"memory":{"guest":"2Gi"}}},"status":{"nodeName":"node","phase":"Running"}});
        state.observe(true,&changed(vmi));
        let other = (false,Key::of(&pod("b","v")).unwrap());
        assert_eq!(state.snapshot(&other).0.usage["node"].mem_bytes,3*1024*1024*1024);
        let mut terminal = pod("a","v3"); terminal["status"]["phase"] = json!("Failed");
        state.observe(false,&changed(terminal));
        assert_eq!(state.snapshot(&other).0.usage["node"].mem_bytes,2*1024*1024*1024);
    }
}
