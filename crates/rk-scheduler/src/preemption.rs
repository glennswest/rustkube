//! Preemption logic for the scheduler.
//!
//! When no node can fit a pod, try to evict lower-priority pods to make room.

use serde_json::Value;
use tracing::{debug, info};

/// Result of preemption analysis for a pod.
#[derive(Debug, Clone)]
pub struct PreemptionCandidate {
    /// Node where preemption should occur.
    pub node_name: String,
    /// Pod names that should be evicted.
    pub victims: Vec<String>,
}

/// Find pods that can be preempted to make room for the given pod.
///
/// Returns the node and victim pods that would allow the pod to schedule,
/// preferring the option with the fewest evictions and lowest-priority victims.
pub fn find_preemption_candidates(
    pod: &Value,
    nodes: &[Value],
    all_pods: &[Value],
) -> Option<PreemptionCandidate> {
    let pod_priority = priority_of(pod);

    debug!(
        "Finding preemption candidates for pod {} with priority {}",
        pod["metadata"]["name"].as_str().unwrap_or("unknown"),
        pod_priority
    );

    let mut best_candidate: Option<PreemptionCandidate> = None;
    let mut best_victim_count = usize::MAX;
    let mut best_victim_priority = i32::MAX;

    for node in nodes {
        let node_name = match node["metadata"]["name"].as_str() {
            Some(name) => name,
            None => continue,
        };

        let node_pods = pods_on_node(all_pods, node_name);

        // Find pods that could be evicted (lower priority than incoming pod)
        let mut evictable: Vec<&Value> = node_pods
            .iter()
            .filter(|p| priority_of(p) < pod_priority)
            .copied()
            .collect();

        // Sort by priority (lowest first) to minimize disruption
        evictable.sort_by_key(|p| priority_of(p));

        // Try to find minimal set of victims that would allow the pod to fit
        for victim_count in 0..=evictable.len() {
            let victims = &evictable[0..victim_count];
            let victim_names: Vec<String> = victims
                .iter()
                .filter_map(|p| p["metadata"]["name"].as_str().map(String::from))
                .collect();

            // Remaining pods after eviction
            let remaining: Vec<&Value> = node_pods
                .iter()
                .filter(|p| {
                    let name = p["metadata"]["name"].as_str().unwrap_or("");
                    !victim_names.contains(&name.to_string())
                })
                .copied()
                .collect();

            if can_fit_after_eviction(pod, node, &remaining) {
                let victim_priority = victims.iter().map(|p| priority_of(p)).max().unwrap_or(0);

                // Prefer fewer victims, or lower priority victims
                let is_better = victim_count < best_victim_count
                    || (victim_count == best_victim_count && victim_priority < best_victim_priority);

                if is_better {
                    info!(
                        "Found preemption candidate on node {} (victims: {}, priority: {})",
                        node_name,
                        victim_count,
                        victim_priority
                    );

                    best_candidate = Some(PreemptionCandidate {
                        node_name: node_name.to_string(),
                        victims: victim_names,
                    });
                    best_victim_count = victim_count;
                    best_victim_priority = victim_priority;
                }

                break; // Found minimal set for this node
            }
        }
    }

    best_candidate
}

/// Extract priority from pod spec (default 0).
fn priority_of(pod: &Value) -> i32 {
    pod["spec"]["priority"].as_i64().unwrap_or(0) as i32
}

/// Filter pods running on the given node.
fn pods_on_node<'a>(pods: &'a [Value], node_name: &str) -> Vec<&'a Value> {
    pods.iter()
        .filter(|p| p["spec"]["nodeName"].as_str() == Some(node_name))
        .collect()
}

/// Check if the pod would fit on the node after evicting the given pods.
fn can_fit_after_eviction(pod: &Value, node: &Value, remaining_pods: &[&Value]) -> bool {
    // Extract requested resources
    let pod_cpu = requested_cpu(pod);
    let pod_mem = requested_memory(pod);

    // Extract node capacity
    let node_cpu = node["status"]["allocatable"]["cpu"]
        .as_str()
        .and_then(parse_cpu_millicores)
        .unwrap_or(0);
    let node_mem = node["status"]["allocatable"]["memory"]
        .as_str()
        .and_then(parse_memory_bytes)
        .unwrap_or(0);

    // Calculate used resources from remaining pods
    let used_cpu: i64 = remaining_pods.iter().map(|p| requested_cpu(p)).sum();
    let used_mem: i64 = remaining_pods.iter().map(|p| requested_memory(p)).sum();

    let available_cpu = node_cpu.saturating_sub(used_cpu);
    let available_mem = node_mem.saturating_sub(used_mem);

    debug!(
        "Resource check: pod needs cpu={} mem={}, node has available cpu={} mem={}",
        pod_cpu, pod_mem, available_cpu, available_mem
    );

    pod_cpu <= available_cpu && pod_mem <= available_mem
}

/// Sum requested CPU from all containers (in millicores).
fn requested_cpu(pod: &Value) -> i64 {
    let empty = vec![];
    let containers = pod["spec"]["containers"].as_array().unwrap_or(&empty);
    containers
        .iter()
        .filter_map(|c| {
            c["resources"]["requests"]["cpu"]
                .as_str()
                .and_then(parse_cpu_millicores)
        })
        .sum()
}

/// Sum requested memory from all containers (in bytes).
fn requested_memory(pod: &Value) -> i64 {
    let empty = vec![];
    let containers = pod["spec"]["containers"].as_array().unwrap_or(&empty);
    containers
        .iter()
        .filter_map(|c| {
            c["resources"]["requests"]["memory"]
                .as_str()
                .and_then(parse_memory_bytes)
        })
        .sum()
}

/// Parse CPU string (e.g. "100m", "1") into millicores.
fn parse_cpu_millicores(s: &str) -> Option<i64> {
    if let Some(m) = s.strip_suffix('m') {
        m.parse().ok()
    } else {
        s.parse::<i64>().ok().map(|v| v * 1000)
    }
}

/// Parse memory string (e.g. "128Mi", "1Gi") into bytes.
fn parse_memory_bytes(s: &str) -> Option<i64> {
    let (num_str, suffix) = if let Some(pos) = s.chars().position(|c| c.is_alphabetic()) {
        (&s[..pos], &s[pos..])
    } else {
        (s, "")
    };

    let num: i64 = num_str.parse().ok()?;

    let multiplier = match suffix {
        "" => 1,
        "Ki" => 1024,
        "Mi" => 1024 * 1024,
        "Gi" => 1024 * 1024 * 1024,
        "Ti" => 1024 * 1024 * 1024 * 1024,
        "K" | "k" => 1000,
        "M" => 1000 * 1000,
        "G" => 1000 * 1000 * 1000,
        "T" => 1000 * 1000 * 1000 * 1000,
        _ => return None,
    };

    Some(num * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_priority_of() {
        let pod_with_priority = json!({
            "spec": {"priority": 100}
        });
        assert_eq!(priority_of(&pod_with_priority), 100);

        let pod_without_priority = json!({"spec": {}});
        assert_eq!(priority_of(&pod_without_priority), 0);
    }

    #[test]
    fn test_pods_on_node() {
        let pods = vec![
            json!({"metadata": {"name": "pod1"}, "spec": {"nodeName": "node1"}}),
            json!({"metadata": {"name": "pod2"}, "spec": {"nodeName": "node2"}}),
            json!({"metadata": {"name": "pod3"}, "spec": {"nodeName": "node1"}}),
        ];

        let node1_pods = pods_on_node(&pods, "node1");
        assert_eq!(node1_pods.len(), 2);

        let node2_pods = pods_on_node(&pods, "node2");
        assert_eq!(node2_pods.len(), 1);
    }

    #[test]
    fn test_parse_cpu_millicores() {
        assert_eq!(parse_cpu_millicores("100m"), Some(100));
        assert_eq!(parse_cpu_millicores("1"), Some(1000));
        assert_eq!(parse_cpu_millicores("2"), Some(2000));
        assert_eq!(parse_cpu_millicores("500m"), Some(500));
    }

    #[test]
    fn test_parse_memory_bytes() {
        assert_eq!(parse_memory_bytes("128Mi"), Some(128 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("1Gi"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("512Ki"), Some(512 * 1024));
        assert_eq!(parse_memory_bytes("1000"), Some(1000));
    }

    #[test]
    fn test_requested_cpu() {
        let pod = json!({
            "spec": {
                "containers": [
                    {"resources": {"requests": {"cpu": "100m"}}},
                    {"resources": {"requests": {"cpu": "200m"}}},
                ]
            }
        });
        assert_eq!(requested_cpu(&pod), 300);
    }

    #[test]
    fn test_requested_memory() {
        let pod = json!({
            "spec": {
                "containers": [
                    {"resources": {"requests": {"memory": "128Mi"}}},
                    {"resources": {"requests": {"memory": "256Mi"}}},
                ]
            }
        });
        assert_eq!(requested_memory(&pod), (128 + 256) * 1024 * 1024);
    }

    #[test]
    fn test_can_fit_after_eviction() {
        let pod = json!({
            "metadata": {"name": "new-pod"},
            "spec": {
                "containers": [
                    {"resources": {"requests": {"cpu": "500m", "memory": "512Mi"}}}
                ]
            }
        });

        let node = json!({
            "metadata": {"name": "node1"},
            "status": {
                "allocatable": {
                    "cpu": "2",
                    "memory": "2Gi"
                }
            }
        });

        // Low-usage pod
        let existing_pod = json!({
            "metadata": {"name": "existing"},
            "spec": {
                "nodeName": "node1",
                "containers": [
                    {"resources": {"requests": {"cpu": "100m", "memory": "128Mi"}}}
                ]
            }
        });

        let remaining = vec![&existing_pod];
        assert!(can_fit_after_eviction(&pod, &node, &remaining));

        // High-usage pod
        let heavy_pod = json!({
            "metadata": {"name": "heavy"},
            "spec": {
                "nodeName": "node1",
                "containers": [
                    {"resources": {"requests": {"cpu": "1800m", "memory": "1536Mi"}}}
                ]
            }
        });

        let remaining = vec![&heavy_pod];
        assert!(!can_fit_after_eviction(&pod, &node, &remaining));
    }

    #[test]
    fn test_find_preemption_candidates() {
        let high_priority_pod = json!({
            "metadata": {"name": "important"},
            "spec": {
                "priority": 100,
                "containers": [
                    {"resources": {"requests": {"cpu": "1", "memory": "1Gi"}}}
                ]
            }
        });

        let node = json!({
            "metadata": {"name": "node1"},
            "status": {
                "allocatable": {
                    "cpu": "2",
                    "memory": "2Gi"
                }
            }
        });

        let low_priority_pod = json!({
            "metadata": {"name": "victim1"},
            "spec": {
                "priority": 10,
                "nodeName": "node1",
                "containers": [
                    {"resources": {"requests": {"cpu": "1500m", "memory": "1536Mi"}}}
                ]
            }
        });

        let pods = vec![low_priority_pod];

        let candidate = find_preemption_candidates(&high_priority_pod, &[node], &pods);
        assert!(candidate.is_some());

        let candidate = candidate.unwrap();
        assert_eq!(candidate.node_name, "node1");
        assert_eq!(candidate.victims.len(), 1);
        assert_eq!(candidate.victims[0], "victim1");
    }

    #[test]
    fn test_no_preemption_when_equal_priority() {
        let pod = json!({
            "metadata": {"name": "new-pod"},
            "spec": {
                "priority": 50,
                "containers": [
                    {"resources": {"requests": {"cpu": "1", "memory": "1Gi"}}}
                ]
            }
        });

        let node = json!({
            "metadata": {"name": "node1"},
            "status": {
                "allocatable": {
                    "cpu": "2",
                    "memory": "2Gi"
                }
            }
        });

        let equal_priority_pod = json!({
            "metadata": {"name": "existing"},
            "spec": {
                "priority": 50,
                "nodeName": "node1",
                "containers": [
                    {"resources": {"requests": {"cpu": "1500m", "memory": "1536Mi"}}}
                ]
            }
        });

        let pods = vec![equal_priority_pod];

        let candidate = find_preemption_candidates(&pod, &[node], &pods);
        assert!(candidate.is_none());
    }

    #[test]
    fn test_preemption_prefers_fewer_victims() {
        let pod = json!({
            "metadata": {"name": "new-pod"},
            "spec": {
                "priority": 100,
                "containers": [
                    {"resources": {"requests": {"cpu": "500m", "memory": "512Mi"}}}
                ]
            }
        });

        let node1 = json!({
            "metadata": {"name": "node1"},
            "status": {
                "allocatable": {
                    "cpu": "2",
                    "memory": "2Gi"
                }
            }
        });

        let node2 = json!({
            "metadata": {"name": "node2"},
            "status": {
                "allocatable": {
                    "cpu": "2",
                    "memory": "2Gi"
                }
            }
        });

        // Node 1: needs to evict 2 pods (each individually insufficient)
        // Total capacity: 2000m CPU, 2048Mi memory
        // Pod needs: 500m CPU, 512Mi memory
        // Setup: victim1 (300m, 200Mi) + victim2 (1300m, 1400Mi) = 1600m, 1600Mi used
        // After evicting victim1: 1300m used, 700m free, 1400Mi used, 648Mi free
        // - 700m >= 500m (OK), but 648Mi >= 512Mi (OK too!)
        // We need victim2 to use more memory so evicting victim1 alone isn't enough
        let victim1 = json!({
            "metadata": {"name": "victim1"},
            "spec": {
                "priority": 10,
                "nodeName": "node1",
                "containers": [
                    {"resources": {"requests": {"cpu": "300m", "memory": "200Mi"}}}
                ]
            }
        });

        let victim2 = json!({
            "metadata": {"name": "victim2"},
            "spec": {
                "priority": 10,
                "nodeName": "node1",
                "containers": [
                    {"resources": {"requests": {"cpu": "1300m", "memory": "1600Mi"}}}
                ]
            }
        });

        // Node 2: needs to evict only 1 pod
        let victim3 = json!({
            "metadata": {"name": "victim3"},
            "spec": {
                "priority": 10,
                "nodeName": "node2",
                "containers": [
                    {"resources": {"requests": {"cpu": "1600m", "memory": "1536Mi"}}}
                ]
            }
        });

        let pods = vec![victim1, victim2, victim3];

        let candidate = find_preemption_candidates(&pod, &[node1, node2], &pods);
        assert!(candidate.is_some());

        let candidate = candidate.unwrap();
        // Should prefer node2 with only 1 victim
        assert_eq!(candidate.node_name, "node2");
        assert_eq!(candidate.victims.len(), 1);
    }
}
