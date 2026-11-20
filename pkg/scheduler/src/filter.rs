//! Scheduling filter plugins.
//!
//! Filters determine which nodes are feasible for a pod.
//! A node passes all filters to be eligible for scheduling.

use serde_json::Value;

/// Result of running filters on a node.
#[derive(Debug)]
pub enum FilterResult {
    Pass,
    Fail(String),
}

/// What a node has already promised to the pods bound to it.
///
/// Milli-CPU and bytes, summed from the requests of every non-terminal pod
/// whose `spec.nodeName` names this node. Without it a node's capacity is read
/// as its `allocatable` forever: nothing ever fills up, every pod fits
/// everywhere, and a cluster overcommits without limit while reporting that
/// each node is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeUsage {
    pub cpu_milli: u64,
    pub mem_bytes: u64,
    /// Non-terminal Pods bound or assumed to the node, held against
    /// `allocatable.pods` (#194). A VMI takes no slot itself: a pod-network
    /// VMI's launcher Pod (#203) is the Pod that counts.
    pub pods: u64,
    /// Every other resource requested, by name: `ephemeral-storage`,
    /// `hugepages-*` and extended resources (`nvidia.com/gpu`,
    /// `example.com/foo`), in their base unit (#297). Upstream's
    /// NodeResourcesFit charges all of them; counting only cpu and memory put
    /// a Pod asking for 2 GPUs on a node that had none left.
    pub scalar: std::collections::BTreeMap<String, u64>,
}

/// Nothing promised: what a node with no Pods on it has used.
pub static NO_USAGE: NodeUsage = NodeUsage {
    cpu_milli: 0,
    mem_bytes: 0,
    pods: 0,
    scalar: std::collections::BTreeMap::new(),
};

impl NodeUsage {
    /// Charge a placed workload: its requests, and a Pod slot when `slot`
    /// (a VMI takes none, #194).
    pub fn charge(&mut self, pod: &Value, slot: bool) {
        let (cpu, mem) = pod_requests(pod);
        self.cpu_milli += cpu;
        self.mem_bytes += mem;
        self.pods += u64::from(slot);
        for (k, v) in pod_scalar_requests(pod) {
            *self.scalar.entry(k).or_default() += v;
        }
    }

    /// The inverse of `charge`. Saturating: a pod whose spec changed between
    /// its charge and uncharge (an in-place resize) must not wrap a count.
    pub fn uncharge(&mut self, pod: &Value, slot: bool) {
        let (cpu, mem) = pod_requests(pod);
        self.cpu_milli = self.cpu_milli.saturating_sub(cpu);
        self.mem_bytes = self.mem_bytes.saturating_sub(mem);
        self.pods = self.pods.saturating_sub(u64::from(slot));
        for (k, v) in pod_scalar_requests(pod) {
            if let Some(e) = self.scalar.get_mut(&k) {
                *e = e.saturating_sub(v);
                if *e == 0 {
                    self.scalar.remove(&k);
                }
            }
        }
    }
}

/// The upstream NodeResourcesFit "pods" check: a node holds at most
/// `allocatable.pods` non-terminal Pods (#194).
///
/// Separate from `run_filters` because a VMI shim goes through those too, and
/// a VM is not a Pod on the node; the Pod path runs it after them. It does
/// not sit behind resource fit's "requests nothing, fits anywhere" shortcut:
/// a BestEffort Pod still takes a slot, and that shortcut is how 1,000 of
/// them landed on a 110-pod node. A node with no `allocatable.pods` is not
/// limited.
pub fn pod_count_filter(node: &Value, used: &NodeUsage) -> FilterResult {
    let limit = match &node["status"]["allocatable"]["pods"] {
        Value::String(s) => s.trim().parse::<u64>().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    };
    match limit {
        Some(limit) if used.pods >= limit => FilterResult::Fail(TOO_MANY_PODS.into()),
        _ => FilterResult::Pass,
    }
}

/// Upstream's reason text, which clients and tests match on.
pub const TOO_MANY_PODS: &str = "Too many pods";

/// Run all filters on a pod-node pair.
pub fn run_filters(
    pod: &Value,
    node: &Value,
    used: &NodeUsage,
    state: &crate::scheduler::ClusterState,
    nodes: &[Value],
) -> FilterResult {
    // Filter 1: Node must be Ready
    if let FilterResult::Fail(reason) = node_ready_filter(node) {
        return FilterResult::Fail(reason);
    }

    // Filter 2: Node must not be unschedulable
    if let FilterResult::Fail(reason) = unschedulable_filter(node) {
        return FilterResult::Fail(reason);
    }

    // Filter 3: Taints/Tolerations
    if let FilterResult::Fail(reason) = taint_toleration_filter(pod, node) {
        return FilterResult::Fail(reason);
    }

    // Filter 4: Node selector
    if let FilterResult::Fail(reason) = node_selector_filter(pod, node) {
        return FilterResult::Fail(reason);
    }

    // Filter 5: Required nodeAffinity (enforces e.g. kubernetes.io/arch for multi-arch)
    if let FilterResult::Fail(reason) = node_affinity_filter(pod, node) {
        return FilterResult::Fail(reason);
    }

    // Filter 6: nodeName binding
    if let FilterResult::Fail(reason) = node_name_filter(pod, node) {
        return FilterResult::Fail(reason);
    }

    // Filter 7: inter-pod affinity and anti-affinity. Placement that depends
    // on where *other* pods are, which is what keeps replicas off one node.
    if let Err(reason) = crate::affinity::pod_affinity_filter(pod, node, nodes, state) {
        return FilterResult::Fail(reason);
    }

    // Filter 8: topology spread constraints.
    if let Err(reason) = crate::spread::spread_filter(pod, node, nodes, state) {
        return FilterResult::Fail(reason);
    }

    // Filter 9: Resource fit
    if let FilterResult::Fail(reason) = resource_fit_filter(pod, node, used) {
        return FilterResult::Fail(reason);
    }

    FilterResult::Pass
}

/// Check that the node has a Ready condition.
fn node_ready_filter(node: &Value) -> FilterResult {
    let conditions = node["status"]["conditions"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let is_ready = conditions.iter().any(|c| {
        c["type"].as_str() == Some("Ready") && c["status"].as_str() == Some("True")
    });

    if is_ready {
        FilterResult::Pass
    } else {
        FilterResult::Fail("node is not Ready".into())
    }
}

/// Check that the node is not marked unschedulable.
fn unschedulable_filter(node: &Value) -> FilterResult {
    if node["spec"]["unschedulable"].as_bool() == Some(true) {
        FilterResult::Fail("node is unschedulable (cordoned)".into())
    } else {
        FilterResult::Pass
    }
}

/// Check taints/tolerations.
///
/// The matching rules live in `apimachinery::taint` and are shared with the
/// node controller's eviction pass. **One ruleset, one implementation**: the
/// scheduler's own copy had already drifted — it treated an unrecognised
/// `operator` as `Equal`, so a typo tolerated a taint instead of failing
/// closed, which is the direction that puts a workload somewhere it was told
/// not to go.
fn taint_toleration_filter(pod: &Value, node: &Value) -> FilterResult {
    let taints = node["spec"]["taints"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    for taint in &taints {
        let effect = taint["effect"].as_str().unwrap_or("");
        // Only NoSchedule and NoExecute keep a pod off a node.
        // PreferNoSchedule is a preference and belongs to scoring.
        if effect != "NoSchedule" && effect != "NoExecute" {
            continue;
        }
        if apimachinery::taint::pod_tolerates(pod, taint).is_none() {
            return FilterResult::Fail(format!(
                "node has taint {}={}:{} not tolerated",
                taint["key"].as_str().unwrap_or(""),
                taint["value"].as_str().unwrap_or(""),
                effect
            ));
        }
    }

    FilterResult::Pass
}

/// Check pod's nodeSelector against node labels.
fn node_selector_filter(pod: &Value, node: &Value) -> FilterResult {
    let selector = &pod["spec"]["nodeSelector"];
    if selector.is_null() || !selector.is_object() {
        return FilterResult::Pass; // No selector = any node
    }

    let selector_map = selector.as_object().unwrap();
    let node_labels = node["metadata"]["labels"].as_object();

    match node_labels {
        Some(labels) => {
            for (k, v) in selector_map {
                if labels.get(k) != Some(v) {
                    return FilterResult::Fail(format!(
                        "node missing label {k}={}", v.as_str().unwrap_or("")
                    ));
                }
            }
            FilterResult::Pass
        }
        None => {
            if selector_map.is_empty() {
                FilterResult::Pass
            } else {
                FilterResult::Fail("node has no labels but pod has nodeSelector".into())
            }
        }
    }
}

/// Check that node has sufficient resources for the pod.
fn resource_fit_filter(pod: &Value, node: &Value, used: &NodeUsage) -> FilterResult {
    // The same rule the node accounting charges by (#73). Reading only the
    // container sum here let a pod that states its requests at pod level pass
    // as requesting nothing, and then be charged its real size once bound.
    let (total_cpu_milli, total_mem_bytes) = pod_requests(pod);
    let scalar = pod_scalar_requests(pod);

    // Requests nothing, so it fits anywhere.
    if total_cpu_milli == 0 && total_mem_bytes == 0 && scalar.is_empty() {
        return FilterResult::Pass;
    }

    let allocatable = &node["status"]["allocatable"];

    // Every other resource (#297), as upstream's NodeResourcesFit: a resource
    // the node does not advertise has 0 allocatable, so a Pod asking for a GPU
    // never lands on a node without one. `ephemeral-storage` is the exception
    // here, held to the rule cpu and memory follow below: checked only when
    // the node reports it, because a node that reports no figure is not known
    // to have none.
    for (name, &want) in &scalar {
        let have = &allocatable[name.as_str()];
        if name == "ephemeral-storage" && have.is_null() {
            continue;
        }
        let have = quantity(have);
        let free = have.saturating_sub(used.scalar.get(name).copied().unwrap_or(0));
        if want > free {
            return FilterResult::Fail(format!("Insufficient {name}"));
        }
    }

    // Check node allocatable
    if allocatable.is_null() {
        return FilterResult::Pass; // No allocatable info = assume it fits
    }

    if let Some(cpu_str) = allocatable["cpu"].as_str() {
        // Free, not total: what the node can still promise after the pods
        // already bound to it.
        let node_cpu = parse_cpu_millis(cpu_str).saturating_sub(used.cpu_milli);
        if total_cpu_milli > node_cpu {
            // Upstream's text, without the amounts: the reasons are counted
            // into a Pod's PodScheduled message, which is rewritten whenever
            // it changes, and free capacity changes with every bind (#194).
            return FilterResult::Fail("Insufficient cpu".into());
        }
    }

    if let Some(mem_str) = allocatable["memory"].as_str() {
        let node_mem = parse_memory_bytes(mem_str).saturating_sub(used.mem_bytes);
        if total_mem_bytes > node_mem {
            return FilterResult::Fail("Insufficient memory".into());
        }
    }

    FilterResult::Pass
}

/// A pod's total requests, in milli-CPU and bytes.
///
/// Init containers are counted as the *maximum* of any single one rather than
/// the sum: they run one at a time and are finished before the app containers
/// start, so summing them would reserve capacity no pod ever holds at once.
/// This is the upstream rule and it matters on a small node, where summing a
/// handful of init containers can make a pod unschedulable that would run.
///
/// This is the only definition. Resource fit, the per-node accounting and
/// preemption all read requests through it, because three copies of the rule
/// drifted apart once already (#73).
///
/// Plus `spec.overhead`, what the Pod's RuntimeClass costs beyond its
/// containers (#135), as upstream adds it.
pub fn pod_requests(pod: &Value) -> (u64, u64) {
    let (cpu, mem) = requests_without_overhead(pod);
    let o = &pod["spec"]["overhead"];
    (
        cpu + o["cpu"].as_str().map(parse_cpu_millis).unwrap_or(0),
        mem + o["memory"].as_str().map(parse_memory_bytes).unwrap_or(0),
    )
}

fn requests_without_overhead(pod: &Value) -> (u64, u64) {
    // Pod-level requests win outright when present.
    //
    // `spec.resources` on the pod (beta-on since v1.34) is the pod's total, and
    // upstream uses it in place of the container sum rather than in addition to
    // it. Summing both would double-count every pod that sets it.
    let pod_level = &pod["spec"]["resources"]["requests"];
    if !pod_level.is_null() {
        let cpu = pod_level["cpu"].as_str().map(parse_cpu_millis).unwrap_or(0);
        let mem = pod_level["memory"].as_str().map(parse_memory_bytes).unwrap_or(0);
        if cpu != 0 || mem != 0 {
            return (cpu, mem);
        }
    }
    // Containers, init containers and sidecars (#285): upstream's PodRequests.
    let total = apimachinery::pod_resources::containers_total(pod, "requests", |k, v| match (k, v.as_str()) {
        ("cpu", Some(q)) => parse_cpu_millis(q),
        ("memory", Some(q)) => parse_memory_bytes(q),
        _ => 0,
    });
    let mut cpu = total.get("cpu").copied().unwrap_or(0);
    let mut mem = total.get("memory").copied().unwrap_or(0);

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
            cpu = cpu.max(r["cpu"].as_str().map(parse_cpu_millis).unwrap_or(0));
            mem = mem.max(r["memory"].as_str().map(parse_memory_bytes).unwrap_or(0));
        }
    }
    (cpu, mem)
}

/// A Pod's requests of every resource but cpu, memory and pods (#297), in
/// base units: `ephemeral-storage` and `hugepages-*` in bytes, extended
/// resources as counts. Only resources asked for in a non-zero amount.
///
/// The rule `pod_requests` follows: containers summed, init containers by
/// their maximum with sidecars (`containers_total`); pod-level requests win
/// for what they name (upstream allows cpu, memory and hugepages there);
/// the larger of spec and a container's reported `resources` /
/// `allocatedResources`; plus `spec.overhead`.
pub fn pod_scalar_requests(pod: &Value) -> std::collections::BTreeMap<String, u64> {
    let scalar = |k: &str| !matches!(k, "cpu" | "memory" | "pods");
    let mut total = apimachinery::pod_resources::containers_total(pod, "requests", |k, v| {
        if scalar(k) { quantity(v) } else { 0 }
    });
    if let Some(pod_level) = pod["spec"]["resources"]["requests"].as_object() {
        for (k, v) in pod_level.iter().filter(|(k, _)| scalar(k.as_str())) {
            total.insert(k.clone(), quantity(v));
        }
    }
    for cs in pod["status"]["containerStatuses"].as_array().into_iter().flatten() {
        for field in ["resources", "allocatedResources"] {
            for (k, v) in cs[field]["requests"].as_object().into_iter().flatten() {
                if scalar(k.as_str()) {
                    let e = total.entry(k.clone()).or_default();
                    *e = (*e).max(quantity(v));
                }
            }
        }
    }
    for (k, v) in pod["spec"]["overhead"].as_object().into_iter().flatten() {
        if scalar(k.as_str()) {
            *total.entry(k.clone()).or_default() += quantity(v);
        }
    }
    total.retain(|_, v| *v > 0);
    total
}

/// A quantity in its base unit, written as a string (`2`, `1Gi`) or, as some
/// writers store it, a JSON number (#292). Anything else is 0.
fn quantity(v: &Value) -> u64 {
    match v {
        Value::String(s) => parse_memory_bytes(s),
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0),
        _ => 0,
    }
}

/// Parse Kubernetes CPU notation to millicores.
pub fn parse_cpu_millis(s: &str) -> u64 {
    apimachinery::quantity::parse_cpu_millis(s)
}

/// Parse Kubernetes memory notation to bytes.
pub fn parse_memory_bytes(s: &str) -> u64 {
    apimachinery::quantity::parse_bytes(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn pod_count_stops_at_allocatable_pods() {
        let node = |pods: Value| json!({"status": {"allocatable": {"pods": pods}}});
        let used = |pods| NodeUsage { pods, ..Default::default() };
        assert!(matches!(pod_count_filter(&node(json!("110")), &used(109)), FilterResult::Pass));
        assert!(matches!(
            pod_count_filter(&node(json!("110")), &used(110)),
            FilterResult::Fail(r) if r == TOO_MANY_PODS
        ));
        assert!(matches!(pod_count_filter(&node(json!(2)), &used(2)), FilterResult::Fail(_)));
        // No allocatable.pods reported: not limited.
        assert!(matches!(pod_count_filter(&json!({}), &used(1000)), FilterResult::Pass));
    }

    #[test]
    fn test_parse_cpu() {
        assert_eq!(parse_cpu_millis("100m"), 100);
        assert_eq!(parse_cpu_millis("1"), 1000);
        assert_eq!(parse_cpu_millis("0.5"), 500);
        assert_eq!(parse_cpu_millis("2"), 2000);
    }

    #[test]
    fn test_parse_memory() {
        assert_eq!(parse_memory_bytes("128Mi"), 128 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("1Gi"), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("512Ki"), 512 * 1024);
        assert_eq!(parse_memory_bytes("1000000"), 1_000_000);
    }

    #[test]
    fn test_node_ready_filter() {
        let node = json!({
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}]
            }
        });
        assert!(matches!(node_ready_filter(&node), FilterResult::Pass));

        let not_ready = json!({
            "status": {
                "conditions": [{"type": "Ready", "status": "False"}]
            }
        });
        assert!(matches!(node_ready_filter(&not_ready), FilterResult::Fail(_)));
    }

    #[test]
    fn test_taint_toleration() {
        let pod = json!({"spec": {}});
        let tainted_node = json!({
            "spec": {
                "taints": [{"key": "node-role.kubernetes.io/master", "effect": "NoSchedule"}]
            }
        });
        assert!(matches!(
            taint_toleration_filter(&pod, &tainted_node),
            FilterResult::Fail(_)
        ));

        // Pod tolerates the taint
        let tolerant_pod = json!({
            "spec": {
                "tolerations": [{
                    "key": "node-role.kubernetes.io/master",
                    "operator": "Exists",
                    "effect": "NoSchedule"
                }]
            }
        });
        assert!(matches!(
            taint_toleration_filter(&tolerant_pod, &tainted_node),
            FilterResult::Pass
        ));
    }

    #[test]
    fn test_node_selector() {
        let pod = json!({
            "spec": {
                "nodeSelector": {"disk": "ssd"}
            }
        });
        let matching_node = json!({
            "metadata": {"labels": {"disk": "ssd", "zone": "us-east"}}
        });
        assert!(matches!(
            node_selector_filter(&pod, &matching_node),
            FilterResult::Pass
        ));

        let non_matching = json!({
            "metadata": {"labels": {"disk": "hdd"}}
        });
        assert!(matches!(
            node_selector_filter(&pod, &non_matching),
            FilterResult::Fail(_)
        ));
    }

    fn full_node() -> Value {
        json!({
            "metadata": {"name": "n1"},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "allocatable": {"cpu": "8", "memory": "4Gi"}
            }
        })
    }

    #[test]
    fn pod_level_requests_are_refused_by_a_node_that_cannot_hold_them() {
        // #73: bare containers, the whole request at pod level. Resource fit
        // read this as requesting nothing and passed it on a full node.
        let pod = json!({"spec": {
            "resources": {"requests": {"memory": "8Gi"}},
            "containers": [{"name": "app"}]
        }});
        assert!(matches!(
            resource_fit_filter(&pod, &full_node(), &NodeUsage::default()),
            FilterResult::Fail(_)
        ));
        // And it fits once the node has room, so it is the size being read.
        let small = json!({"spec": {
            "resources": {"requests": {"memory": "1Gi"}},
            "containers": [{"name": "app"}]
        }});
        assert!(matches!(
            resource_fit_filter(&small, &full_node(), &NodeUsage::default()),
            FilterResult::Pass
        ));
    }

    #[test]
    fn overhead_is_added_to_the_requests() {
        let pod = json!({"spec": {"overhead": {"cpu": "250m", "memory": "120Mi"},
            "containers": [{"name": "app", "resources": {"requests": {"cpu": "100m", "memory": "8Mi"}}}]}});
        assert_eq!(pod_requests(&pod), (350, 128 * 1024 * 1024));
    }

    #[test]
    fn resource_fit_counts_an_init_container_larger_than_the_app() {
        // The filter summed app containers only; the accounting takes the
        // larger of that and the biggest init container. Same rule now.
        let pod = json!({"spec": {
            "initContainers": [{"name": "i", "resources": {"requests": {"memory": "6Gi"}}}],
            "containers": [{"name": "app", "resources": {"requests": {"memory": "1Gi"}}}]
        }});
        assert!(matches!(
            resource_fit_filter(&pod, &full_node(), &NodeUsage::default()),
            FilterResult::Fail(_)
        ));
    }

    /// Sidecars (#285, 1.33): upstream's PodRequests. A `restartPolicy:
    /// Always` init container adds to the pod's total; a later ordinary init
    /// container runs beside the sidecars started before it.
    #[test]
    fn sidecars_add_to_the_pod_and_to_later_init_steps() {
        let req = |cpu: &str| json!({"requests": {"cpu": cpu}});
        let pod = json!({"spec": {
            "initContainers": [
                {"name": "setup", "resources": req("300m")},
                {"name": "proxy", "restartPolicy": "Always", "resources": req("200m")},
                {"name": "migrate", "resources": req("500m")}
            ],
            "containers": [{"name": "app", "resources": req("400m")}]
        }});
        // Running: app 400 + proxy 200 = 600; busiest init step: migrate 500
        // beside proxy 200 = 700.
        assert_eq!(pod_requests(&pod).0, 700);
        let lighter = json!({"spec": {
            "initContainers": [{"name": "proxy", "restartPolicy": "Always", "resources": req("200m")}],
            "containers": [{"name": "app", "resources": req("400m")}]
        }});
        assert_eq!(pod_requests(&lighter).0, 600, "a sidecar is summed, not maxed");
        let plain = json!({"spec": {
            "initContainers": [{"name": "i", "resources": req("200m")}],
            "containers": [{"name": "app", "resources": req("400m")}]
        }});
        assert_eq!(pod_requests(&plain).0, 400, "an ordinary init container is maxed");
    }
}

/// Enforce `requiredDuringSchedulingIgnoredDuringExecution` nodeAffinity: the
/// node must match at least one nodeSelectorTerm (OR across terms; AND across a
/// term's matchExpressions). This is what enforces `kubernetes.io/arch` for
/// multi-arch scheduling once admission injects the arch nodeAffinity.
fn node_affinity_filter(pod: &Value, node: &Value) -> FilterResult {
    let terms = pod["spec"]["affinity"]["nodeAffinity"]
        ["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
        .as_array();
    let terms = match terms {
        Some(t) if !t.is_empty() => t,
        _ => return FilterResult::Pass, // no required affinity
    };
    let labels = node["metadata"]["labels"].as_object();
    let node_name = node["metadata"]["name"].as_str().unwrap_or("");
    for term in terms {
        if node_selector_term_matches(term, labels, node_name) {
            return FilterResult::Pass;
        }
    }
    FilterResult::Fail("node does not match required nodeAffinity".into())
}

pub(crate) fn node_selector_term_matches(
    term: &Value,
    labels: Option<&serde_json::Map<String, Value>>,
    node_name: &str,
) -> bool {
    if let Some(exprs) = term["matchExpressions"].as_array() {
        for e in exprs {
            let key = e["key"].as_str().unwrap_or("");
            let node_val = labels.and_then(|l| l.get(key));
            if !match_expression(e, node_val) {
                return false;
            }
        }
    }
    // matchFields — only metadata.name is meaningful.
    if let Some(fields) = term["matchFields"].as_array() {
        for f in fields {
            if f["key"].as_str() != Some("metadata.name") {
                continue;
            }
            let op = f["operator"].as_str().unwrap_or("");
            let values: Vec<&str> = f["values"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let ok = match op {
                "In" => values.contains(&node_name),
                "NotIn" => !values.contains(&node_name),
                _ => true,
            };
            if !ok {
                return false;
            }
        }
    }
    true
}

fn match_expression(expr: &Value, node_val: Option<&Value>) -> bool {
    let op = expr["operator"].as_str().unwrap_or("");
    let values: Vec<&str> = expr["values"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let val = node_val.and_then(|v| v.as_str());
    match op {
        "In" => val.map(|v| values.contains(&v)).unwrap_or(false),
        "NotIn" => val.map(|v| !values.contains(&v)).unwrap_or(true),
        "Exists" => node_val.is_some(),
        "DoesNotExist" => node_val.is_none(),
        "Gt" => val
            .and_then(|v| v.parse::<i64>().ok())
            .zip(values.first().and_then(|s| s.parse::<i64>().ok()))
            .map(|(a, b)| a > b)
            .unwrap_or(false),
        "Lt" => val
            .and_then(|v| v.parse::<i64>().ok())
            .zip(values.first().and_then(|s| s.parse::<i64>().ok()))
            .map(|(a, b)| a < b)
            .unwrap_or(false),
        _ => true,
    }
}

/// If the pod is already bound to a node (`spec.nodeName`), only that node fits.
fn node_name_filter(pod: &Value, node: &Value) -> FilterResult {
    let want = pod["spec"]["nodeName"].as_str().unwrap_or("");
    if want.is_empty() {
        return FilterResult::Pass;
    }
    if node["metadata"]["name"].as_str() == Some(want) {
        FilterResult::Pass
    } else {
        FilterResult::Fail(format!("pod is bound to node {want}"))
    }

}

#[cfg(test)]
mod affinity_tests {
    use super::*;
    use serde_json::json;

    fn node(arch: &str) -> Value {
        json!({"metadata":{"name":"n1","labels":{"kubernetes.io/arch":arch}},
               "status":{"conditions":[{"type":"Ready","status":"True"}]},
               "spec":{}})
    }
    fn arch_pod(arches: &[&str]) -> Value {
        json!({"spec":{"affinity":{"nodeAffinity":{"requiredDuringSchedulingIgnoredDuringExecution":
            {"nodeSelectorTerms":[{"matchExpressions":[
                {"key":"kubernetes.io/arch","operator":"In","values":arches}]}]}}}}})
    }

    #[test]
    fn arch_affinity_filters() {
        // amd64-only pod fits amd64 node, not arm64 node.
        assert!(matches!(node_affinity_filter(&arch_pod(&["amd64"]), &node("amd64")), FilterResult::Pass));
        assert!(matches!(node_affinity_filter(&arch_pod(&["amd64"]), &node("arm64")), FilterResult::Fail(_)));
        // multi-arch pod fits either.
        assert!(matches!(node_affinity_filter(&arch_pod(&["amd64","arm64"]), &node("arm64")), FilterResult::Pass));
        // no affinity → passes.
        assert!(matches!(node_affinity_filter(&json!({"spec":{}}), &node("arm64")), FilterResult::Pass));
    }
}

#[cfg(test)]
mod scalar_tests {
    //! #297: every resource beyond cpu, memory and pods is counted.
    use super::*;
    use serde_json::json;

    fn node(alloc: Value) -> Value {
        json!({"metadata":{"name":"n1"},
               "status":{"conditions":[{"type":"Ready","status":"True"}],"allocatable":alloc}})
    }
    fn wants(res: Value) -> Value {
        json!({"spec":{"containers":[{"name":"c","resources":{"requests":res}}]}})
    }
    fn reason(r: FilterResult) -> String {
        match r {
            FilterResult::Pass => "pass".into(),
            FilterResult::Fail(s) => s,
        }
    }

    #[test]
    fn an_extended_resource_the_node_lacks_does_not_fit() {
        let pod = wants(json!({"example.com/foo": "1"}));
        assert_eq!(reason(resource_fit_filter(&pod, &node(json!({"cpu":"4"})), &NO_USAGE)), "Insufficient example.com/foo");
        // No allocatable at all: still none of it.
        let bare = json!({"metadata":{"name":"n1"},"status":{}});
        assert_eq!(reason(resource_fit_filter(&pod, &bare, &NO_USAGE)), "Insufficient example.com/foo");
        assert_eq!(reason(resource_fit_filter(&pod, &node(json!({"example.com/foo":"1"})), &NO_USAGE)), "pass");
        // A JSON number, as some writers store it (#292).
        assert_eq!(reason(resource_fit_filter(&pod, &node(json!({"example.com/foo":1})), &NO_USAGE)), "pass");
    }

    #[test]
    fn two_fit_and_the_third_waits() {
        // The issue's rig in miniature: a node with 2, Pods of 1.
        let n = node(json!({"cpu":"4","memory":"4Gi","example.com/foo":"2"}));
        let pod = wants(json!({"example.com/foo": "1"}));
        let mut used = NodeUsage::default();
        for _ in 0..2 {
            assert_eq!(reason(resource_fit_filter(&pod, &n, &used)), "pass");
            used.charge(&pod, true);
        }
        assert_eq!(used.scalar.get("example.com/foo"), Some(&2));
        assert_eq!(reason(resource_fit_filter(&pod, &n, &used)), "Insufficient example.com/foo");
        used.uncharge(&pod, true);
        assert_eq!(reason(resource_fit_filter(&pod, &n, &used)), "pass");
        used.uncharge(&pod, true);
        assert_eq!(used, NodeUsage::default(), "uncharge leaves no empty entries");
    }

    #[test]
    fn ephemeral_storage_is_checked_when_the_node_reports_it() {
        let pod = wants(json!({"ephemeral-storage": "2Gi"}));
        assert_eq!(reason(resource_fit_filter(&pod, &node(json!({"ephemeral-storage":"1Gi"})), &NO_USAGE)), "Insufficient ephemeral-storage");
        assert_eq!(reason(resource_fit_filter(&pod, &node(json!({"ephemeral-storage":"10Gi"})), &NO_USAGE)), "pass");
        assert_eq!(reason(resource_fit_filter(&pod, &node(json!({"cpu":"4"})), &NO_USAGE)), "pass");
    }

    #[test]
    fn scalar_requests_follow_the_pod_requests_rule() {
        // Init containers by their max, app containers summed, overhead added.
        let pod = json!({"spec":{
            "initContainers":[
                {"name":"i1","resources":{"requests":{"example.com/foo":"3"}}},
                {"name":"i2","resources":{"requests":{"example.com/foo":"1"}}}],
            "containers":[
                {"name":"a","resources":{"requests":{"example.com/foo":"1","cpu":"1"}}},
                {"name":"b","resources":{"requests":{"example.com/foo":"1","ephemeral-storage":"1Ki"}}}],
            "overhead":{"ephemeral-storage":"1Ki","cpu":"100m"}}});
        let r = pod_scalar_requests(&pod);
        assert_eq!(r.get("example.com/foo"), Some(&3));
        assert_eq!(r.get("ephemeral-storage"), Some(&2048));
        assert!(!r.contains_key("cpu"));
        // A sidecar runs beside the app containers: added, not maxed.
        let side = json!({"spec":{
            "initContainers":[{"name":"s","restartPolicy":"Always","resources":{"requests":{"example.com/foo":"1"}}}],
            "containers":[{"name":"a","resources":{"requests":{"example.com/foo":"1"}}}]}});
        assert_eq!(pod_scalar_requests(&side).get("example.com/foo"), Some(&2));
        // Requests of zero are no request.
        assert!(pod_scalar_requests(&wants(json!({"example.com/foo":"0"}))).is_empty());
    }

    #[test]
    fn a_pod_requesting_only_an_extended_resource_is_not_best_effort_here() {
        // The "requests nothing, fits anywhere" shortcut must not pass it.
        let pod = wants(json!({"nvidia.com/gpu": "2"}));
        let n = node(json!({"cpu":"64","memory":"256Gi","nvidia.com/gpu":"1"}));
        assert_eq!(reason(resource_fit_filter(&pod, &n, &NO_USAGE)), "Insufficient nvidia.com/gpu");
    }
}
