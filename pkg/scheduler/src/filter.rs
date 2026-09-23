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

/// Run all filter plugins on a pod-node pair.
/// What a node has already promised to the pods bound to it.
///
/// Milli-CPU and bytes, summed from the requests of every non-terminal pod
/// whose `spec.nodeName` names this node. Without it a node's capacity is read
/// as its `allocatable` forever: nothing ever fills up, every pod fits
/// everywhere, and a cluster overcommits without limit while reporting that
/// each node is empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeUsage {
    pub cpu_milli: u64,
    pub mem_bytes: u64,
}

pub fn run_filters(
    pod: &Value,
    node: &Value,
    used: NodeUsage,
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
fn resource_fit_filter(pod: &Value, node: &Value, used: NodeUsage) -> FilterResult {
    // The same rule the node accounting charges by (#73). Reading only the
    // container sum here let a pod that states its requests at pod level pass
    // as requesting nothing, and then be charged its real size once bound.
    let (total_cpu_milli, total_mem_bytes) = pod_requests(pod);

    // Requests nothing, so it fits anywhere.
    if total_cpu_milli == 0 && total_mem_bytes == 0 {
        return FilterResult::Pass;
    }

    // Check node allocatable
    let allocatable = &node["status"]["allocatable"];
    if allocatable.is_null() {
        return FilterResult::Pass; // No allocatable info = assume it fits
    }

    if let Some(cpu_str) = allocatable["cpu"].as_str() {
        // Free, not total: what the node can still promise after the pods
        // already bound to it.
        let node_cpu = parse_cpu_millis(cpu_str).saturating_sub(used.cpu_milli);
        if total_cpu_milli > node_cpu {
            return FilterResult::Fail(format!(
                "insufficient CPU: requested {total_cpu_milli}m, free {node_cpu}m \
                 ({}m already requested)",
                used.cpu_milli
            ));
        }
    }

    if let Some(mem_str) = allocatable["memory"].as_str() {
        let node_mem = parse_memory_bytes(mem_str).saturating_sub(used.mem_bytes);
        if total_mem_bytes > node_mem {
            return FilterResult::Fail(format!(
                "insufficient memory: requested {total_mem_bytes}B, free {node_mem}B \
                 ({}B already requested)",
                used.mem_bytes
            ));
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
pub fn pod_requests(pod: &Value) -> (u64, u64) {
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
    let sum = |list: &Value| -> (u64, u64) {
        let mut cpu = 0u64;
        let mut mem = 0u64;
        for c in list.as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
            let r = &c["resources"]["requests"];
            if let Some(v) = r["cpu"].as_str() {
                cpu += parse_cpu_millis(v);
            }
            if let Some(v) = r["memory"].as_str() {
                mem += parse_memory_bytes(v);
            }
        }
        (cpu, mem)
    };
    let (mut cpu, mut mem) = sum(&pod["spec"]["containers"]);
    let mut init_cpu = 0u64;
    let mut init_mem = 0u64;
    for c in pod["spec"]["initContainers"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let r = &c["resources"]["requests"];
        init_cpu = init_cpu.max(r["cpu"].as_str().map(parse_cpu_millis).unwrap_or(0));
        init_mem =
            init_mem.max(r["memory"].as_str().map(parse_memory_bytes).unwrap_or(0));
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
            cpu = cpu.max(r["cpu"].as_str().map(parse_cpu_millis).unwrap_or(0));
            mem = mem.max(r["memory"].as_str().map(parse_memory_bytes).unwrap_or(0));
        }
    }
    (cpu, mem)
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
            resource_fit_filter(&pod, &full_node(), NodeUsage::default()),
            FilterResult::Fail(_)
        ));
        // And it fits once the node has room, so it is the size being read.
        let small = json!({"spec": {
            "resources": {"requests": {"memory": "1Gi"}},
            "containers": [{"name": "app"}]
        }});
        assert!(matches!(
            resource_fit_filter(&small, &full_node(), NodeUsage::default()),
            FilterResult::Pass
        ));
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
            resource_fit_filter(&pod, &full_node(), NodeUsage::default()),
            FilterResult::Fail(_)
        ));
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
