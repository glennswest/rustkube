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
    // Extract pod resource requests
    let containers = pod["spec"]["containers"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut total_cpu_milli: u64 = 0;
    let mut total_mem_bytes: u64 = 0;

    for container in &containers {
        let requests = &container["resources"]["requests"];
        if let Some(cpu) = requests["cpu"].as_str() {
            total_cpu_milli += parse_cpu_millis(cpu);
        }
        if let Some(mem) = requests["memory"].as_str() {
            total_mem_bytes += parse_memory_bytes(mem);
        }
    }

    // If no resource requests, the pod fits anywhere
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

/// Parse Kubernetes CPU notation to millicores.
pub fn parse_cpu_millis(s: &str) -> u64 {
    if let Some(stripped) = s.strip_suffix('m') {
        stripped.parse().unwrap_or(0)
    } else {
        // Whole cores
        let cores: f64 = s.parse().unwrap_or(0.0);
        (cores * 1000.0) as u64
    }
}

/// Parse Kubernetes memory notation to bytes.
pub fn parse_memory_bytes(s: &str) -> u64 {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix("Ki") {
        stripped.parse::<u64>().unwrap_or(0) * 1024
    } else if let Some(stripped) = s.strip_suffix("Mi") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024
    } else if let Some(stripped) = s.strip_suffix("Gi") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024
    } else if let Some(stripped) = s.strip_suffix("Ti") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024 * 1024
    } else if let Some(stripped) = s.strip_suffix('K') {
        stripped.parse::<u64>().unwrap_or(0) * 1000
    } else if let Some(stripped) = s.strip_suffix('M') {
        stripped.parse::<u64>().unwrap_or(0) * 1_000_000
    } else if let Some(stripped) = s.strip_suffix('G') {
        stripped.parse::<u64>().unwrap_or(0) * 1_000_000_000
    } else if let Some(stripped) = s.strip_suffix('T') {
        stripped.parse::<u64>().unwrap_or(0) * 1_000_000_000_000
    } else {
        s.parse().unwrap_or(0)
    }
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

fn node_selector_term_matches(
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
