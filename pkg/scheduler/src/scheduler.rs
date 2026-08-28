//! Core scheduler loop.
//!
//! Watches for pods without a nodeName, runs filter and score plugins,
//! then binds the pod to the best node via the API server.

use crate::filter::{self, FilterResult, NodeUsage};
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

/// A pod's total requests, in milli-CPU and bytes.
///
/// Init containers are counted as the *maximum* of any single one rather than
/// the sum: they run one at a time and are finished before the app containers
/// start, so summing them would reserve capacity no pod ever holds at once.
/// This is the upstream rule and it matters on a small node, where summing a
/// handful of init containers can make a pod unschedulable that would run.
fn pod_requests(pod: &Value) -> (u64, u64) {
    // Pod-level requests win outright when present.
    //
    // `spec.resources` on the pod (beta-on since v1.34) is the pod's total, and
    // upstream uses it in place of the container sum rather than in addition to
    // it. Summing both would double-count every pod that sets it.
    let pod_level = &pod["spec"]["resources"]["requests"];
    if !pod_level.is_null() {
        let cpu = pod_level["cpu"].as_str().map(crate::filter::parse_cpu_millis).unwrap_or(0);
        let mem = pod_level["memory"].as_str().map(crate::filter::parse_memory_bytes).unwrap_or(0);
        if cpu != 0 || mem != 0 {
            return (cpu, mem);
        }
    }
    let sum = |list: &Value| -> (u64, u64) {
        let mut cpu = 0u64;
        let mut mem = 0u64;
        for c in list.as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
            let r = &c["resources"]["requests"];
            if let Some(v) = r["cpu"].as_str() {
                cpu += crate::filter::parse_cpu_millis(v);
            }
            if let Some(v) = r["memory"].as_str() {
                mem += crate::filter::parse_memory_bytes(v);
            }
        }
        (cpu, mem)
    };
    let (mut cpu, mut mem) = sum(&pod["spec"]["containers"]);
    let mut init_cpu = 0u64;
    let mut init_mem = 0u64;
    for c in pod["spec"]["initContainers"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let r = &c["resources"]["requests"];
        init_cpu = init_cpu.max(r["cpu"].as_str().map(crate::filter::parse_cpu_millis).unwrap_or(0));
        init_mem =
            init_mem.max(r["memory"].as_str().map(crate::filter::parse_memory_bytes).unwrap_or(0));
    }
    cpu = cpu.max(init_cpu);
    mem = mem.max(init_mem);

    // What the pod actually holds, which is not always what its spec asks for.
    //
    // In-place pod resize is GA-locked as of v1.35, so `spec` is a *request to
    // become* that size and the kubelet actuates it asynchronously. On a shrink
    // the pod still holds the larger amount until it does, and a scheduler that
    // believes the spec will hand the difference to somebody else and overcommit
    // the node. Upstream's rule is the maximum of the desired, the actuated and
    // the allocated — the pessimistic one, which is the only safe direction when
    // the three disagree.
    for (i, cs) in pod["status"]["containerStatuses"]
        .as_array().map(|v| v.as_slice()).unwrap_or(&[]).iter().enumerate()
    {
        let _ = i;
        for field in ["resources", "allocatedResources"] {
            let r = &cs[field]["requests"];
            if r.is_null() {
                continue;
            }
            // Per container, so this is a floor on the total rather than an
            // exact sum — which is the safe side of the same argument.
            cpu = cpu.max(r["cpu"].as_str().map(crate::filter::parse_cpu_millis).unwrap_or(0));
            mem = mem.max(r["memory"].as_str().map(crate::filter::parse_memory_bytes).unwrap_or(0));
        }
    }
    (cpu, mem)
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
                        let (cpu, mem) = pod_requests(&pod);
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

        // PrioritySort: highest priority first, ties broken by creationTimestamp.
        pending.sort_by(|a, b| {
            pod_priority(&b.1)
                .cmp(&pod_priority(&a.1))
                .then_with(|| creation_ts(&a.1).cmp(&creation_ts(&b.1)))
        });

        for (ns_name, pod) in &pending {
            let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
            match self.schedule_pod(ns_name, pod, &nodes, &state).await {
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
        state: &ClusterState,
    ) -> anyhow::Result<String> {
        let pod_name = pod["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("pod missing name"))?;

        // Phase 1: Filter — find nodes that can run this pod
        let feasible: Vec<&Value> = nodes
            .iter()
            .filter(|node| {
                let result = filter::run_filters(pod, node, state.used(node), state, nodes);
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

#[cfg(test)]
mod accounting_tests {
    use super::*;
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
