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
/// `Manual` is deliberately not "true": it means start and stop decide, and
/// what they set is the boolean — so the boolean is the only thing left to
/// read, which is what falling through to it does.
pub fn wants_running(vm: &Value) -> bool {
    match vm["spec"]["runStrategy"].as_str() {
        Some("Always") | Some("RerunOnFailure") => return true,
        Some("Halted") => return false,
        _ => {}
    }
    vm["spec"]["running"].as_bool().unwrap_or(false)
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
}
