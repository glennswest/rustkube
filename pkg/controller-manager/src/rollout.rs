//! The arithmetic of a Deployment rollout, as pure functions.
//!
//! Separated from the controller because this is the part that is easy to get
//! wrong and impossible to test through an API client: the whole question is
//! "given these ReplicaSets and this strategy, what should each one's replica
//! count be next", and that is a calculation, not an I/O sequence.
//!
//! The rules the arithmetic has to keep, which are the whole point of a
//! rolling update:
//!
//! - never more than `replicas + maxSurge` pods exist at once
//! - never fewer than `replicas - maxUnavailable` pods are available
//!
//! The scale-down follows upstream's `reconcileOldReplicaSets` (#266).
//!
//! Break the first and a cluster with no headroom cannot roll at all. Break
//! the second and a rollout is an outage. Everything below is in service of
//! those two lines.

use serde_json::Value;

/// A `maxSurge` / `maxUnavailable` value resolved against a replica count.
///
/// Kubernetes spells these as either an integer (`2`) or a percentage string
/// (`"25%"`). Surge rounds **up** and unavailable rounds **down**, which is
/// what keeps `25%` of 1 replica from meaning "no headroom to roll" and
/// "may take the only pod away" respectively.
pub fn int_or_percent(v: &Value, total: u64, round_up: bool) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(s) = v.as_str() {
        if let Some(pct) = s.strip_suffix('%').and_then(|p| p.parse::<u64>().ok()) {
            let scaled = total * pct;
            return if round_up {
                scaled.div_ceil(100)
            } else {
                scaled / 100
            };
        }
        if let Ok(n) = s.parse::<u64>() {
            return n;
        }
    }
    0
}

/// What the controller needs to know about one ReplicaSet to plan a step.
#[derive(Debug, Clone, PartialEq)]
pub struct RsView {
    pub name: String,
    /// `deployment.kubernetes.io/revision`. Ordering for scale-down and for
    /// pruning history: the oldest revision goes first.
    pub revision: u64,
    /// `spec.replicas` — what it has been told to run.
    pub spec_replicas: u64,
    /// `status.availableReplicas` — what is actually serving.
    pub available: u64,
}

/// The replica counts a single reconcile pass should write.
///
/// A plan rather than a sequence of calls so the decision can be asserted on
/// directly. `old` carries only the ReplicaSets whose count changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub new_replicas: u64,
    pub old: Vec<(String, u64)>,
}

/// `Recreate`: every old pod goes away before any new one starts.
///
/// The strategy that accepts downtime in exchange for never running two
/// versions at once — which is the right trade for anything holding an
/// exclusive lock or a schema it is migrating.
pub fn plan_recreate(desired: u64, _new: &RsView, olds: &[RsView]) -> Plan {
    let old_running: u64 = olds.iter().map(|r| r.spec_replicas).sum();
    if old_running > 0 {
        // Take the old ones down first, and do not start the new one yet:
        // "not at the same time" is the entire promise of this strategy.
        return Plan {
            new_replicas: 0,
            old: olds
                .iter()
                .filter(|r| r.spec_replicas > 0)
                .map(|r| (r.name.clone(), 0))
                .collect(),
        };
    }
    Plan { new_replicas: desired, old: Vec::new() }
}

/// `RollingUpdate`: step the new ReplicaSet up and the old ones down while
/// holding both invariants at the top of this module.
///
/// One step per call. The controller runs again whenever the Deployment or
/// one of its ReplicaSets changes, so convergence is that sequence of passes'
/// job — which is also what makes it safe: each pass re-reads the
/// world, so a pod that failed to become ready simply stalls the rollout
/// instead of the controller marching on from a stale plan.
pub fn plan_rolling(
    desired: u64,
    max_surge: u64,
    max_unavailable: u64,
    new: &RsView,
    olds: &[RsView],
) -> Plan {
    // --- scale the new one up ------------------------------------------
    //
    // Bounded by the surge ceiling, counting what every ReplicaSet has been
    // *told* to run rather than what is running: pods that are still starting
    // occupy the headroom, and ignoring them is how a rollout overshoots.
    let max_total = desired + max_surge;
    let current_total: u64 =
        new.spec_replicas + olds.iter().map(|r| r.spec_replicas).sum::<u64>();
    let mut new_replicas = new.spec_replicas;
    if current_total < max_total && new.spec_replicas < desired {
        let headroom = max_total - current_total;
        let want = desired - new.spec_replicas;
        new_replicas = new.spec_replicas + headroom.min(want);
    }
    // A scale-*down* of the deployment must still reach the new ReplicaSet,
    // or `kubectl scale` on a rolled-out Deployment does nothing.
    if new.spec_replicas > desired {
        new_replicas = desired;
    }

    // Upstream's rolloutRolling: a pass that scales the new ReplicaSet does
    // nothing else; the old ones are looked at on the next pass, against
    // what the new one now has.
    if new_replicas != new.spec_replicas {
        return Plan { new_replicas, old: Vec::new() };
    }

    // --- scale the old ones down ---------------------------------------
    //
    // Upstream's reconcileOldReplicaSets (#266). How far the old ones may go
    // in all is the pods every ReplicaSet runs, less the availability floor,
    // less the new ReplicaSet's pods that are not available yet:
    //
    //   maxScaledDown = allPods - minAvailable - newUnavailable
    //
    // Within that, first the old pods that are not available anyway (they
    // count toward nothing), then available ones — but only as many as
    // `available - minAvailable`, so what stays available never drops
    // under the floor. That second bound is on *available* pods alone: an
    // old pod may go while the new one is still pending, provided the floor
    // holds without it. With `replicas: 1, maxUnavailable: 1` the floor is
    // 0, so the old pod goes and a new one that could not schedule beside it
    // (a required anti-affinity on one node) schedules after it; counting
    // the pending new pod against the available ones, as this did before,
    // left that rollout stalled for ever.
    let min_available = desired.saturating_sub(max_unavailable);
    let all_pods: u64 = new.spec_replicas + olds.iter().map(|r| r.spec_replicas).sum::<u64>();
    let new_unavailable = new.spec_replicas.saturating_sub(new.available);
    let max_scaled_down = all_pods.saturating_sub(min_available).saturating_sub(new_unavailable);
    if max_scaled_down == 0 {
        return Plan { new_replicas, old: Vec::new() };
    }

    // Oldest revision first: the further behind a ReplicaSet is, the less
    // anyone wants to roll back to it.
    let mut ordered: Vec<RsView> = olds.iter().filter(|r| r.spec_replicas > 0).cloned().collect();
    ordered.sort_by_key(|r| r.revision);

    // 1. Unhealthy old replicas, up to maxScaledDown.
    let mut cleaned = 0;
    for rs in ordered.iter_mut() {
        if cleaned >= max_scaled_down {
            break;
        }
        let unhealthy = rs.spec_replicas.saturating_sub(rs.available);
        let down = unhealthy.min(max_scaled_down - cleaned);
        rs.spec_replicas -= down;
        cleaned += down;
    }

    // 2. Healthy ones, down to the floor.
    let available: u64 = new.available + olds.iter().map(|r| r.available).sum::<u64>();
    let mut budget = available.saturating_sub(min_available);
    for rs in ordered.iter_mut() {
        if budget == 0 {
            break;
        }
        let down = rs.spec_replicas.min(budget);
        rs.spec_replicas -= down;
        budget -= down;
    }

    let old = ordered
        .into_iter()
        .filter(|r| olds.iter().any(|o| o.name == r.name && o.spec_replicas != r.spec_replicas))
        .map(|r| (r.name, r.spec_replicas))
        .collect();
    Plan { new_replicas, old }
}

/// Which ReplicaSets to delete to honour `revisionHistoryLimit`.
///
/// Only ones already scaled to zero — a ReplicaSet still running pods is not
/// history. Oldest revision first, and the returned names are the ones to
/// remove. **The limit counts retained *old* ReplicaSets**, so a limit of 0
/// keeps none and makes rollback impossible, which is what it means upstream.
pub fn prunable(olds: &[RsView], limit: u64) -> Vec<String> {
    let mut idle: Vec<&RsView> = olds.iter().filter(|r| r.spec_replicas == 0).collect();
    idle.sort_by_key(|r| r.revision);
    let keep = limit as usize;
    if idle.len() <= keep {
        return Vec::new();
    }
    idle[..idle.len() - keep].iter().map(|r| r.name.clone()).collect()
}

/// The revision a newly-created ReplicaSet should carry.
///
/// One past the highest that has ever existed for this Deployment — including
/// ReplicaSets scaled to zero, which is what makes a rollback a *new* revision
/// rather than a return to an old number. Two revisions with the same number
/// would make `rollout history` ambiguous and `rollout undo --to-revision`
/// pick arbitrarily.
pub fn next_revision(owned: &[RsView]) -> u64 {
    owned.iter().map(|r| r.revision).max().unwrap_or(0) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rs(name: &str, revision: u64, spec: u64, available: u64) -> RsView {
        RsView { name: name.into(), revision, spec_replicas: spec, available }
    }

    #[test]
    fn surge_rounds_up_and_unavailable_rounds_down() {
        // 25% of 1 replica: surge must give at least one pod of headroom or a
        // single-replica Deployment can never roll; unavailable must give zero
        // or the only pod may be taken away.
        assert_eq!(int_or_percent(&json!("25%"), 1, true), 1);
        assert_eq!(int_or_percent(&json!("25%"), 1, false), 0);
        assert_eq!(int_or_percent(&json!("25%"), 8, true), 2);
        assert_eq!(int_or_percent(&json!("25%"), 8, false), 2);
        assert_eq!(int_or_percent(&json!(3), 8, true), 3);
        // A malformed value is no headroom rather than a panic.
        assert_eq!(int_or_percent(&json!("banana"), 8, true), 0);
        assert_eq!(int_or_percent(&Value::Null, 8, true), 0);
    }

    /// The first step of a fresh rollout: surge up, and take nothing down
    /// until the new pods are actually available.
    #[test]
    fn a_rollout_surges_before_it_takes_anything_down() {
        let new = rs("new", 2, 0, 0);
        let olds = vec![rs("old", 1, 3, 3)];
        let plan = plan_rolling(3, 1, 1, &new, &olds);
        assert_eq!(plan.new_replicas, 1, "one pod of surge headroom");
        assert!(
            plan.old.is_empty(),
            "nothing may come down while the new pod is not available: {:?}",
            plan.old
        );
    }

    /// Once a new pod is available, the budget opens and an old one goes.
    #[test]
    fn an_available_new_pod_releases_an_old_one() {
        let new = rs("new", 2, 1, 1);
        let olds = vec![rs("old", 1, 3, 3)];
        let plan = plan_rolling(3, 1, 1, &new, &olds);
        // 4 available, floor is 2, new has none pending -> 2 may go.
        assert_eq!(plan.old, vec![("old".to_string(), 1)]);
    }

    /// The invariant that matters: a step never drops available capacity
    /// below `replicas - maxUnavailable`. As upstream, an old pod may go
    /// while a new one is still pending, as long as what stays available
    /// holds the floor without it.
    #[test]
    fn the_availability_floor_is_never_broken() {
        // 3 desired, maxUnavailable 1 -> floor of 2 available.
        // New RS told to run 2 but only 1 is available: 1 unavailable.
        let new = rs("new", 2, 2, 1);
        let olds = vec![rs("old", 1, 2, 2)];
        let plan = plan_rolling(3, 1, 1, &new, &olds);
        // 3 available, floor 2: one old pod may go (upstream's arithmetic).
        assert_eq!(plan.old, vec![("old".to_string(), 1)]);

        // And the floor genuinely holds: what would remain available if every
        // planned scale-down happened at once.
        let remaining: u64 = new.available
            + olds
                .iter()
                .map(|o| {
                    plan.old
                        .iter()
                        .find(|(n, _)| n == &o.name)
                        .map(|(_, n)| *n)
                        .unwrap_or(o.spec_replicas)
                        .min(o.available)
                })
                .sum::<u64>();
        assert!(remaining >= 3 - 1, "dropped to {remaining}, floor is 2");

        // At the floor already: nothing goes.
        let new = rs("new", 2, 2, 0);
        let olds = vec![rs("old", 1, 2, 2)];
        assert!(plan_rolling(3, 1, 1, &new, &olds).old.is_empty());
    }

    /// #266: one replica, maxUnavailable 1 (100%), the new pod unschedulable
    /// until the old one is gone (a required anti-affinity on one node).
    /// The old ReplicaSet goes to 0 and the rollout completes.
    #[test]
    fn a_surge_pod_that_cannot_schedule_does_not_stall_the_rollout() {
        // Pass 1: the surge pod is asked for; nothing else this pass.
        let mut new = rs("new", 2, 0, 0);
        let mut old = rs("old", 1, 1, 1);
        let plan = plan_rolling(1, 1, 1, &new, std::slice::from_ref(&old));
        assert_eq!(plan, Plan { new_replicas: 1, old: vec![] });
        new.spec_replicas = 1;
        // Pass 2: it is Pending; the old one may go anyway (floor 0).
        let plan = plan_rolling(1, 1, 1, &new, std::slice::from_ref(&old));
        assert_eq!(plan, Plan { new_replicas: 1, old: vec![("old".into(), 0)] });
        old.spec_replicas = 0;
        old.available = 0;
        // The new pod schedules and becomes available: settled.
        new.available = 1;
        let plan = plan_rolling(1, 1, 1, &new, std::slice::from_ref(&old));
        assert_eq!(plan, Plan { new_replicas: 1, old: vec![] });

        // With maxUnavailable 0 the floor is 1: the old pod stays, as
        // upstream (that Deployment cannot roll on one node).
        let new = rs("new", 2, 1, 0);
        let old = rs("old", 1, 1, 1);
        assert!(plan_rolling(1, 1, 0, &new, &[old]).old.is_empty());
    }

    /// Old pods that are not available go first, within maxScaledDown.
    #[test]
    fn unhealthy_old_replicas_go_first() {
        // 4 desired, surge 1, unavailable 1 -> floor 3. Old runs 4, 2 of them
        // not available; new runs 1, available.
        let new = rs("new", 3, 1, 1);
        let olds = vec![rs("old", 1, 4, 2)];
        let plan = plan_rolling(4, 1, 1, &new, &olds);
        // allPods 5 - floor 3 - 0 = 2: both unhealthy go; available 3 = floor,
        // so no healthy one does.
        assert_eq!(plan.old, vec![("old".to_string(), 2)]);
    }

    /// The surge ceiling holds: total told-to-run never exceeds desired+surge.
    #[test]
    fn the_surge_ceiling_is_never_exceeded() {
        let new = rs("new", 2, 0, 0);
        let olds = vec![rs("old", 1, 10, 10)];
        let plan = plan_rolling(10, 2, 2, &new, &olds);
        let total = plan.new_replicas + 10;
        assert!(total <= 12, "surged to {total}, ceiling is 12");
    }

    /// Repeated steps converge: new at desired, every old at zero.
    #[test]
    fn stepping_converges_to_the_new_replicaset_alone() {
        let desired = 4;
        let mut new = rs("new", 2, 0, 0);
        let mut olds = vec![rs("old", 1, 4, 4)];

        for _ in 0..20 {
            let plan = plan_rolling(desired, 1, 1, &new, &olds);
            new.spec_replicas = plan.new_replicas;
            // Pods become available immediately in this model; the point of
            // the test is the arithmetic converging, not pod startup.
            new.available = new.spec_replicas;
            for (name, n) in &plan.old {
                if let Some(o) = olds.iter_mut().find(|o| &o.name == name) {
                    o.spec_replicas = *n;
                    o.available = *n;
                }
            }
            if new.spec_replicas == desired && olds.iter().all(|o| o.spec_replicas == 0) {
                return;
            }
        }
        panic!("did not converge: new={new:?} olds={olds:?}");
    }

    /// A plain scale-down of an already-rolled-out Deployment reaches the new
    /// ReplicaSet — otherwise `kubectl scale` silently does nothing.
    #[test]
    fn scaling_down_a_settled_deployment_shrinks_the_current_replicaset() {
        let new = rs("new", 2, 5, 5);
        let plan = plan_rolling(2, 1, 1, &new, &[]);
        assert_eq!(plan.new_replicas, 2);
    }

    /// Recreate takes everything down before it starts anything.
    #[test]
    fn recreate_never_runs_both_versions() {
        let new = rs("new", 2, 0, 0);
        let olds = vec![rs("old", 1, 3, 3)];
        let plan = plan_recreate(3, &new, &olds);
        assert_eq!(plan.new_replicas, 0, "started the new version too early");
        assert_eq!(plan.old, vec![("old".to_string(), 0)]);

        // Once the old ones are gone, the new one starts.
        let olds = vec![rs("old", 1, 0, 0)];
        let plan = plan_recreate(3, &new, &olds);
        assert_eq!(plan.new_replicas, 3);
    }

    /// History keeps the newest N idle ReplicaSets and prunes the rest, and
    /// never prunes one that is still running pods.
    #[test]
    fn history_prunes_oldest_idle_only() {
        let olds = vec![
            rs("r1", 1, 0, 0),
            rs("r2", 2, 0, 0),
            rs("r3", 3, 0, 0),
            rs("r4", 4, 2, 2), // still running — not history
        ];
        assert_eq!(prunable(&olds, 2), vec!["r1".to_string()]);
        assert_eq!(prunable(&olds, 10), Vec::<String>::new());
        // A limit of zero keeps nothing, which is what it means upstream.
        assert_eq!(
            prunable(&olds, 0),
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()]
        );
    }

    /// A rollback is a new revision, not a return to an old number — two
    /// ReplicaSets sharing a revision would make `rollout undo --to-revision`
    /// ambiguous.
    #[test]
    fn a_revision_is_always_one_past_the_highest_ever_seen() {
        let owned = vec![rs("a", 1, 0, 0), rs("b", 3, 2, 2), rs("c", 2, 0, 0)];
        assert_eq!(next_revision(&owned), 4);
        assert_eq!(next_revision(&[]), 1);
    }
}
