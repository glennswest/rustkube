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
pub fn run_filters(pod: &Value, node: &Value) -> FilterResult {
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

    // Filter 5: Resource fit
    if let FilterResult::Fail(reason) = resource_fit_filter(pod, node) {
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
fn taint_toleration_filter(pod: &Value, node: &Value) -> FilterResult {
    let taints = node["spec"]["taints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let tolerations = pod["spec"]["tolerations"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    for taint in &taints {
        let taint_effect = taint["effect"].as_str().unwrap_or("");
        // Only NoSchedule and NoExecute prevent scheduling
        if taint_effect != "NoSchedule" && taint_effect != "NoExecute" {
            continue;
        }

        let taint_key = taint["key"].as_str().unwrap_or("");
        let taint_value = taint["value"].as_str().unwrap_or("");

        let tolerated = tolerations.iter().any(|t| {
            let t_key = t["key"].as_str().unwrap_or("");
            let t_operator = t["operator"].as_str().unwrap_or("Equal");
            let t_value = t["value"].as_str().unwrap_or("");
            let t_effect = t["effect"].as_str().unwrap_or("");

            // Effect must match (or toleration has empty effect = match all)
            if !t_effect.is_empty() && t_effect != taint_effect {
                return false;
            }

            match t_operator {
                "Exists" => t_key == taint_key || t_key.is_empty(),
                _ => t_key == taint_key && t_value == taint_value,
            }
        });

        if !tolerated {
            return FilterResult::Fail(format!(
                "node has taint {taint_key}={taint_value}:{taint_effect} not tolerated"
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
fn resource_fit_filter(pod: &Value, node: &Value) -> FilterResult {
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
        let node_cpu = parse_cpu_millis(cpu_str);
        if total_cpu_milli > node_cpu {
            return FilterResult::Fail(format!(
                "insufficient CPU: requested {total_cpu_milli}m, available {node_cpu}m"
            ));
        }
    }

    if let Some(mem_str) = allocatable["memory"].as_str() {
        let node_mem = parse_memory_bytes(mem_str);
        if total_mem_bytes > node_mem {
            return FilterResult::Fail(format!(
                "insufficient memory: requested {total_mem_bytes}B, available {node_mem}B"
            ));
        }
    }

    FilterResult::Pass
}

/// Parse Kubernetes CPU notation to millicores.
fn parse_cpu_millis(s: &str) -> u64 {
    if let Some(stripped) = s.strip_suffix('m') {
        stripped.parse().unwrap_or(0)
    } else {
        // Whole cores
        let cores: f64 = s.parse().unwrap_or(0.0);
        (cores * 1000.0) as u64
    }
}

/// Parse Kubernetes memory notation to bytes.
fn parse_memory_bytes(s: &str) -> u64 {
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
