//! Inter-pod affinity, anti-affinity, and topology spread.
//!
//! These are the plugins that make placement depend on *other pods* rather than
//! on the node alone: keep these together, keep those apart, and spread this
//! set evenly across failure domains. A scheduler without them will happily put
//! every replica of a service on one node — correct by every other measure and
//! useless the moment that node reboots.
//!
//! All three work the same way: a label selector picks a set of pods, a
//! `topologyKey` names a node label whose value defines a *domain* (a node, a
//! rack, a zone), and the constraint is evaluated over the domains that the
//! selected pods currently occupy.

use crate::scheduler::ClusterState;
use serde_json::Value;
use std::collections::HashMap;

/// Does a pod's labels satisfy a `LabelSelector` (matchLabels + matchExpressions)?
///
/// Kubernetes semantics: an **empty** selector matches everything, and every
/// term must hold (they are ANDed).
pub fn selector_matches(selector: &Value, labels: &Value) -> bool {
    if let Some(m) = selector["matchLabels"].as_object() {
        for (k, v) in m {
            if labels[k] != *v {
                return false;
            }
        }
    }
    for expr in selector["matchExpressions"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let key = expr["key"].as_str().unwrap_or("");
        let op = expr["operator"].as_str().unwrap_or("In");
        let vals: Vec<&str> =
            expr["values"].as_array().map(|v| v.as_slice()).unwrap_or(&[])
                .iter().filter_map(|x| x.as_str()).collect();
        let have = labels[key].as_str();
        let ok = match op {
            "In" => have.map(|h| vals.contains(&h)).unwrap_or(false),
            "NotIn" => have.map(|h| !vals.contains(&h)).unwrap_or(true),
            "Exists" => !labels[key].is_null(),
            "DoesNotExist" => labels[key].is_null(),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// The domain a node belongs to for a given `topologyKey`.
///
/// `kubernetes.io/hostname` makes each node its own domain, which is the
/// common case for anti-affinity. A missing label means the node is in *no*
/// domain for this key, and upstream treats that as not matching rather than
/// as an empty-string domain — otherwise every unlabelled node would collapse
/// into one giant domain and constraints would be satisfied by accident.
pub fn domain_of<'a>(node: &'a Value, topology_key: &str) -> Option<&'a str> {
    node["metadata"]["labels"][topology_key].as_str()
}

/// Whether a term's selector matches a placed pod, honouring `namespaces` and
/// `namespaceSelector`.
///
/// Default when neither is given: the pod's *own* namespace, which is what
/// makes anti-affinity between replicas of one deployment work without saying
/// so.
fn term_matches_pod(term: &Value, pod_ns: &str, other: &Value) -> bool {
    let other_ns = other["metadata"]["namespace"].as_str().unwrap_or("default");
    let listed = term["namespaces"].as_array().map(|v| v.as_slice()).unwrap_or(&[]);
    let ns_ok = if !listed.is_empty() {
        listed.iter().any(|n| n.as_str() == Some(other_ns))
    } else if term["namespaceSelector"].is_object() {
        // An empty namespaceSelector means *all* namespaces.
        true
    } else {
        other_ns == pod_ns
    };
    ns_ok && selector_matches(&term["labelSelector"], &other["metadata"]["labels"])
}

/// Domains (by topologyKey value) that already hold a pod matching `term`.
fn occupied_domains(
    term: &Value,
    pod_ns: &str,
    nodes_by_name: &HashMap<&str, &Value>,
    state: &ClusterState,
) -> Vec<String> {
    let key = term["topologyKey"].as_str().unwrap_or("kubernetes.io/hostname");
    let mut out = Vec::new();
    for (node_name, other) in &state.placed {
        if !term_matches_pod(term, pod_ns, other) {
            continue;
        }
        if let Some(n) = nodes_by_name.get(node_name.as_str()) {
            if let Some(d) = domain_of(n, key) {
                out.push(d.to_string());
            }
        }
    }
    out
}

/// Required inter-pod affinity and anti-affinity.
///
/// Returns `Err(reason)` when the node is not feasible.
pub fn pod_affinity_filter(
    pod: &Value,
    node: &Value,
    nodes: &[Value],
    state: &ClusterState,
) -> Result<(), String> {
    let pod_ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    let by_name: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| n["metadata"]["name"].as_str().map(|s| (s, n)))
        .collect();

    let affinity = &pod["spec"]["affinity"];

    // Affinity: this node's domain must already hold a matching pod.
    for term in affinity["podAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
        .as_array().map(|v| v.as_slice()).unwrap_or(&[])
    {
        let key = term["topologyKey"].as_str().unwrap_or("kubernetes.io/hostname");
        let Some(here) = domain_of(node, key) else {
            return Err(format!("node has no {key} label, required by podAffinity"));
        };
        let occupied = occupied_domains(term, pod_ns, &by_name, state);
        if !occupied.iter().any(|d| d == here) {
            return Err(format!(
                "podAffinity: no matching pod in {key}={here}"
            ));
        }
    }

    // Anti-affinity: this node's domain must hold *no* matching pod.
    for term in affinity["podAntiAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
        .as_array().map(|v| v.as_slice()).unwrap_or(&[])
    {
        let key = term["topologyKey"].as_str().unwrap_or("kubernetes.io/hostname");
        // No label for the key means the node is in no domain, so it cannot
        // violate the constraint — and cannot satisfy an affinity either.
        let Some(here) = domain_of(node, key) else {
            continue;
        };
        let occupied = occupied_domains(term, pod_ns, &by_name, state);
        if occupied.iter().any(|d| d == here) {
            return Err(format!(
                "podAntiAffinity: a matching pod is already in {key}={here}"
            ));
        }
    }
    Ok(())
}

/// Preferred inter-pod affinity and anti-affinity, as a score in 0..=100.
///
/// Each satisfied term contributes its `weight`; the total is normalised
/// against the sum of weights so that a pod with one term and a pod with ten
/// are scored on the same scale.
pub fn pod_affinity_score(
    pod: &Value,
    node: &Value,
    nodes: &[Value],
    state: &ClusterState,
) -> i64 {
    let pod_ns = pod["metadata"]["namespace"].as_str().unwrap_or("default");
    let by_name: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| n["metadata"]["name"].as_str().map(|s| (s, n)))
        .collect();
    let affinity = &pod["spec"]["affinity"];
    let mut got: i64 = 0;
    let mut total: i64 = 0;

    for (path, want_present) in [
        ("podAffinity", true),
        ("podAntiAffinity", false),
    ] {
        for w in affinity[path]["preferredDuringSchedulingIgnoredDuringExecution"]
            .as_array().map(|v| v.as_slice()).unwrap_or(&[])
        {
            let weight = w["weight"].as_i64().unwrap_or(1).clamp(1, 100);
            total += weight;
            let term = &w["podAffinityTerm"];
            let key = term["topologyKey"].as_str().unwrap_or("kubernetes.io/hostname");
            let Some(here) = domain_of(node, key) else { continue };
            let occupied = occupied_domains(term, pod_ns, &by_name, state);
            let present = occupied.iter().any(|d| d == here);
            if present == want_present {
                got += weight;
            }
        }
    }
    if total == 0 { 0 } else { got * 100 / total }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, zone: &str) -> Value {
        json!({"metadata":{"name":name,"labels":{
            "kubernetes.io/hostname": name, "topology.kubernetes.io/zone": zone}}})
    }

    fn anti(key: &str) -> Value {
        json!({"metadata":{"namespace":"default","labels":{"app":"web"}},
               "spec":{"affinity":{"podAntiAffinity":{
                   "requiredDuringSchedulingIgnoredDuringExecution":[{
                       "topologyKey": key,
                       "labelSelector":{"matchLabels":{"app":"web"}}}]}}}})
    }

    fn placed(node: &str, ns: &str, app: &str) -> (String, Value) {
        (node.to_string(),
         json!({"metadata":{"name":format!("{app}-1"),"namespace":ns,"labels":{"app":app}}}))
    }

    #[test]
    fn anti_affinity_keeps_replicas_off_an_occupied_node() {
        let nodes = vec![node("n1","a"), node("n2","b")];
        let mut st = ClusterState::default();
        st.placed.push(placed("n1","default","web"));
        let pod = anti("kubernetes.io/hostname");
        assert!(pod_affinity_filter(&pod, &nodes[0], &nodes, &st).is_err());
        assert!(pod_affinity_filter(&pod, &nodes[1], &nodes, &st).is_ok());
    }

    #[test]
    fn anti_affinity_defaults_to_the_pods_own_namespace() {
        // A matching pod in *another* namespace must not block placement, or
        // one tenant's deployment would constrain another's.
        let nodes = vec![node("n1","a")];
        let mut st = ClusterState::default();
        st.placed.push(placed("n1","other","web"));
        let pod = anti("kubernetes.io/hostname");
        assert!(pod_affinity_filter(&pod, &nodes[0], &nodes, &st).is_ok());
    }

    #[test]
    fn anti_affinity_over_zone_blocks_a_whole_zone() {
        let nodes = vec![node("n1","a"), node("n2","a"), node("n3","b")];
        let mut st = ClusterState::default();
        st.placed.push(placed("n1","default","web"));
        let pod = anti("topology.kubernetes.io/zone");
        // n2 is a different host but the same zone.
        assert!(pod_affinity_filter(&pod, &nodes[1], &nodes, &st).is_err());
        assert!(pod_affinity_filter(&pod, &nodes[2], &nodes, &st).is_ok());
    }

    #[test]
    fn affinity_requires_a_matching_pod_to_be_present() {
        let nodes = vec![node("n1","a"), node("n2","b")];
        let mut st = ClusterState::default();
        st.placed.push(placed("n1","default","cache"));
        let pod = json!({"metadata":{"namespace":"default"},
            "spec":{"affinity":{"podAffinity":{
                "requiredDuringSchedulingIgnoredDuringExecution":[{
                    "topologyKey":"kubernetes.io/hostname",
                    "labelSelector":{"matchLabels":{"app":"cache"}}}]}}}});
        assert!(pod_affinity_filter(&pod, &nodes[0], &nodes, &st).is_ok(), "cache is here");
        assert!(pod_affinity_filter(&pod, &nodes[1], &nodes, &st).is_err(), "cache is not here");
    }

    #[test]
    fn selector_operators_behave_as_kubernetes_defines_them() {
        let labels = json!({"app":"web","tier":"front"});
        assert!(selector_matches(&json!({}), &labels), "an empty selector matches everything");
        assert!(selector_matches(&json!({"matchLabels":{"app":"web"}}), &labels));
        assert!(!selector_matches(&json!({"matchLabels":{"app":"db"}}), &labels));
        assert!(selector_matches(
            &json!({"matchExpressions":[{"key":"tier","operator":"In","values":["front","back"]}]}),
            &labels));
        assert!(selector_matches(
            &json!({"matchExpressions":[{"key":"zone","operator":"DoesNotExist"}]}), &labels));
        assert!(!selector_matches(
            &json!({"matchExpressions":[{"key":"app","operator":"NotIn","values":["web"]}]}),
            &labels));
    }
}
