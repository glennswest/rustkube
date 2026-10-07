//! What kube-controller-manager and kube-scheduler may do (#176).
//!
//! Both were bound to `cluster-admin` at bootstrap ("tighten later"), so a
//! stolen controller or scheduler client certificate was the whole cluster.
//! They now get their own ClusterRoles, reconciled at every boot like the
//! project roles, and the bootstrap bindings point at them.
//!
//! **The controller-manager** runs every controller under one identity, so
//! upstream's small `system:kube-controller-manager` role (which assumes a
//! ServiceAccount per controller) does not fit; owner's decision on #176: one
//! role written from what rustkube's controllers call.
//! - It reads, patches, updates and deletes every resource: the garbage
//!   collector and namespace deletion walk everything discovery lists (as
//!   upstream's `generic-garbage-collector` and `namespace-controller` do).
//!   An RBAC object it updates is still held to escalation prevention (#98):
//!   it cannot grant what it does not hold.
//! - It creates only what its controllers create.
//! - It holds no `bind`, `escalate`, `impersonate`, no ServiceAccount token
//!   requests, and creates no RBAC objects, Secrets or Namespaces.
//!
//! **The scheduler** gets upstream's `system:kube-scheduler` shape for what
//! rustkube's scheduler does: it reads what placement needs, binds by
//! updating the Pod (rustkube binds with a PUT, not `pods/binding`), writes
//! `pods/status`, a claim's selected-node, the VMI and VMI-migration status
//! (#72, #184), its Lease and its Events (#138).

use serde_json::{json, Value};

pub const CONTROLLER_MANAGER: &str = "system:kube-controller-manager";
pub const SCHEDULER: &str = "system:kube-scheduler";

fn rule(groups: &[&str], resources: &[&str], verbs: &[&str]) -> Value {
    json!({"apiGroups": groups, "resources": resources, "verbs": verbs})
}

fn cluster_role(name: &str, description: &str, rules: Vec<Value>) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {"name": name, "annotations": {"rustkube.io/description": description}},
        "rules": rules,
    })
}

pub fn controller_manager_role() -> Value {
    cluster_role(
        CONTROLLER_MANAGER,
        "kube-controller-manager: every controller under one identity (#176).",
        vec![
            // Informers, the garbage collector and namespace deletion.
            rule(&["*"], &["*"], &["get", "list", "watch", "patch", "update", "delete", "deletecollection"]),
            // What the controllers create, and nothing else.
            rule(&[""], &["pods", "events", "serviceaccounts", "configmaps", "persistentvolumes", "endpoints"], &["create"]),
            rule(&["apps"], &["replicasets", "controllerrevisions"], &["create"]),
            rule(&["batch"], &["jobs"], &["create"]),
            rule(&["discovery.k8s.io"], &["endpointslices"], &["create"]),
            rule(&["coordination.k8s.io"], &["leases"], &["create"]),
            rule(&["events.k8s.io"], &["events"], &["create"]),
            rule(&["storage.k8s.io"], &["volumeattachments"], &["create"]),
            rule(&["kubevirt.io"], &["virtualmachineinstances", "virtualmachineinstancemigrations"], &["create"]),
            rule(&["rustkube.io"], &["podmigrations"], &["create"]),
            // The CSR controller approves kubelet client CSRs and signs them.
            json!({"apiGroups": ["certificates.k8s.io"], "resources": ["signers"],
                   "resourceNames": ["kubernetes.io/kube-apiserver-client-kubelet"], "verbs": ["approve", "sign"]}),
        ],
    )
}

pub fn scheduler_role() -> Value {
    let read = &["get", "list", "watch"];
    cluster_role(
        SCHEDULER,
        "kube-scheduler: placement reads, binding, and the statuses it reports (#176).",
        vec![
            rule(&[""], &["pods", "nodes", "persistentvolumeclaims", "persistentvolumes", "namespaces",
                         "services", "replicationcontrollers"], read),
            rule(&["apps"], &["replicasets", "statefulsets"], read),
            rule(&["policy"], &["poddisruptionbudgets"], read),
            rule(&["storage.k8s.io"], &["storageclasses", "csinodes", "csidrivers", "csistoragecapacities",
                                        "volumeattachments"], read),
            rule(&["scheduling.k8s.io"], &["priorityclasses"], read),
            rule(&["apiextensions.k8s.io"], &["customresourcedefinitions"], read),
            rule(&["kubevirt.io"], &["virtualmachineinstances", "virtualmachineinstancemigrations"], read),
            // Bind (a PUT of the Pod), PodScheduled, a claim's selected-node.
            rule(&[""], &["pods"], &["update"]),
            rule(&[""], &["pods/status"], &["patch", "update"]),
            rule(&[""], &["pods/binding", "bindings"], &["create"]),
            rule(&[""], &["persistentvolumeclaims"], &["patch", "update"]),
            rule(&["kubevirt.io"], &["virtualmachineinstances/status", "virtualmachineinstancemigrations/status"],
                 &["patch", "update"]),
            rule(&["coordination.k8s.io"], &["leases"], &["create", "get", "update"]),
            rule(&["", "events.k8s.io"], &["events"], &["create", "patch", "update"]),
        ],
    )
}

/// The bootstrap binding of `user` to the ClusterRole of the same name.
pub fn binding(user: &str) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {"name": user},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": user},
        "subjects": [{"kind": "User", "name": user, "apiGroup": "rbac.authorization.k8s.io"}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbac_engine::{rule_matches, AuthorizationRequest};

    fn allows(role: &Value, verb: &str, group: &str, resource: &str, sub: Option<&str>, name: Option<&str>) -> bool {
        let req = AuthorizationRequest {
            verb: verb.into(), resource: resource.into(), subresource: sub.map(str::to_string),
            api_group: group.into(), namespace: Some("ns".into()), name: name.map(str::to_string),
        };
        role["rules"].as_array().unwrap().iter().any(|r| rule_matches(r, &req))
    }

    #[test]
    fn the_controller_manager_does_its_job_and_no_more() {
        let cm = controller_manager_role();
        // What the controllers do.
        for (verb, group, res) in [("create", "", "pods"), ("create", "apps", "replicasets"), ("create", "batch", "jobs"),
            ("create", "", "persistentvolumes"), ("create", "kubevirt.io", "virtualmachineinstances"),
            ("create", "coordination.k8s.io", "leases"), ("delete", "", "secrets"), ("delete", "rbac.authorization.k8s.io", "rolebindings"),
            ("list", "cilium.io", "ciliumnodes"), ("patch", "apps", "deployments"), ("update", "", "namespaces")] {
            assert!(allows(&cm, verb, group, res, None, None), "{verb} {group}/{res}");
        }
        assert!(allows(&cm, "update", "certificates.k8s.io", "certificatesigningrequests", Some("approval"), None));
        assert!(allows(&cm, "approve", "certificates.k8s.io", "signers", None, Some("kubernetes.io/kube-apiserver-client-kubelet")));
        // What it must not.
        for (verb, group, res, sub) in [("create", "", "serviceaccounts", Some("token")), ("create", "", "secrets", None),
            ("create", "rbac.authorization.k8s.io", "clusterrolebindings", None), ("create", "rbac.authorization.k8s.io", "roles", None),
            ("bind", "rbac.authorization.k8s.io", "clusterroles", None), ("escalate", "rbac.authorization.k8s.io", "clusterroles", None),
            ("impersonate", "", "users", None), ("create", "", "namespaces", None), ("create", "", "pods", Some("exec"))] {
            assert!(!allows(&cm, verb, group, res, sub, None), "must not {verb} {group}/{res}{sub:?}");
        }
        assert!(!allows(&cm, "approve", "certificates.k8s.io", "signers", None, Some("kubernetes.io/kube-apiserver-client")));
    }

    #[test]
    fn the_scheduler_places_and_reports_and_no_more() {
        let s = scheduler_role();
        for (verb, group, res, sub) in [("list", "", "pods", None), ("update", "", "pods", None), ("patch", "", "pods", Some("status")),
            ("patch", "", "persistentvolumeclaims", None), ("watch", "storage.k8s.io", "csistoragecapacities", None),
            ("patch", "kubevirt.io", "virtualmachineinstances", Some("status")), ("create", "", "events", None),
            ("update", "coordination.k8s.io", "leases", None), ("list", "apiextensions.k8s.io", "customresourcedefinitions", None)] {
            assert!(allows(&s, verb, group, res, sub, None), "{verb} {group}/{res}{sub:?}");
        }
        for (verb, group, res) in [("get", "", "secrets"), ("create", "", "pods"), ("delete", "", "pods"),
            ("update", "kubevirt.io", "virtualmachineinstances"), ("create", "rbac.authorization.k8s.io", "clusterrolebindings")] {
            assert!(!allows(&s, verb, group, res, None, None), "must not {verb} {group}/{res}");
        }
    }
}
