//! `metadata.generation` for built-in objects (#285), as upstream's
//! strategies keep it.
//!
//! A kind that tracks it gets `1` on create and one more on every update
//! through its main resource that changes what it asks for — its `spec`
//! (a Deployment also its annotations, a webhook configuration its
//! `webhooks`). `/status` writes and metadata-only writes leave it, and a
//! value the client sent is never taken. It is what a controller's
//! `status.observedGeneration` is compared with: `kubectl rollout status`,
//! `kubectl wait`, and a Pod's `status.observedGeneration` (1.35, the
//! kubelet's). Every update keeps the stored value otherwise, and the update
//! that starts a deletion (sets `deletionTimestamp`) moves a positive
//! generation forward, as upstream's `BeforeDelete`.
//!
//! Custom resources are [`crate::handlers::resource::Strategy`]'s; their
//! keys carry the group, so [`resource_of`] never names one.

use serde_json::{json, Value};

/// What an update has to change to move the generation on.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tracks {
    /// `spec`.
    Spec,
    /// `spec` or `metadata.annotations` (Deployment).
    SpecAndAnnotations,
    /// `webhooks` (Mutating/ValidatingWebhookConfiguration).
    Webhooks,
    /// `spec` without `nodeName` (Pod): a main-resource update cannot change
    /// a bound Pod's node, and the bind itself is upstream's `pods/binding`,
    /// which does not count — rustkube's scheduler binds with a PUT.
    PodSpec,
}

fn tracks(resource: &str) -> Option<Tracks> {
    Some(match resource {
        "pods" => Tracks::PodSpec,
        "deployments" => Tracks::SpecAndAnnotations,
        "mutatingwebhookconfigurations" | "validatingwebhookconfigurations" => Tracks::Webhooks,
        "replicasets" | "statefulsets" | "daemonsets" | "replicationcontrollers" | "jobs" | "cronjobs"
        | "poddisruptionbudgets" | "networkpolicies" | "ingresses" | "customresourcedefinitions"
        | "validatingadmissionpolicies" | "validatingadmissionpolicybindings"
        | "mutatingadmissionpolicies" | "mutatingadmissionpolicybindings" | "flowschemas"
        | "prioritylevelconfigurations" | "deviceclasses" | "resourceslices" => Tracks::Spec,
        _ => return None,
    })
}

/// The built-in resource a store key holds: `/registry/{plural}/…`. A custom
/// resource's key is `/registry/{group}/{plural}/…`, and a group has a dot.
pub fn resource_of(key: &str) -> Option<&str> {
    let seg = key.strip_prefix("/registry/")?.split('/').next()?;
    (!seg.is_empty() && !seg.contains('.')).then_some(seg)
}

/// A create of `resource`: generation 1 if the kind tracks it.
pub fn on_create(resource: &str, obj: &mut Value) {
    if tracks(resource).is_none() {
        return;
    }
    if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.insert("generation".into(), json!(1));
    }
}

/// An update of the object at `key` from `stored` to `obj`. Idempotent: the
/// write paths apply it again after admission webhooks.
pub fn on_update(key: &str, stored: &Value, obj: &mut Value) {
    on_update_through(crate::admission::subresource().as_deref(), key, stored, obj)
}

/// [`on_update`] for a write through `subresource`. A `/status` write keeps
/// the stored generation, as upstream's status strategies: the Deployment
/// controller's revision annotation rides on its status write and is no new
/// spec. `/scale`, `/resize` and `/ephemeralcontainers` change the spec and
/// count.
fn on_update_through(subresource: Option<&str>, key: &str, stored: &Value, obj: &mut Value) {
    let Some(resource) = resource_of(key) else { return };
    if subresource == Some("status") {
        match &stored["metadata"]["generation"] {
            Value::Null => {
                obj["metadata"].as_object_mut().map(|m| m.remove("generation"));
            }
            g => obj["metadata"]["generation"] = g.clone(),
        }
        return;
    }
    if !obj.get("metadata").is_some_and(Value::is_object) {
        return;
    }
    let old = stored["metadata"]["generation"].as_i64().filter(|g| *g > 0);
    let mut generation = match tracks(resource) {
        // An object stored before generations were kept counts as 1.
        Some(t) => {
            let base = old.unwrap_or(1);
            if changed(t, stored, obj) { base + 1 } else { base }
        }
        None => match old {
            Some(g) => g,
            None => {
                obj["metadata"].as_object_mut().map(|m| m.remove("generation"));
                return;
            }
        },
    };
    let deleting = stored["metadata"]["deletionTimestamp"].is_null() && obj["metadata"]["deletionTimestamp"].is_string();
    if deleting {
        generation += 1;
    }
    obj["metadata"]["generation"] = json!(generation);
}

fn changed(t: Tracks, stored: &Value, obj: &Value) -> bool {
    match t {
        Tracks::Spec => stored.get("spec") != obj.get("spec"),
        Tracks::SpecAndAnnotations => {
            stored.get("spec") != obj.get("spec")
                || stored["metadata"].get("annotations") != obj["metadata"].get("annotations")
        }
        Tracks::Webhooks => stored.get("webhooks") != obj.get("webhooks"),
        Tracks::PodSpec => {
            let without_node = |v: &Value| {
                let mut s = v.get("spec").cloned().unwrap_or(Value::Null);
                if let Some(m) = s.as_object_mut() {
                    m.remove("nodeName");
                }
                s
            };
            without_node(stored) != without_node(obj)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(o: &Value) -> Option<i64> {
        o["metadata"]["generation"].as_i64()
    }

    #[test]
    fn keys_name_built_ins_only() {
        assert_eq!(resource_of("/registry/deployments/default/web"), Some("deployments"));
        assert_eq!(resource_of("/registry/nodes/n1"), Some("nodes"));
        assert_eq!(resource_of("/registry/example.com/widgets/default/w"), None);
        assert_eq!(resource_of("/other/deployments/x"), None);
    }

    #[test]
    fn create_starts_at_one_for_tracked_kinds_and_ignores_the_client() {
        let mut d = json!({"metadata": {"name": "web", "generation": 9}, "spec": {"replicas": 1}});
        on_create("deployments", &mut d);
        assert_eq!(gen(&d), Some(1));
        let mut p = json!({"metadata": {"name": "p"}, "spec": {}});
        on_create("pods", &mut p);
        assert_eq!(gen(&p), Some(1), "Pods track generation since 1.35");
        let mut cm = json!({"metadata": {"name": "c"}, "data": {}});
        on_create("configmaps", &mut cm);
        assert_eq!(gen(&cm), None);
    }

    #[test]
    fn a_spec_change_moves_it_and_nothing_else_does() {
        let key = "/registry/replicasets/default/rs";
        let stored = json!({"metadata": {"name": "rs", "generation": 3}, "spec": {"replicas": 1}, "status": {"replicas": 0}});
        let mut same = json!({"metadata": {"name": "rs", "generation": 40, "labels": {"a": "b"}}, "spec": {"replicas": 1}, "status": {"replicas": 1}});
        on_update(key, &stored, &mut same);
        assert_eq!(gen(&same), Some(3), "labels, status and a client's value leave it");
        let mut scaled = json!({"metadata": {"name": "rs"}, "spec": {"replicas": 2}});
        on_update(key, &stored, &mut scaled);
        assert_eq!(gen(&scaled), Some(4));
        on_update(key, &stored, &mut scaled);
        assert_eq!(gen(&scaled), Some(4), "idempotent: applied again after webhooks");
        // Stored before generations were kept.
        let old = json!({"metadata": {"name": "rs"}, "spec": {"replicas": 1}});
        let mut up = json!({"metadata": {"name": "rs"}, "spec": {"replicas": 5}});
        on_update(key, &old, &mut up);
        assert_eq!(gen(&up), Some(2));
    }

    #[test]
    fn per_kind_rules() {
        let stored = json!({"metadata": {"name": "d", "generation": 1}, "spec": {"replicas": 1}});
        let mut annotated = json!({"metadata": {"name": "d", "annotations": {"x": "1"}}, "spec": {"replicas": 1}});
        on_update("/registry/deployments/default/d", &stored, &mut annotated);
        assert_eq!(gen(&annotated), Some(2), "a Deployment counts annotations");
        let mut rs = annotated.clone();
        on_update("/registry/replicasets/default/d", &stored, &mut rs);
        assert_eq!(gen(&rs), Some(1), "a ReplicaSet does not");

        let hook = json!({"metadata": {"name": "h", "generation": 2}, "webhooks": [{"name": "a"}]});
        let mut changed = json!({"metadata": {"name": "h"}, "webhooks": [{"name": "b"}]});
        on_update("/registry/validatingwebhookconfigurations/h", &hook, &mut changed);
        assert_eq!(gen(&changed), Some(3));

        // The scheduler's bind is a PUT of spec.nodeName: not a change.
        let pod = json!({"metadata": {"name": "p", "generation": 1}, "spec": {"containers": [{"image": "a"}]}});
        let mut bound = json!({"metadata": {"name": "p"}, "spec": {"containers": [{"image": "a"}], "nodeName": "n1"}});
        on_update("/registry/pods/default/p", &pod, &mut bound);
        assert_eq!(gen(&bound), Some(1));
        let mut resized = json!({"metadata": {"name": "p"}, "spec": {"containers": [{"image": "b"}], "nodeName": "n1"}});
        on_update("/registry/pods/default/p", &bound, &mut resized);
        assert_eq!(gen(&resized), Some(2));
    }

    #[test]
    fn untracked_kinds_keep_what_is_stored() {
        let key = "/registry/configmaps/default/c";
        let mut sent = json!({"metadata": {"name": "c", "generation": 5}, "data": {"a": "1"}});
        on_update(key, &json!({"metadata": {"name": "c"}}), &mut sent);
        assert_eq!(gen(&sent), None, "a client cannot set it");
        let mut kept = json!({"metadata": {"name": "c"}, "data": {"a": "2"}});
        on_update(key, &json!({"metadata": {"name": "c", "generation": 2}}), &mut kept);
        assert_eq!(gen(&kept), Some(2));
    }

    #[test]
    fn a_status_write_keeps_it_whatever_it_carries() {
        let key = "/registry/deployments/default/d";
        let stored = json!({"metadata": {"name": "d", "generation": 2}, "spec": {"replicas": 1}});
        let mut revision = json!({"metadata": {"name": "d", "generation": 9,
            "annotations": {"deployment.kubernetes.io/revision": "1"}}, "spec": {"replicas": 1}, "status": {}});
        on_update_through(Some("status"), key, &stored, &mut revision);
        assert_eq!(gen(&revision), Some(2), "the controller's revision annotation on /status");
        let mut scaled = json!({"metadata": {"name": "d"}, "spec": {"replicas": 3}});
        on_update_through(Some("scale"), key, &stored, &mut scaled);
        assert_eq!(gen(&scaled), Some(3), "/scale changes the spec");
    }

    #[test]
    fn starting_a_deletion_moves_it_on() {
        let key = "/registry/deployments/default/d";
        let stored = json!({"metadata": {"name": "d", "generation": 2}, "spec": {}});
        let mut marked = json!({"metadata": {"name": "d", "deletionTimestamp": "2026-10-21T00:00:00Z"}, "spec": {}});
        on_update(key, &stored, &mut marked);
        assert_eq!(gen(&marked), Some(3));
        // Already deleting: a finalizer removal does not move it again.
        let deleting = marked.clone();
        let mut fin = json!({"metadata": {"name": "d", "deletionTimestamp": "2026-10-21T00:00:00Z"}, "spec": {}});
        on_update(key, &deleting, &mut fin);
        assert_eq!(gen(&fin), Some(3));
        // An untracked kind without a generation stays without one.
        let mut cm = json!({"metadata": {"name": "c", "deletionTimestamp": "2026-10-21T00:00:00Z"}});
        on_update("/registry/configmaps/default/c", &json!({"metadata": {"name": "c"}}), &mut cm);
        assert_eq!(gen(&cm), None);
    }
}
