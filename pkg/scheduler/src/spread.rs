//! Pod topology spread constraints.
//!
//! `topologySpreadConstraints` is the modern answer to "spread these evenly",
//! and it says something anti-affinity cannot: anti-affinity is a hard *no two
//! here*, while spread expresses *keep the counts within maxSkew of each
//! other*. That difference matters as soon as there are more replicas than
//! domains, where anti-affinity leaves every extra replica unschedulable and
//! spread simply balances them.
//!
//! Terms, in the upstream vocabulary:
//!
//! - `topologyKey` — a node label whose value names the domain (host, rack,
//!   zone).
//! - `maxSkew` — the most the busiest matching domain may exceed the least.
//! - `whenUnsatisfiable` — `DoNotSchedule` (a filter) or `ScheduleAnyway` (a
//!   score).
//! - `minDomains` — treat the world as having at least this many domains, so a
//!   set does not collapse into the one domain that happens to exist yet.
//! - `labelSelector` — which pods count.

use crate::affinity::{domain_of, selector_matches};
use crate::scheduler::ClusterState;
use serde_json::Value;
use std::collections::HashMap;

/// Count matching pods per domain, over the domains the cluster actually has.
///
/// Domains with no matching pods are counted as zero rather than omitted —
/// which is the whole point, since an empty domain is exactly where the next
/// pod should go.
fn counts_by_domain(
    constraint: &Value,
    pod_ns: &str,
    nodes: &[Value],
    state: &ClusterState,
) -> HashMap<String, i64> {
    let key = constraint["topologyKey"].as_str().unwrap_or("kubernetes.io/hostname");
    let mut counts: HashMap<String, i64> = HashMap::new();
    for n in nodes {
        if let Some(d) = domain_of(n, key) {
            counts.entry(d.to_string()).or_insert(0);
        }
    }
    let by_name: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| n["metadata"]["name"].as_str().map(|s| (s, n)))
        .collect();
    for (node_name, other) in &state.placed {
        // Spread is within a namespace, as upstream defines it.
        if other["metadata"]["namespace"].as_str().unwrap_or("default") != pod_ns {
            continue;
        }
        if !selector_matches(&constraint["labelSelector"], &other["metadata"]["labels"]) {
            continue;
        }
        if let Some(n) = by_name.get(node_name.as_str()) {
            if let Some(d) = domain_of(n, key) {
                *counts.entry(d.to_string()).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// The skew that results from placing this pod in `here`.
fn skew_if_placed(counts: &HashMap<String, i64>, here: &str, min_domains: i64) -> i64 {
    let mut vals: Vec<i64> = counts
        .iter()
        .map(|(d, c)| if d == here { c + 1 } else { *c })
        .collect();
    // minDomains: pretend the missing domains exist and are empty, so a set
    // does not pile into the one domain that is up yet and call it balanced.
    while (vals.len() as i64) < min_domains {
        vals.push(0);
    }
    let max = vals.iter().copied().max().unwrap_or(0);
    let min = vals.iter().copied().min().unwrap_or(0);
    max - min
}

/// `DoNotSchedule` constraints, as a filter.
pub fn spread_filter(
    pod: &Value,
    node: &Value,
    nodes: &[Value],
    state: &ClusterState,
) -> Result<(), String> {
    let pod_ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    for c in pod["spec"]["topologySpreadConstraints"]
        .as_array().map(|v| v.as_slice()).unwrap_or(&[])
    {
        if c["whenUnsatisfiable"].as_str().unwrap_or("DoNotSchedule") != "DoNotSchedule" {
            continue;
        }
        let key = c["topologyKey"].as_str().unwrap_or("kubernetes.io/hostname");
        let Some(here) = domain_of(node, key) else {
            return Err(format!("node has no {key} label, required by topology spread"));
        };
        let max_skew = c["maxSkew"].as_i64().unwrap_or(1).max(1);
        let min_domains = c["minDomains"].as_i64().unwrap_or(0);
        let counts = counts_by_domain(c, pod_ns, nodes, state);
        let skew = skew_if_placed(&counts, here, min_domains);
        if skew > max_skew {
            return Err(format!(
                "topology spread: placing here makes skew {skew} over {key} (maxSkew {max_skew})"
            ));
        }
    }
    Ok(())
}

/// `ScheduleAnyway` constraints, as a score in 0..=100 — lower skew is better.
pub fn spread_score(pod: &Value, node: &Value, nodes: &[Value], state: &ClusterState) -> i64 {
    let pod_ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    let mut worst: i64 = 0;
    let mut any = false;
    for c in pod["spec"]["topologySpreadConstraints"]
        .as_array().map(|v| v.as_slice()).unwrap_or(&[])
    {
        if c["whenUnsatisfiable"].as_str().unwrap_or("DoNotSchedule") != "ScheduleAnyway" {
            continue;
        }
        let key = c["topologyKey"].as_str().unwrap_or("kubernetes.io/hostname");
        let Some(here) = domain_of(node, key) else { continue };
        any = true;
        let counts = counts_by_domain(c, pod_ns, nodes, state);
        worst = worst.max(skew_if_placed(&counts, here, c["minDomains"].as_i64().unwrap_or(0)));
    }
    if !any {
        return 0;
    }
    // A skew of 0 is perfect; every extra point of skew costs 20, floored at 0.
    (100 - worst * 20).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, zone: &str) -> Value {
        json!({"metadata":{"name":name,"labels":{
            "kubernetes.io/hostname": name, "topology.kubernetes.io/zone": zone}}})
    }

    fn placed(node: &str, app: &str) -> (String, Value) {
        (node.to_string(),
         json!({"metadata":{"name":format!("{app}-x"),"namespace":"default",
                            "labels":{"app":app}}}))
    }

    fn spreading_pod(max_skew: i64, key: &str, mode: &str) -> Value {
        json!({"metadata":{"namespace":"default","labels":{"app":"web"}},
               "spec":{"topologySpreadConstraints":[{
                   "maxSkew": max_skew, "topologyKey": key,
                   "whenUnsatisfiable": mode,
                   "labelSelector":{"matchLabels":{"app":"web"}}}]}})
    }

    #[test]
    fn a_full_domain_is_refused_when_an_empty_one_exists() {
        // Two nodes, one already holding a web pod. maxSkew 1 means the second
        // replica must go to the empty node — placing it on n1 would make the
        // counts 2 and 0.
        let nodes = vec![node("n1", "a"), node("n2", "b")];
        let mut state = ClusterState::default();
        state.placed.push(placed("n1", "web"));
        let pod = spreading_pod(1, "kubernetes.io/hostname", "DoNotSchedule");

        assert!(spread_filter(&pod, &nodes[0], &nodes, &state).is_err(), "n1 should be refused");
        assert!(spread_filter(&pod, &nodes[1], &nodes, &state).is_ok(), "n2 should be allowed");
    }

    #[test]
    fn an_unmatched_pod_does_not_count_toward_the_skew() {
        // The same shape, but the placed pod belongs to another app. It must
        // not constrain where `web` goes.
        let nodes = vec![node("n1", "a"), node("n2", "b")];
        let mut state = ClusterState::default();
        state.placed.push(placed("n1", "batch"));
        let pod = spreading_pod(1, "kubernetes.io/hostname", "DoNotSchedule");
        assert!(spread_filter(&pod, &nodes[0], &nodes, &state).is_ok());
    }

    #[test]
    fn schedule_anyway_scores_rather_than_refuses() {
        // ScheduleAnyway must never fail a node — it expresses a preference,
        // and a constraint that cannot be met still has to place the pod.
        let nodes = vec![node("n1", "a"), node("n2", "b")];
        let mut state = ClusterState::default();
        state.placed.push(placed("n1", "web"));
        let pod = spreading_pod(1, "kubernetes.io/hostname", "ScheduleAnyway");

        assert!(spread_filter(&pod, &nodes[0], &nodes, &state).is_ok());
        let full = spread_score(&pod, &nodes[0], &nodes, &state);
        let empty = spread_score(&pod, &nodes[1], &nodes, &state);
        assert!(empty > full, "the empty domain must score higher ({empty} vs {full})");
    }

    #[test]
    fn zones_are_domains_too() {
        // Four nodes in two zones, both zone-a nodes taken. Spreading over
        // zone must send the pod to zone b even though two hosts are free.
        let nodes = vec![node("n1","a"), node("n2","a"), node("n3","b"), node("n4","b")];
        let mut state = ClusterState::default();
        state.placed.push(placed("n1", "web"));
        state.placed.push(placed("n2", "web"));
        let pod = spreading_pod(1, "topology.kubernetes.io/zone", "DoNotSchedule");

        assert!(spread_filter(&pod, &nodes[0], &nodes, &state).is_err());
        assert!(spread_filter(&pod, &nodes[2], &nodes, &state).is_ok());
    }

    #[test]
    fn min_domains_counts_the_domains_that_are_not_there_yet() {
        // One node up, minDomains 3: the cluster is expected to grow, so a
        // second replica here would already be a skew of 2 against the empty
        // domains that are coming.
        let nodes = vec![node("n1", "a")];
        let mut state = ClusterState::default();
        state.placed.push(placed("n1", "web"));
        let mut pod = spreading_pod(1, "kubernetes.io/hostname", "DoNotSchedule");
        pod["spec"]["topologySpreadConstraints"][0]["minDomains"] = json!(3);
        assert!(spread_filter(&pod, &nodes[0], &nodes, &state).is_err());
    }
}
