//! Reading a `VirtualMachine`'s intent.
//!
//! Shared because two components must agree on it and are otherwise unrelated:
//! the VirtualMachine controller reconciles to it, and the apiserver's
//! `start`/`stop` subresources write it. A second copy that read the fields
//! differently would give a VM that says one thing and does another — the
//! shape of failure that put two guests on one MAC when a derivation lived in
//! two places (rustkube-node#39).
//!
//! It lives here rather than in the controller because the apiserver must not
//! depend on a controller: the request path cannot be made to wait on the
//! thing that reconciles it.

use serde_json::Value;

/// Should this VirtualMachine have a running instance?
///
/// `spec.running` is the original boolean and `spec.runStrategy` the newer
/// enum. Upstream forbids setting both, but a client can still send both, so
/// `runStrategy` wins where they disagree: it is the more specific statement.
///
/// With neither the answer is **false**. A VirtualMachine that says nothing
/// about running should not start a guest, and the CRD prints `spec.running`
/// as a column, so an absent field already reads as stopped to anyone looking.
///
/// `Once` is "true": it starts a VMI like `Always` does, and differs only in
/// what happens once that VMI has finished, which is the controller's business
/// (it is not replaced, #104). Read as the boolean, a `Once` VM with no
/// `running` never started at all.
///
/// `Manual` is deliberately not "true": it means start and stop decide, and
/// what they set is the boolean — so the boolean is the only thing left to
/// read, which is what falling through to it does.
pub fn wants_running(vm: &Value) -> bool {
    match vm["spec"]["runStrategy"].as_str() {
        Some("Always") | Some("RerunOnFailure") | Some("Once") => return true,
        Some("Halted") => return false,
        _ => {}
    }
    vm["spec"]["running"].as_bool().unwrap_or(false)
}

/// A VMI's live migration in flight, as `status.migrationState` records it
/// (upstream KubeVirt's field, #184): one with a `migrationUid` that has
/// neither `completed` nor `failed`.
///
/// Who writes what, so the controller, the scheduler and the kubelets
/// (rustkube-node#40) agree:
///
/// - the migration controller: `migrationUid`, `sourceNode`, `mode`; on
///   abort, `abortRequested`; on success it moves `status.nodeName`
/// - the scheduler: `targetNode`, chosen as a placement would be
/// - the target kubelet, once it can receive: `targetNodeAddress` (and
///   `targetDirectMigrationNodePorts` if it uses them)
/// - the source kubelet: `startTimestamp` when it starts sending; then
///   `completed` or `failed` (+ `failureReason`), and `endTimestamp`
pub fn active_migration(vmi: &Value) -> Option<&Value> {
    let state = &vmi["status"]["migrationState"];
    let uid = state["migrationUid"].as_str().filter(|u| !u.is_empty());
    let done = state["completed"].as_bool() == Some(true) || state["failed"].as_bool() == Some(true);
    (uid.is_some() && !done).then_some(state)
}

/// The node a migration in flight is moving this VMI to, once one is chosen.
pub fn migration_target(vmi: &Value) -> Option<&str> {
    active_migration(vmi)?["targetNode"].as_str().filter(|n| !n.is_empty())
}

/// A migration in flight still waiting for the scheduler to choose its
/// target: not chosen, not being aborted.
pub fn wants_migration_target(vmi: &Value) -> bool {
    active_migration(vmi).is_some_and(|m| {
        m["targetNode"].as_str().is_none_or(str::is_empty) && m["abortRequested"].as_bool() != Some(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_strategy_wins_over_the_boolean() {
        assert!(wants_running(&json!({"spec": {"running": false, "runStrategy": "Always"}})));
        assert!(!wants_running(&json!({"spec": {"running": true, "runStrategy": "Halted"}})));
    }

    #[test]
    fn once_starts_a_guest() {
        assert!(wants_running(&json!({"spec": {"runStrategy": "Once"}})));
    }

    #[test]
    fn manual_falls_through_to_what_start_and_stop_set() {
        assert!(wants_running(&json!({"spec": {"running": true, "runStrategy": "Manual"}})));
        assert!(!wants_running(&json!({"spec": {"running": false, "runStrategy": "Manual"}})));
    }

    #[test]
    fn a_vm_that_says_nothing_does_not_start_a_guest() {
        assert!(!wants_running(&json!({"spec": {}})));
        assert!(!wants_running(&json!({})));
    }

    #[test]
    fn the_boolean_still_works() {
        assert!(wants_running(&json!({"spec": {"running": true}})));
        assert!(!wants_running(&json!({"spec": {"running": false}})));
    }

    #[test]
    fn a_migration_is_active_until_it_completes_or_fails() {
        let mut vmi = json!({"status": {"migrationState": {"migrationUid": "m1", "sourceNode": "a"}}});
        assert!(active_migration(&vmi).is_some());
        assert!(wants_migration_target(&vmi));
        assert_eq!(migration_target(&vmi), None);
        vmi["status"]["migrationState"]["targetNode"] = json!("b");
        assert!(!wants_migration_target(&vmi));
        assert_eq!(migration_target(&vmi), Some("b"));
        vmi["status"]["migrationState"]["completed"] = json!(true);
        assert!(active_migration(&vmi).is_none());
        assert_eq!(migration_target(&vmi), None);
        let failed = json!({"status": {"migrationState": {"migrationUid": "m1", "failed": true}}});
        assert!(active_migration(&failed).is_none());
        let aborting = json!({"status": {"migrationState": {"migrationUid": "m1", "abortRequested": true}}});
        assert!(!wants_migration_target(&aborting));
        assert!(active_migration(&json!({"status": {}})).is_none());
    }
}
