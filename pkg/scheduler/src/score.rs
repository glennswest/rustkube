//! Scheduling score plugins.
//!
//! Score each feasible node to find the best placement.
//! Higher score = better fit.

use crate::filter::NodeUsage;
use serde_json::Value;

/// Score a node for a given pod. Higher is better.
pub fn score_node(
    pod: &Value,
    node: &Value,
    used: NodeUsage,
    state: &crate::scheduler::ClusterState,
    nodes: &[Value],
) -> i64 {
    let mut total: i64 = 0;

    // Balance first: least-requested is what keeps a cluster from filling one
    // node while others idle, and it is the plugin every other score is a
    // refinement of.
    total += least_requested_score(pod, node, used);
    total += image_locality_score(pod, node);
    total += node_affinity_score(pod, node);
    // Preferred inter-pod affinity and anti-affinity.
    total += crate::affinity::pod_affinity_score(pod, node, nodes, state);
    // ScheduleAnyway topology spread — a preference for the emptier domain.
    total += crate::spread::spread_score(pod, node, nodes, state);

    total
}

/// Prefer nodes with more available resources (balanced allocation).
fn least_requested_score(_pod: &Value, node: &Value, used: NodeUsage) -> i64 {
    let allocatable = &node["status"]["allocatable"];
    if allocatable.is_null() {
        return 50; // Default mid-score if no resource info
    }

    let cpu = allocatable["cpu"]
        .as_str()
        .map(parse_cpu_millis)
        .unwrap_or(1000);
    let mem = allocatable["memory"]
        .as_str()
        .map(parse_memory_bytes)
        .unwrap_or(1024 * 1024 * 1024);

    // The *fraction* free, which is what "least requested" means. Scoring raw
    // allocatable — as this did — is "most resources wins", and it is not a
    // heuristic for spreading but the opposite of one: the biggest node always
    // scores highest and never scores lower as it fills, so every pod in the
    // cluster lands on it. On one node that is invisible; on several it is the
    // whole behaviour.
    let free_cpu = cpu.saturating_sub(used.cpu_milli);
    let free_mem = mem.saturating_sub(used.mem_bytes);
    let cpu_score = if cpu == 0 { 0 } else { (free_cpu * 50 / cpu) as i64 };
    let mem_score = if mem == 0 { 0 } else { (free_mem * 50 / mem) as i64 };

    cpu_score + mem_score
}

/// Prefer nodes that already have the pod's container images cached.
fn image_locality_score(pod: &Value, node: &Value) -> i64 {
    let containers = pod["spec"]["containers"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let node_images = node["status"]["images"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let node_image_names: Vec<&str> = node_images
        .iter()
        .flat_map(|img| {
            img["names"]
                .as_array()
                .map(|names| {
                    names
                        .iter()
                        .filter_map(|n| n.as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();

    let mut matched = 0;
    for container in &containers {
        if let Some(image) = container["image"].as_str() {
            if node_image_names.contains(&image) {
                matched += 1;
            }
        }
    }

    matched * 10 // 10 points per cached image
}

/// Score based on nodeAffinity preferred scheduling terms.
fn node_affinity_score(pod: &Value, node: &Value) -> i64 {
    let preferred = &pod["spec"]["affinity"]["nodeAffinity"]
        ["preferredDuringSchedulingIgnoredDuringExecution"];

    if preferred.is_null() || !preferred.is_array() {
        return 0;
    }

    let node_labels = node["metadata"]["labels"].as_object();
    let mut score: i64 = 0;

    if let Some(terms) = preferred.as_array() {
        for term in terms {
            let weight = term["weight"].as_i64().unwrap_or(1);
            let expressions = term["preference"]["matchExpressions"]
                .as_array()
                .cloned()
                .unwrap_or_default();

            let all_match = expressions.iter().all(|expr| {
                let key = expr["key"].as_str().unwrap_or("");
                let operator = expr["operator"].as_str().unwrap_or("In");
                let values: Vec<&str> = expr["values"]
                    .as_array()
                    .map(|vs| vs.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                match node_labels {
                    Some(labels) => {
                        let node_val = labels.get(key).and_then(|v| v.as_str());
                        match operator {
                            "In" => node_val.map(|v| values.contains(&v)).unwrap_or(false),
                            "NotIn" => node_val.map(|v| !values.contains(&v)).unwrap_or(true),
                            "Exists" => labels.contains_key(key),
                            "DoesNotExist" => !labels.contains_key(key),
                            _ => false,
                        }
                    }
                    None => matches!(operator, "DoesNotExist"),
                }
            });

            if all_match {
                score += weight;
            }
        }
    }

    score
}

fn parse_cpu_millis(s: &str) -> u64 {
    if let Some(stripped) = s.strip_suffix('m') {
        stripped.parse().unwrap_or(0)
    } else {
        let cores: f64 = s.parse().unwrap_or(0.0);
        (cores * 1000.0) as u64
    }
}

fn parse_memory_bytes(s: &str) -> u64 {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix("Ki") {
        stripped.parse::<u64>().unwrap_or(0) * 1024
    } else if let Some(stripped) = s.strip_suffix("Mi") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024
    } else if let Some(stripped) = s.strip_suffix("Gi") {
        stripped.parse::<u64>().unwrap_or(0) * 1024 * 1024 * 1024
    } else {
        s.parse().unwrap_or(0)
    }
}

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use crate::filter::NodeUsage;
    use crate::scheduler::ClusterState;
    use serde_json::json;

    fn node(name: &str, cpu: &str, mem: &str) -> Value {
        json!({"metadata":{"name":name,"labels":{"kubernetes.io/hostname":name}},
               "status":{"allocatable":{"cpu":cpu,"memory":mem}}})
    }

    #[test]
    fn a_filling_node_scores_lower_than_an_empty_one() {
        // The defect this replaced: the score read raw allocatable, so the
        // biggest node always won and never got less attractive as it filled.
        // Every pod in the cluster landed on it.
        let big = node("big", "8", "16Gi");
        let st = ClusterState::default();
        let pod = json!({"spec":{"containers":[]}});

        let empty = score_node(&pod, &big, NodeUsage::default(), &st, &[]);
        let half = score_node(
            &pod, &big,
            NodeUsage { cpu_milli: 4000, mem_bytes: 8 * 1024 * 1024 * 1024 },
            &st, &[],
        );
        assert!(half < empty, "a half-full node must score lower ({half} vs {empty})");
    }

    #[test]
    fn a_small_empty_node_beats_a_large_full_one() {
        // What makes spreading work at all: capacity is not the score, free
        // capacity is. Scoring on size alone made the largest node a magnet.
        let st = ClusterState::default();
        let pod = json!({"spec":{"containers":[]}});
        let small_empty = score_node(&pod, &node("s", "2", "4Gi"), NodeUsage::default(), &st, &[]);
        let big_full = score_node(
            &pod, &node("b", "64", "256Gi"),
            NodeUsage { cpu_milli: 63_000, mem_bytes: 250 * 1024 * 1024 * 1024 },
            &st, &[],
        );
        assert!(small_empty > big_full, "{small_empty} should beat {big_full}");
    }
}
