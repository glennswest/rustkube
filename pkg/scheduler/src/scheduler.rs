//! Core scheduler loop.
//!
//! Once a second, lists pods without a nodeName (and unplaced
//! VirtualMachineInstances), runs the fixed filter and score functions, then
//! binds each to the best node via the API server.

use crate::filter::{self, FilterResult, NodeUsage};
use crate::score;
use crate::virtualmachine;
use crate::volumebinding;
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

    /// PATCH a resource with a strategic-merge patch.
    pub async fn patch(&self, path: &str, body: &Value) -> reqwest::Result<Value> {
        self.client
            .patch(format!("{}{}", self.base_url, path))
            .header("content-type", "application/strategic-merge-patch+json")
            .json(body)
            .send()
            .await?
            .json()
            .await
    }

    /// PATCH with a **merge** patch (RFC 7386).
    ///
    /// Separate from `patch` because a CustomResourceDefinition does not
    /// accept a strategic-merge patch — strategic merge needs the Go struct
    /// tags that built-in types have and a CRD has not. rustkube-node's
    /// kubelet already patches a VMI's status this way; the scheduler writes
    /// to the same subresource and has to speak the same content type.
    pub async fn patch_merge(&self, path: &str, body: &Value) -> reqwest::Result<Value> {
        self.client
            .patch(format!("{}{}", self.base_url, path))
            .header("content-type", "application/merge-patch+json")
            .json(body)
            .send()
            .await?
            .json()
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
        self.usage.get(node_name_of(node)).copied().unwrap_or_default()
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
            self.api.clone(),
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
            elector.acquire().await;
            info!("Became leader; scheduling pods");
            crate::metrics_server::set_leader(true);
            let mut interval = time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                // Renew before each pass; step down immediately if we lost it.
                if !elector.try_acquire_or_renew().await {
                    warn!("Lost leadership; pausing scheduling");
                    crate::metrics_server::set_leader(false);
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

        // Collect all unscheduled, non-terminal pods across namespaces — and,
        // in the same pass, what each node has already promised.
        //
        // The already-bound pods are not a distraction from the work: without
        // them a node's free capacity is unknowable, every node looks empty
        // forever, and the cluster piles every pod onto whichever node scores
        // highest. The listing is already in hand, so this costs nothing.
        let mut pending: Vec<(String, Value)> = Vec::new();
        let mut state = ClusterState::default();
        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default").to_string();
            let pod_list: Value = self
                .api
                .list(&format!("/api/v1/namespaces/{ns_name}/pods"))
                .await?;
            for pod in pod_list["items"].as_array().cloned().unwrap_or_default() {
                let phase_now = pod["status"]["phase"].as_str().unwrap_or("Pending");
                let terminal = phase_now == "Succeeded" || phase_now == "Failed";
                if let Some(on) = pod["spec"]["nodeName"].as_str().filter(|s| !s.is_empty()) {
                    // A pod that has finished has given its request back.
                    if !terminal {
                        let (cpu, mem) = crate::filter::pod_requests(&pod);
                        let e = state.usage.entry(on.to_string()).or_default();
                        e.cpu_milli += cpu;
                        e.mem_bytes += mem;
                        state.placed.push((on.to_string(), pod.clone()));
                    }
                    continue; // already scheduled
                }
                if terminal {
                    continue;
                }
                pending.push((ns_name.clone(), pod));
            }
        }

        // Virtual machines, listed once for the whole cluster.
        //
        // **Before the pods are placed, not after.** A VM's memory is a hard
        // promise the hypervisor has already taken, so a node running two
        // 8 GiB guests has 16 GiB less to offer — and until now nothing in
        // this pass knew that, so pods were scheduled onto capacity a VM was
        // already holding. Folding them into the same `ClusterState` fixes
        // that in the same stroke as placing them.
        let pending_vms = self.collect_virtual_machines(&mut state).await;

        // Storage, listed once and only when something actually needs it. A
        // cluster with no PVCs pays nothing for this.
        let volumes = if pending.iter().any(|(_, p)| volumebinding::uses_storage(p)) {
            self.load_volume_state(&pending).await
        } else {
            volumebinding::VolumeState::default()
        };

        // PrioritySort: highest priority first, ties broken by creationTimestamp.
        pending.sort_by(|a, b| {
            pod_priority(&b.1)
                .cmp(&pod_priority(&a.1))
                .then_with(|| creation_ts(&a.1).cmp(&creation_ts(&b.1)))
        });

        crate::metrics_server::set_pending_pods(pending.len());

        for (ns_name, pod) in &pending {
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            let started = std::time::Instant::now();
            match self.schedule_pod(ns_name, pod, &nodes, &state, &volumes).await {
                Ok(Placement::Bound(node)) => {
                    crate::metrics_server::record_attempt("scheduled");
                    crate::metrics_server::record_e2e_latency(
                        started.elapsed().as_secs_f64(),
                        "scheduled",
                    );
                    info!("Scheduled pod {ns_name}/{pod_name} -> {node}")
                }
                Ok(Placement::WaitingForVolumes(node)) => {
                    // Not an attempt that failed and not one that succeeded:
                    // the pod is placed and waiting on storage, which upstream
                    // counts as unschedulable until the volume binds.
                    crate::metrics_server::record_attempt("unschedulable");
                    info!("Pod {ns_name}/{pod_name} will run on {node} once its volumes bind")
                }
                Err(e) => {
                    crate::metrics_server::record_attempt("unschedulable");
                    debug!("Failed to schedule pod {ns_name}/{pod_name}: {e}")
                }
            }
        }

        // Machines last, and each one charged to its node as it is placed.
        //
        // Pods in this pass do not see each other's placements — the next
        // pass re-reads from the apiserver a second later and corrects it,
        // and a pod that briefly overcommits a node is recoverable. A VM is
        // not: two 8 GiB guests placed on a node with 12 GiB free in the same
        // pass both get a node, and the second one fails to start with no
        // memory. So `state` is updated here between machines, which costs
        // nothing and is the difference between a VM that runs and one that
        // is scheduled onto a node that cannot hold it.
        let mut state = state;
        for vmi in &pending_vms {
            self.schedule_virtual_machine(vmi, &nodes, &mut state).await;
        }

        Ok(())
    }

    /// Every VMI in the cluster: the placed ones charged to their nodes, the
    /// unplaced ones returned to be scheduled.
    ///
    /// A cluster with no kubevirt CRD answers 404 here, which is not an error
    /// — it is a cluster with no VMs. Pod scheduling must not stop because of
    /// it, so the failure is logged once at debug and the pass carries on.
    async fn collect_virtual_machines(&self, state: &mut ClusterState) -> Vec<Value> {
        let list: Value = match self.api.list(virtualmachine::LIST_PATH).await {
            Ok(v) => v,
            Err(e) => {
                debug!("no virtualmachineinstances to schedule: {e}");
                return Vec::new();
            }
        };
        let mut pending = Vec::new();
        for vmi in list["items"].as_array().cloned().unwrap_or_default() {
            if virtualmachine::is_terminal(&vmi) {
                continue;
            }
            match virtualmachine::node_of(&vmi) {
                Some(node) => {
                    let (cpu, mem) = virtualmachine::requests(&vmi);
                    let e = state.usage.entry(node.to_string()).or_default();
                    e.cpu_milli += cpu;
                    e.mem_bytes += mem;
                    // As a shim, so inter-pod affinity and topology spread see
                    // a running VM the way they see a running pod — a pod that
                    // must not share a node with this VM has no way to say so
                    // otherwise.
                    state
                        .placed
                        .push((node.to_string(), virtualmachine::scheduling_shim(&vmi)));
                }
                None => pending.push(vmi),
            }
        }
        crate::metrics_server::set_pending_virtual_machines(pending.len());
        pending
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
            .filter(|node| {
                match filter::run_filters(&shim, node, state.used(node), state, nodes) {
                    FilterResult::Pass => true,
                    FilterResult::Fail(reason) => {
                        let n = node["metadata"]["name"].as_str().unwrap_or("?");
                        refused.push(format!("{n}: {reason}"));
                        false
                    }
                }
            })
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
            .map(|node| (*node, score::score_node(&shim, node, state.used(node), state, nodes)))
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
        let body = json!({"status": status});
        match self.api.patch_merge(&virtualmachine::status_path(ns, name), &body).await {
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
        if let Err(e) = self.api.patch_merge(&virtualmachine::status_path(ns, name), &body).await {
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
        if let Some(reason) =
            volumebinding::rwop_conflict(pod, namespace, volumes, &state.placed)
        {
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
                (*node, score::score_node(pod, node, state.used(node), state, nodes))
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
                let path = format!(
                    "/api/v1/namespaces/{namespace}/persistentvolumeclaims/{claim}"
                );
                let already = volumes
                    .claim(namespace, claim)
                    .and_then(|c| {
                        c["metadata"]["annotations"][volumebinding::ANN_SELECTED_NODE].as_str()
                    })
                    .unwrap_or("");
                if already == chosen_name {
                    continue;
                }
                let patch = json!({"metadata": {"annotations": {
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

    /// List everything the volume filter needs, once per pass.
    async fn load_volume_state(
        &self,
        pending: &[(String, Value)],
    ) -> volumebinding::VolumeState {
        let mut state = volumebinding::VolumeState::default();

        if let Ok(list) = self.api.list("/api/v1/persistentvolumes").await {
            for pv in list["items"].as_array().cloned().unwrap_or_default() {
                if let Some(name) = pv["metadata"]["name"].as_str() {
                    state.volumes.insert(name.to_string(), pv.clone());
                }
            }
        }
        if let Ok(list) = self.api.list("/apis/storage.k8s.io/v1/storageclasses").await {
            for sc in list["items"].as_array().cloned().unwrap_or_default() {
                if let Some(name) = sc["metadata"]["name"].as_str() {
                    state.classes.insert(name.to_string(), sc.clone());
                }
            }
        }
        if let Ok(list) = self.api.list("/apis/storage.k8s.io/v1/csidrivers").await {
            for d in list["items"].as_array().cloned().unwrap_or_default() {
                if d["spec"]["storageCapacity"].as_bool() == Some(true) {
                    if let Some(name) = d["metadata"]["name"].as_str() {
                        state.capacity_tracking.push(name.to_string());
                    }
                }
            }
        }
        if let Ok(list) = self
            .api
            .list("/apis/storage.k8s.io/v1/csistoragecapacities")
            .await
        {
            state.capacities = list["items"].as_array().cloned().unwrap_or_default();
        }

        // Only the namespaces that have a pending pod with storage.
        let mut namespaces: Vec<&str> = pending
            .iter()
            .filter(|(_, p)| volumebinding::uses_storage(p))
            .map(|(ns, _)| ns.as_str())
            .collect();
        namespaces.sort_unstable();
        namespaces.dedup();
        for ns in namespaces {
            if let Ok(list) = self
                .api
                .list(&format!(
                    "/api/v1/namespaces/{ns}/persistentvolumeclaims"
                ))
                .await
            {
                for pvc in list["items"].as_array().cloned().unwrap_or_default() {
                    if let Some(name) = pvc["metadata"]["name"].as_str() {
                        state
                            .claims
                            .insert((ns.to_string(), name.to_string()), pvc.clone());
                    }
                }
            }
        }
        state
    }
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
        assert_eq!(cpu, 2000, "must account the actuated size, not the desired one");
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
