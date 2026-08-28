//! Taints and tolerations: the matching rules, and what a `NoExecute` taint
//! does to pods already running.
//!
//! Pure functions, because these are rules rather than I/O and every one of
//! them is a sentence from the Kubernetes docs that is easy to implement
//! *almost* right.
//!
//! Three things the cluster needs and did not have:
//!
//! - **A taint has to come off again.** The node controller added
//!   `node.kubernetes.io/not-ready:NoSchedule` when a node stopped posting
//!   status and never removed it. A node that recovered was Ready, healthy,
//!   and permanently unschedulable — and nothing said why, because the taint
//!   is in `spec` and everyone was reading `status`.
//! - **`NoExecute` has to evict.** It is the difference between "do not put
//!   anything new here" and "get off". Without it a `NoExecute` taint is a
//!   `NoSchedule` taint with a different spelling.
//! - **`tolerationSeconds` has to be honoured.** A pod that tolerates
//!   `not-ready:NoExecute` for 300s is saying "give the node five minutes to
//!   come back before you move me", which is the entire reason the field
//!   exists. Evicting immediately makes a brief blip an outage.

use serde_json::Value;

/// Does this toleration tolerate this taint, and for how long?
///
/// `Some(None)` — tolerated forever. `Some(Some(n))` — tolerated for `n`
/// seconds after the taint was added. `None` — not tolerated.
///
/// The matching rules, which are small and easy to get subtly wrong:
///
/// - an empty `effect` matches every effect
/// - `operator: Exists` with an empty `key` matches **every** taint, which is
///   how a DaemonSet says "run me anywhere"
/// - `operator: Exists` with a key matches that key at any value
/// - `operator: Equal` (the default) matches key *and* value
pub fn toleration_matches(toleration: &Value, taint: &Value) -> Option<Option<i64>> {
    let t_key = toleration["key"].as_str().unwrap_or("");
    let t_effect = toleration["effect"].as_str().unwrap_or("");
    let t_value = toleration["value"].as_str().unwrap_or("");
    let operator = toleration["operator"].as_str().unwrap_or("Equal");

    let taint_key = taint["key"].as_str().unwrap_or("");
    let taint_effect = taint["effect"].as_str().unwrap_or("");
    let taint_value = taint["value"].as_str().unwrap_or("");

    // An empty effect tolerates every effect.
    if !t_effect.is_empty() && t_effect != taint_effect {
        return None;
    }

    let matches = match operator {
        "Exists" => t_key.is_empty() || t_key == taint_key,
        // An empty operator means Equal — that is the API default, and a
        // toleration written without one is the common case.
        "" | "Equal" => t_key == taint_key && t_value == taint_value,
        // Anything else does not tolerate, which is what upstream does.
        // Failing closed means a workload does not run; failing open means it
        // runs somewhere it was told not to, and the second is worse. The API
        // rejects unknown operators at admission, so reaching here at all
        // means something already went wrong.
        _ => false,
    };
    if !matches {
        return None;
    }
    Some(toleration["tolerationSeconds"].as_i64())
}

/// Does this pod tolerate this taint, and for how long?
///
/// The **longest** toleration wins when several match: they are permissions,
/// and the most permissive is what the author asked for. Taking the first
/// match would make the order of a list significant, which it is not.
pub fn pod_tolerates(pod: &Value, taint: &Value) -> Option<Option<i64>> {
    let tolerations = pod["spec"]["tolerations"].as_array()?;
    let mut best: Option<Option<i64>> = None;
    for t in tolerations {
        match toleration_matches(t, taint) {
            None => continue,
            // Forever beats any finite window, and short-circuits.
            Some(None) => return Some(None),
            Some(Some(secs)) => {
                best = match best {
                    Some(Some(prev)) if prev >= secs => Some(Some(prev)),
                    _ => Some(Some(secs)),
                }
            }
        }
    }
    best
}

/// What a `NoExecute` taint means for one pod.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Nothing to do: no `NoExecute` taint applies, or the pod tolerates them
    /// all indefinitely.
    Stay,
    /// Evict now.
    Evict(String),
    /// Evict once this many seconds have passed since the taint was added.
    EvictIn(i64, String),
}

/// Decide what happens to a pod on a node carrying these taints.
///
/// Only `NoExecute` is considered: `NoSchedule` and `PreferNoSchedule` govern
/// placement, and a pod already running has been placed. Evicting for a
/// `NoSchedule` taint would tear down a healthy workload for a rule that was
/// never about it.
///
/// `age_secs` is how long each taint has been on the node, which is what
/// `tolerationSeconds` is measured against.
pub fn verdict_for(pod: &Value, taints: &[Value], age_secs: impl Fn(&Value) -> i64) -> Verdict {
    let mut soonest: Option<(i64, String)> = None;
    for taint in taints {
        if taint["effect"].as_str() != Some("NoExecute") {
            continue;
        }
        let key = taint["key"].as_str().unwrap_or("").to_string();
        match pod_tolerates(pod, taint) {
            // Tolerated forever.
            Some(None) => continue,
            // Not tolerated at all: nothing to wait for.
            None => return Verdict::Evict(key),
            Some(Some(secs)) => {
                let remaining = secs - age_secs(taint);
                if remaining <= 0 {
                    return Verdict::Evict(key);
                }
                soonest = match soonest {
                    Some((prev, _)) if prev <= remaining => soonest,
                    _ => Some((remaining, key)),
                };
            }
        }
    }
    match soonest {
        Some((secs, key)) => Verdict::EvictIn(secs, key),
        None => Verdict::Stay,
    }
}

/// Add a taint to a node object, or leave it alone if it is already there.
pub fn add_taint(node: &mut Value, key: &str, effect: &str, now: &str) -> bool {
    let taint = serde_json::json!({ "key": key, "effect": effect, "timeAdded": now });
    match node["spec"]["taints"].as_array_mut() {
        Some(taints) => {
            if taints
                .iter()
                .any(|t| t["key"].as_str() == Some(key) && t["effect"].as_str() == Some(effect))
            {
                return false;
            }
            taints.push(taint);
            true
        }
        None => {
            node["spec"]["taints"] = serde_json::json!([taint]);
            true
        }
    }
}

/// Remove a taint from a node object. Returns whether anything changed.
///
/// **The half that was missing.** A taint that is added when a node goes bad
/// and never removed when it recovers leaves a healthy node permanently
/// unschedulable, and the reason is in `spec` while everyone is looking at
/// `status`.
pub fn remove_taint(node: &mut Value, key: &str, effect: &str) -> bool {
    let Some(taints) = node["spec"]["taints"].as_array_mut() else {
        return false;
    };
    let before = taints.len();
    taints.retain(|t| {
        !(t["key"].as_str() == Some(key) && t["effect"].as_str() == Some(effect))
    });
    before != taints.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn taint(key: &str, effect: &str) -> Value {
        json!({"key": key, "effect": effect})
    }

    #[test]
    fn an_empty_effect_tolerates_every_effect() {
        let tol = json!({"key": "k", "operator": "Exists"});
        assert!(toleration_matches(&tol, &taint("k", "NoSchedule")).is_some());
        assert!(toleration_matches(&tol, &taint("k", "NoExecute")).is_some());
    }

    /// `operator: Exists` with no key is "run me anywhere" — what every
    /// DaemonSet that must survive a broken node writes, Cilium's included.
    #[test]
    fn exists_with_no_key_tolerates_everything() {
        let tol = json!({"operator": "Exists"});
        assert!(toleration_matches(&tol, &taint("anything", "NoExecute")).is_some());
        assert!(toleration_matches(&tol, &taint("other", "NoSchedule")).is_some());
    }

    #[test]
    fn equal_matches_key_and_value_only() {
        let tol = json!({"key": "dedicated", "value": "gpu", "effect": "NoSchedule"});
        assert!(toleration_matches(&tol, &json!({"key":"dedicated","value":"gpu","effect":"NoSchedule"})).is_some());
        // Wrong value.
        assert!(toleration_matches(&tol, &json!({"key":"dedicated","value":"cpu","effect":"NoSchedule"})).is_none());
        // Wrong effect.
        assert!(toleration_matches(&tol, &json!({"key":"dedicated","value":"gpu","effect":"NoExecute"})).is_none());
    }

    /// An unrecognised operator must not accidentally tolerate. Failing closed
    /// means a workload does not run; failing open means it runs somewhere it
    /// was told not to.
    #[test]
    fn an_unknown_operator_does_not_tolerate() {
        let tol = json!({"key": "k", "operator": "Nonsense"});
        assert!(toleration_matches(&tol, &taint("k", "NoSchedule")).is_none());
    }

    /// Tolerations are permissions, so the most permissive wins. Taking the
    /// first match would make list order significant, which it is not.
    #[test]
    fn the_longest_toleration_wins() {
        let pod = json!({"spec": {"tolerations": [
            {"key": "k", "operator": "Exists", "tolerationSeconds": 10},
            {"key": "k", "operator": "Exists", "tolerationSeconds": 300},
        ]}});
        assert_eq!(pod_tolerates(&pod, &taint("k", "NoExecute")), Some(Some(300)));

        // And "forever" beats any finite window, in either order.
        let pod = json!({"spec": {"tolerations": [
            {"key": "k", "operator": "Exists", "tolerationSeconds": 10},
            {"key": "k", "operator": "Exists"},
        ]}});
        assert_eq!(pod_tolerates(&pod, &taint("k", "NoExecute")), Some(None));
    }

    /// Only NoExecute evicts. A running pod has already been placed, so
    /// tearing it down for a NoSchedule taint would apply a placement rule to
    /// something it was never about.
    #[test]
    fn only_no_execute_evicts() {
        let pod = json!({"spec": {}});
        assert_eq!(verdict_for(&pod, &[taint("k", "NoSchedule")], |_| 0), Verdict::Stay);
        assert_eq!(
            verdict_for(&pod, &[taint("k", "PreferNoSchedule")], |_| 0),
            Verdict::Stay
        );
        assert_eq!(
            verdict_for(&pod, &[taint("k", "NoExecute")], |_| 0),
            Verdict::Evict("k".into())
        );
    }

    /// tolerationSeconds is "give the node this long to come back". Evicting
    /// immediately makes a brief blip an outage.
    #[test]
    fn toleration_seconds_delays_the_eviction() {
        let pod = json!({"spec": {"tolerations": [
            {"key": "node.kubernetes.io/not-ready", "operator": "Exists",
             "effect": "NoExecute", "tolerationSeconds": 300}
        ]}});
        let t = taint("node.kubernetes.io/not-ready", "NoExecute");

        // Just applied: nearly the whole window remains.
        assert_eq!(
            verdict_for(&pod, std::slice::from_ref(&t), |_| 10),
            Verdict::EvictIn(290, "node.kubernetes.io/not-ready".into())
        );
        // Window elapsed: go.
        assert_eq!(
            verdict_for(&pod, std::slice::from_ref(&t), |_| 300),
            Verdict::Evict("node.kubernetes.io/not-ready".into())
        );
    }

    /// A pod that tolerates everything forever never leaves — the DaemonSet
    /// case again, and the one that must not regress.
    #[test]
    fn tolerating_everything_means_staying() {
        let pod = json!({"spec": {"tolerations": [{"operator": "Exists"}]}});
        let taints = [taint("a", "NoExecute"), taint("b", "NoExecute")];
        assert_eq!(verdict_for(&pod, &taints, |_| 9999), Verdict::Stay);
    }

    /// With several taints the earliest deadline governs.
    #[test]
    fn the_soonest_deadline_wins() {
        let pod = json!({"spec": {"tolerations": [
            {"key": "a", "operator": "Exists", "tolerationSeconds": 300},
            {"key": "b", "operator": "Exists", "tolerationSeconds": 30},
        ]}});
        let taints = [taint("a", "NoExecute"), taint("b", "NoExecute")];
        assert_eq!(verdict_for(&pod, &taints, |_| 0), Verdict::EvictIn(30, "b".into()));
    }

    #[test]
    fn taints_are_added_once_and_can_be_removed() {
        let mut node = json!({"spec": {}});
        assert!(add_taint(&mut node, "k", "NoSchedule", "now"));
        // Idempotent.
        assert!(!add_taint(&mut node, "k", "NoSchedule", "now"));
        assert_eq!(node["spec"]["taints"].as_array().unwrap().len(), 1);

        // A different effect on the same key is a different taint.
        assert!(add_taint(&mut node, "k", "NoExecute", "now"));
        assert_eq!(node["spec"]["taints"].as_array().unwrap().len(), 2);

        assert!(remove_taint(&mut node, "k", "NoSchedule"));
        assert_eq!(node["spec"]["taints"].as_array().unwrap().len(), 1);
        // Removing what is not there changes nothing.
        assert!(!remove_taint(&mut node, "k", "NoSchedule"));
        assert!(!remove_taint(&mut json!({"spec": {}}), "k", "NoSchedule"));
    }
}
