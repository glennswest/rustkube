//! Scheduling score plugins.
//!
//! Score each feasible node to find the best placement.
//! Higher score = better fit.

use serde_json::Value;

/// Score a node for a given pod. Higher is better.
pub fn score_node(pod: &Value, node: &Value) -> i64 {
    let mut total: i64 = 0;

    total += least_requested_score(pod, node);
    total += image_locality_score(pod, node);
    total += node_affinity_score(pod, node);

    total
}

/// Prefer nodes with more available resources (balanced allocation).
fn least_requested_score(_pod: &Value, node: &Value) -> i64 {
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

    // Simple heuristic: more resources = higher score
    let cpu_score = (cpu / 100).min(50) as i64;
    let mem_score = (mem / (256 * 1024 * 1024)).min(50) as i64;

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
