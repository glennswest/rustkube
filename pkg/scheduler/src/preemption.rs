//! Preemption (#84): when a Pod fits on no node, which lower-priority Pods
//! must go from which node to make room — upstream's DefaultPreemption,
//! decided here, carried out by the scheduling loop (`scheduler.rs`), which
//! nominates the Pod (`status.nominatedNodeName`) and evicts the victims
//! through the Eviction API.
//!
//! - A Pod with `preemptionPolicy: Never` preempts nothing.
//! - A node is a candidate when the Pod passes every filter there once all
//!   its lower-priority Pods are gone (Pods already terminating are not
//!   counted as victims, and a failure removing Pods cannot fix — taints,
//!   selectors, affinity — rules the node out).
//! - Victims are then reprieved, as many as still leave room: those a
//!   PodDisruptionBudget protects first, then the rest, each group highest
//!   priority first — so the fewest and least important Pods go, and budgets
//!   are broken only when nothing else makes room.
//! - The node chosen has, in order: the fewest budget violations, the lowest
//!   highest-victim priority, the lowest sum of victim priorities, the
//!   fewest victims.

use crate::filter::{self, FilterResult, NodeUsage};
use crate::scheduler::{pod_priority, ClusterState};
use serde_json::Value;

/// One node's preemption: the Pods that must go.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub node: String,
    pub victims: Vec<Value>,
    /// Victims a PodDisruptionBudget would not allow to go.
    pub pdb_violations: usize,
}

fn name(o: &Value) -> &str {
    o["metadata"]["name"].as_str().unwrap_or("")
}

fn uid(o: &Value) -> &str {
    o["metadata"]["uid"].as_str().unwrap_or("")
}

/// May this Pod preempt at all?
pub fn may_preempt(pod: &Value) -> bool {
    pod["spec"]["preemptionPolicy"].as_str() != Some("Never")
}

fn sub(u: &mut NodeUsage, pod: &Value) {
    let (cpu, mem) = filter::pod_requests(pod);
    u.cpu_milli = u.cpu_milli.saturating_sub(cpu);
    u.mem_bytes = u.mem_bytes.saturating_sub(mem);
    u.pods = u.pods.saturating_sub(1);
}

fn add(u: &mut NodeUsage, pod: &Value) {
    let (cpu, mem) = filter::pod_requests(pod);
    u.cpu_milli += cpu;
    u.mem_bytes += mem;
    u.pods += 1;
}

fn fits(pod: &Value, node: &Value, state: &ClusterState, nodes: &[Value]) -> bool {
    let used = state.used(node);
    matches!(filter::run_filters(pod, node, used, state, nodes), FilterResult::Pass)
        && matches!(filter::pod_count_filter(node, used), FilterResult::Pass)
}

/// `state` with these Pods gone from `node`.
fn without(state: &ClusterState, node: &str, gone: &[&Value]) -> ClusterState {
    let mut s = state.clone();
    let uids: Vec<&str> = gone.iter().map(|p| uid(p)).collect();
    s.placed.retain(|(n, p)| !(n == node && uids.contains(&uid(p))));
    let u = s.usage.entry(node.to_string()).or_default();
    for p in gone {
        sub(u, p);
    }
    s
}

/// Does a PodDisruptionBudget cover `pod` with no disruption left?
/// `taken` counts victims already charged to each budget.
fn violates(pod: &Value, pdbs: &[Value], taken: &mut std::collections::HashMap<String, i64>) -> bool {
    let ns = pod["metadata"]["namespace"].as_str().unwrap_or("");
    let labels = &pod["metadata"]["labels"];
    let mut violation = false;
    for pdb in pdbs.iter().filter(|p| p["metadata"]["namespace"].as_str() == Some(ns)) {
        let sel = &pdb["spec"]["selector"];
        if !sel.is_object() || !apimachinery::selector::matches(sel, labels) {
            continue;
        }
        let key = format!("{ns}/{}", name(pdb));
        let used = taken.entry(key).or_insert(0);
        let allowed = pdb["status"]["disruptionsAllowed"].as_i64().unwrap_or(0);
        if allowed - *used <= 0 {
            violation = true;
        }
        *used += 1;
    }
    violation
}

/// The victims on one node, or None when removing every lower-priority Pod
/// still does not make room.
pub fn on_node(pod: &Value, node: &Value, state: &ClusterState, nodes: &[Value], pdbs: &[Value]) -> Option<Candidate> {
    let node_name = node["metadata"]["name"].as_str()?;
    let prio = pod_priority(pod);
    let mut lower: Vec<&Value> = state
        .placed
        .iter()
        .filter(|(n, p)| {
            n == node_name && pod_priority(p) < prio && p["metadata"]["deletionTimestamp"].is_null()
                && uid(p) != uid(pod)
        })
        .map(|(_, p)| p)
        .collect();
    if lower.is_empty() {
        return None;
    }
    let mut trial = without(state, node_name, &lower);
    if !fits(pod, node, &trial, nodes) {
        return None;
    }
    // Reprieve: budget-protected first, then the rest; each highest priority
    // first. A Pod stays if the preemptor still fits with it back.
    lower.sort_by_key(|p| std::cmp::Reverse(pod_priority(p)));
    let mut taken = std::collections::HashMap::new();
    let (protected, free): (Vec<&Value>, Vec<&Value>) = lower.iter().partition(|p| violates(p, pdbs, &mut taken.clone()));
    let mut victims: Vec<Value> = Vec::new();
    for p in protected.into_iter().chain(free) {
        trial.placed.push((node_name.to_string(), p.clone()));
        add(trial.usage.entry(node_name.to_string()).or_default(), p);
        if !fits(pod, node, &trial, nodes) {
            trial.placed.pop();
            sub(trial.usage.entry(node_name.to_string()).or_default(), p);
            victims.push(p.clone());
        }
    }
    let pdb_violations = victims.iter().filter(|v| violates(v, pdbs, &mut taken)).count();
    Some(Candidate { node: node_name.to_string(), victims, pdb_violations })
}

/// The best node to preempt on, if any.
pub fn select(pod: &Value, nodes: &[&Value], all_nodes: &[Value], state: &ClusterState, pdbs: &[Value]) -> Option<Candidate> {
    if !may_preempt(pod) {
        return None;
    }
    let mut candidates: Vec<Candidate> = nodes.iter().filter_map(|n| on_node(pod, n, state, all_nodes, pdbs)).collect();
    candidates.sort_by_key(|c| {
        let highest = c.victims.iter().map(pod_priority).max().unwrap_or(i64::MIN);
        let sum: i64 = c.victims.iter().map(pod_priority).sum();
        (c.pdb_violations, highest, sum, c.victims.len())
    });
    candidates.into_iter().next()
}

/// A Pod already nominated whose lower-priority Pods on that node are still
/// terminating: upstream waits for them rather than preempting again.
pub fn waiting_for_victims(pod: &Value, state: &ClusterState) -> bool {
    let Some(node) = pod["status"]["nominatedNodeName"].as_str().filter(|n| !n.is_empty()) else {
        return false;
    };
    let prio = pod_priority(pod);
    state.placed.iter().any(|(n, p)| n == node && pod_priority(p) < prio && !p["metadata"]["deletionTimestamp"].is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(n: &str, cpu: &str) -> Value {
        json!({"metadata": {"name": n, "labels": {"kubernetes.io/hostname": n}},
               "status": {"allocatable": {"cpu": cpu, "memory": "8Gi", "pods": "110"},
                          "conditions": [{"type": "Ready", "status": "True"}]}})
    }
    fn pod(n: &str, prio: i64, cpu: &str) -> Value {
        json!({"metadata": {"name": n, "namespace": "d", "uid": n, "labels": {"app": n}},
               "spec": {"priority": prio, "containers": [{"name": "c", "resources": {"requests": {"cpu": cpu}}}]}})
    }
    fn state(placed: Vec<(&str, Value)>) -> ClusterState {
        let mut s = ClusterState::default();
        for (n, p) in placed {
            add(s.usage.entry(n.into()).or_default(), &p);
            s.placed.push((n.into(), p));
        }
        s
    }

    #[test]
    fn the_fewest_lowest_victims_on_the_best_node() {
        let nodes = vec![node("a", "2"), node("b", "2")];
        // a: two 1-CPU pods at 10 and 20; b: one 2-CPU pod at 5.
        let s = state(vec![("a", pod("a1", 10, "1")), ("a", pod("a2", 20, "1")), ("b", pod("b1", 5, "2"))]);
        let high = pod("high", 100, "1");
        let refs: Vec<&Value> = nodes.iter().collect();
        let c = select(&high, &refs, &nodes, &s, &[]).unwrap();
        // a needs one victim (a1, priority 10); b needs b1 (priority 5): lower highest-victim wins.
        assert_eq!(c.node, "b");
        assert_eq!(c.victims.iter().map(|v| name(v)).collect::<Vec<_>>(), vec!["b1"]);
        // On a alone, only the lower of the two goes.
        let on_a = on_node(&high, &nodes[0], &s, &nodes, &[]).unwrap();
        assert_eq!(on_a.victims.iter().map(|v| name(v)).collect::<Vec<_>>(), vec!["a1"]);
    }

    #[test]
    fn nothing_to_preempt_when_it_would_not_help_or_is_not_allowed() {
        let nodes = vec![node("a", "2")];
        let s = state(vec![("a", pod("a1", 100, "2"))]);
        let refs: Vec<&Value> = nodes.iter().collect();
        assert!(select(&pod("p", 100, "1"), &refs, &nodes, &s, &[]).is_none(), "equal priority is not lower");
        assert!(select(&pod("p", 200, "4"), &refs, &nodes, &s, &[]).is_none(), "4 CPUs never fit a 2-CPU node");
        let mut never = pod("p", 200, "1");
        never["spec"]["preemptionPolicy"] = json!("Never");
        assert!(select(&never, &refs, &nodes, &s, &[]).is_none());
        let mut going = pod("a1", 1, "2");
        going["metadata"]["deletionTimestamp"] = json!("2026-10-07T00:00:00Z");
        let s = state(vec![("a", going)]);
        assert!(select(&pod("p", 200, "1"), &refs, &nodes, &s, &[]).is_none(), "a terminating Pod is no victim");
        let mut nominated = pod("p", 200, "1");
        nominated["status"] = json!({"nominatedNodeName": "a"});
        assert!(waiting_for_victims(&nominated, &s));
    }

    #[test]
    fn a_budget_protected_pod_is_spared_when_another_will_do() {
        let nodes = vec![node("a", "2")];
        let s = state(vec![("a", pod("guarded", 10, "1")), ("a", pod("plain", 20, "1"))]);
        let pdb = json!({"metadata": {"name": "pdb", "namespace": "d"}, "spec": {"selector": {"matchLabels": {"app": "guarded"}}},
                         "status": {"disruptionsAllowed": 0}});
        let c = on_node(&pod("high", 100, "1"), &nodes[0], &s, &nodes, &[pdb]).unwrap();
        assert_eq!(c.victims.iter().map(|v| name(v)).collect::<Vec<_>>(), vec!["plain"], "the higher-priority but unprotected one goes");
        assert_eq!(c.pdb_violations, 0);
    }
}
