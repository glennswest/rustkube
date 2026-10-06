//! RBAC escalation prevention (#98).
//!
//! Without it, anyone who may write a RoleBinding may bind any role — every
//! project's admin could bind `cluster-admin` in their project, and anyone
//! given `create clusterrolebindings` for any reason could make themselves
//! cluster-admin. Upstream's rule, enforced the same way here:
//!
//! - a **Role or ClusterRole** may be written by a caller who holds the
//!   `escalate` verb on it, or who already holds every rule it grants (in
//!   the role's namespace, or cluster-wide for a ClusterRole). A ClusterRole
//!   with an `aggregationRule` needs `escalate`;
//! - a **RoleBinding or ClusterRoleBinding** may be written by a caller who
//!   holds the `bind` verb on the role it references (`roles` or
//!   `clusterroles`, by name, in the binding's namespace), or who already
//!   holds every rule of that role in the binding's scope — the binding's
//!   namespace for a RoleBinding (which may reference a ClusterRole), the
//!   whole cluster for a ClusterRoleBinding.
//!
//! `system:masters` is never checked. An update that changes only what the
//! garbage collector writes (ownerReferences, finalizers) is not checked, as
//! upstream does not, so a GC can finish deleting a role it could not create.
//!
//! "Holds a rule" is upstream's `Covers`: the rule is broken into one verb on
//! one group/resource (and one resourceName, if it names any), and each must
//! be granted by one of the caller's rules — `*` covers anything, a literal
//! `*` in the new rule needs a `*` in the caller's, `*/status` covers every
//! resource's `status`, and (as this authorizer grants them) `pods/*` covers
//! `pods/exec`. A caller's rule naming resourceNames covers only those names.
//!
//! It runs from [`crate::admission::admit`], after the mutating webhooks, so
//! it judges the object that will be stored — every write path that admits
//! (POST, PUT, PATCH, server-side apply's upsert) is covered. Writes the
//! apiserver makes for itself (bootstrap, manifests, the admin RoleBinding a
//! ProjectRequest makes) have no request and are not checked.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
use crate::storage::ResourceStorage;
use serde_json::Value;

const GROUP: &str = "rbac.authorization.k8s.io";

/// Is this a write escalation prevention judges?
pub fn applies(group: &str, resource: &str, subresource: Option<&str>) -> bool {
    group == GROUP
        && subresource.is_none()
        && matches!(resource, "roles" | "clusterroles" | "rolebindings" | "clusterrolebindings")
}

/// Refuse the write of `obj` (a Role, ClusterRole or one of their bindings,
/// named by `resource`) unless `user` may make it. `old` is what is stored
/// now, for an update.
pub async fn confirm(
    rbac: &RbacEngine,
    user: &UserInfo,
    resource: &str,
    namespace: Option<&str>,
    obj: &Value,
    old: Option<&Value>,
) -> Result<(), ApiError> {
    if rbac.is_superuser(user) {
        return Ok(());
    }
    if old.is_some_and(|old| only_gc_fields_changed(old, obj)) {
        return Ok(());
    }
    let namespace = obj["metadata"]["namespace"]
        .as_str()
        .filter(|n| !n.is_empty())
        .or(namespace)
        .filter(|_| matches!(resource, "roles" | "rolebindings"));
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let kind = match resource {
        "roles" => "Role",
        "clusterroles" => "ClusterRole",
        "rolebindings" => "RoleBinding",
        _ => "ClusterRoleBinding",
    };
    let forbidden = |why: String| {
        ApiError::forbidden(&format!("{resource}.{GROUP} \"{name}\" is forbidden: {why}"))
    };
    let ask = |verb: &str, resource: &str, name: &str| AuthorizationRequest {
        verb: verb.into(),
        resource: resource.into(),
        subresource: None,
        api_group: GROUP.into(),
        namespace: namespace.map(str::to_string),
        name: Some(name.to_string()),
    };

    let rules = if matches!(kind, "Role" | "ClusterRole") {
        if rbac.authorize(user, &ask("escalate", resource, name)).await {
            return Ok(());
        }
        if kind == "ClusterRole" && !obj["aggregationRule"].is_null() {
            return Err(forbidden(format!(
                "user \"{}\" must hold \"escalate\" on clusterroles to write a ClusterRole with an aggregationRule",
                user.username
            )));
        }
        obj["rules"].clone()
    } else {
        let ref_kind = obj["roleRef"]["kind"].as_str().unwrap_or("");
        let ref_name = obj["roleRef"]["name"].as_str().unwrap_or("");
        let ref_resource = match (ref_kind, namespace) {
            ("ClusterRole", _) => "clusterroles",
            ("Role", Some(_)) => "roles",
            _ => {
                return Err(ApiError::invalid(&format!(
                    "{kind} \"{name}\" is invalid: roleRef.kind: Unsupported value: \"{ref_kind}\""
                )))
            }
        };
        if rbac.authorize(user, &ask("bind", ref_resource, ref_name)).await {
            return Ok(());
        }
        let key = match namespace {
            Some(ns) if ref_resource == "roles" => ResourceStorage::namespaced_key("roles", ns, ref_name),
            _ => ResourceStorage::cluster_key("clusterroles", ref_name),
        };
        match rbac.storage().get(&key).await {
            Ok(role) => role["rules"].clone(),
            Err(_) => {
                return Err(forbidden(format!(
                    "user \"{}\" may not bind {ref_kind} \"{ref_name}\": it could not be read, and binding it needs \"bind\" on it",
                    user.username
                )))
            }
        }
    };

    let held = rbac.rules_for(user, namespace).await;
    let missing = uncovered(&held.resource_rules, &held.non_resource_rules, rules.as_array().map(Vec::as_slice).unwrap_or(&[]));
    if missing.is_empty() {
        return Ok(());
    }
    let groups = user.groups.iter().map(|g| format!("{g:?}")).collect::<Vec<_>>().join(" ");
    let mut why = format!(
        "user \"{}\" (groups=[{groups}]) is attempting to grant RBAC permissions not currently held:\n{}",
        user.username,
        missing.join("\n")
    );
    if held.incomplete {
        why.push_str("\n(some of the user's own roles could not be read)");
    }
    Err(forbidden(why))
}

/// Did the update change only what the garbage collector writes?
fn only_gc_fields_changed(old: &Value, new: &Value) -> bool {
    let strip = |v: &Value| {
        let mut v = v.clone();
        if let Some(m) = v["metadata"].as_object_mut() {
            for f in ["ownerReferences", "finalizers", "resourceVersion", "managedFields", "generation"] {
                m.remove(f);
            }
        }
        v
    };
    strip(old) == strip(new)
}

/// The parts of `rules` that `owner` does not hold, one line each in
/// upstream's message format. Empty when every one is held.
pub fn uncovered(owner: &[Value], owner_non_resource: &[Value], rules: &[Value]) -> Vec<String> {
    let mut missing = Vec::new();
    for rule in rules {
        let verbs = strings(&rule["verbs"]);
        let names = strings(&rule["resourceNames"]);
        for group in strings(&rule["apiGroups"]) {
            for resource in strings(&rule["resources"]) {
                for verb in &verbs {
                    let held = |name: Option<&str>| {
                        owner.iter().any(|o| covers_resource(o, group, resource, verb, name))
                    };
                    if names.is_empty() {
                        if !held(None) {
                            missing.push(format!(
                                "{{APIGroups:[{group:?}], Resources:[{resource:?}], Verbs:[{verb:?}]}}"
                            ));
                        }
                    } else {
                        for name in &names {
                            if !held(Some(name)) {
                                missing.push(format!(
                                    "{{APIGroups:[{group:?}], Resources:[{resource:?}], ResourceNames:[{name:?}], Verbs:[{verb:?}]}}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        for url in strings(&rule["nonResourceURLs"]) {
            for verb in &verbs {
                if !owner_non_resource.iter().any(|o| covers_url(o, url, verb)) {
                    missing.push(format!("{{NonResourceURLs:[{url:?}], Verbs:[{verb:?}]}}"));
                }
            }
        }
    }
    missing
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn has(list: &[&str], s: &str) -> bool {
    list.iter().any(|x| *x == "*" || *x == s)
}

/// Does the caller's resource rule `o` grant `verb` on `group`/`resource`
/// (for `name`, or for every name when `None`)?
fn covers_resource(o: &Value, group: &str, resource: &str, verb: &str, name: Option<&str>) -> bool {
    if !has(&strings(&o["verbs"]), verb) || !has(&strings(&o["apiGroups"]), group) {
        return false;
    }
    let resources = strings(&o["resources"]);
    let resource_ok = has(&resources, resource)
        || resource.split_once('/').is_some_and(|(base, sub)| {
            resources.iter().any(|r| *r == format!("*/{sub}") || *r == format!("{base}/*"))
        });
    if !resource_ok {
        return false;
    }
    let names = strings(&o["resourceNames"]);
    names.is_empty() || name.is_some_and(|n| names.contains(&n))
}

/// Does the caller's non-resource rule `o` grant `verb` on `url`?
fn covers_url(o: &Value, url: &str, verb: &str) -> bool {
    has(&strings(&o["verbs"]), verb)
        && strings(&o["nonResourceURLs"]).iter().any(|u| {
            *u == url || (u.ends_with('*') && url.starts_with(u.trim_end_matches('*')))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn missing(owner: Value, rules: Value) -> Vec<String> {
        let owner = owner.as_array().unwrap().clone();
        let (res, non): (Vec<_>, Vec<_>) = owner.into_iter().partition(|r| r.get("nonResourceURLs").is_none());
        uncovered(&res, &non, rules.as_array().unwrap())
    }

    #[test]
    fn admin_does_not_hold_cluster_admin() {
        // A project admin's rules, abridged: everything namespaced it may do
        // is named, nothing is `*`. Binding cluster-admin is an escalation.
        let admin = json!([{"apiGroups": [""], "resources": ["pods", "configmaps", "secrets"], "verbs": ["*"]},
                           {"apiGroups": ["rbac.authorization.k8s.io"], "resources": ["rolebindings", "roles"], "verbs": ["*"]}]);
        let cluster_admin = json!([{"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]},
                                   {"nonResourceURLs": ["*"], "verbs": ["*"]}]);
        let m = missing(admin.clone(), cluster_admin);
        assert_eq!(m, vec![
            r#"{APIGroups:["*"], Resources:["*"], Verbs:["*"]}"#,
            r#"{NonResourceURLs:["*"], Verbs:["*"]}"#,
        ]);
        // A subset of what it holds is no escalation.
        let view = json!([{"apiGroups": [""], "resources": ["pods", "configmaps"], "verbs": ["get", "list", "watch"]}]);
        assert!(missing(admin, view).is_empty());
    }

    #[test]
    fn wildcards_cover_everything_and_are_only_covered_by_wildcards() {
        let all = json!([{"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]}, {"nonResourceURLs": ["*"], "verbs": ["*"]}]);
        let anything = json!([{"apiGroups": ["apps", ""], "resources": ["deployments", "pods/exec", "*"], "verbs": ["*", "get"],
                               "resourceNames": ["a"]}, {"nonResourceURLs": ["/metrics", "/logs/*"], "verbs": ["get"]}]);
        assert!(missing(all, anything).is_empty());

        // Every verb named is not `*`: a new verb would be granted too.
        let named = json!([{"apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "watch", "create", "update", "patch", "delete"]}]);
        assert_eq!(missing(named.clone(), json!([{"apiGroups": [""], "resources": ["pods"], "verbs": ["*"]}])).len(), 1);
        assert_eq!(missing(named.clone(), json!([{"apiGroups": ["*"], "resources": ["pods"], "verbs": ["get"]}])).len(), 1);
        assert_eq!(missing(named, json!([{"apiGroups": [""], "resources": ["*"], "verbs": ["get"]}])).len(), 1);
    }

    #[test]
    fn verbs_and_groups_are_each_checked() {
        let reader = json!([{"apiGroups": [""], "resources": ["secrets"], "verbs": ["get", "list"]}]);
        assert_eq!(
            missing(reader.clone(), json!([{"apiGroups": [""], "resources": ["secrets"], "verbs": ["get", "delete"]}])),
            vec![r#"{APIGroups:[""], Resources:["secrets"], Verbs:["delete"]}"#]
        );
        assert_eq!(missing(reader, json!([{"apiGroups": ["apps"], "resources": ["secrets"], "verbs": ["get"]}])).len(), 1);
    }

    #[test]
    fn subresources() {
        // `pods` does not hold `pods/exec` — a shell is not an edit.
        let pods = json!([{"apiGroups": [""], "resources": ["pods"], "verbs": ["*"]}]);
        assert_eq!(missing(pods, json!([{"apiGroups": [""], "resources": ["pods/exec"], "verbs": ["create"]}])).len(), 1);
        // `*/status` holds every status; `pods/*` every subresource of pods.
        let status = json!([{"apiGroups": [""], "resources": ["*/status"], "verbs": ["update"]}]);
        assert!(missing(status.clone(), json!([{"apiGroups": [""], "resources": ["nodes/status", "pods/status"], "verbs": ["update"]}])).is_empty());
        assert_eq!(missing(status, json!([{"apiGroups": [""], "resources": ["pods"], "verbs": ["update"]}])).len(), 1);
        let pod_subs = json!([{"apiGroups": [""], "resources": ["pods/*"], "verbs": ["create"]}]);
        assert!(missing(pod_subs.clone(), json!([{"apiGroups": [""], "resources": ["pods/exec"], "verbs": ["create"]}])).is_empty());
        assert_eq!(missing(pod_subs, json!([{"apiGroups": [""], "resources": ["nodes/proxy"], "verbs": ["create"]}])).len(), 1);
    }

    #[test]
    fn resource_names() {
        let one = json!([{"apiGroups": [""], "resources": ["configmaps"], "resourceNames": ["a", "b"], "verbs": ["get"]}]);
        // Names it holds, yes; another name, or every name, no.
        assert!(missing(one.clone(), json!([{"apiGroups": [""], "resources": ["configmaps"], "resourceNames": ["b"], "verbs": ["get"]}])).is_empty());
        assert_eq!(
            missing(one.clone(), json!([{"apiGroups": [""], "resources": ["configmaps"], "resourceNames": ["a", "c"], "verbs": ["get"]}])),
            vec![r#"{APIGroups:[""], Resources:["configmaps"], ResourceNames:["c"], Verbs:["get"]}"#]
        );
        assert_eq!(missing(one, json!([{"apiGroups": [""], "resources": ["configmaps"], "verbs": ["get"]}])).len(), 1);
        // A rule without names holds every name.
        let all = json!([{"apiGroups": [""], "resources": ["configmaps"], "verbs": ["get"]}]);
        assert!(missing(all, json!([{"apiGroups": [""], "resources": ["configmaps"], "resourceNames": ["z"], "verbs": ["get"]}])).is_empty());
    }

    #[test]
    fn non_resource_urls() {
        let healthz = json!([{"nonResourceURLs": ["/healthz", "/logs/*"], "verbs": ["get"]}]);
        assert!(missing(healthz.clone(), json!([{"nonResourceURLs": ["/healthz", "/logs/kube.log"], "verbs": ["get"]}])).is_empty());
        assert_eq!(missing(healthz.clone(), json!([{"nonResourceURLs": ["/metrics"], "verbs": ["get"]}])).len(), 1);
        assert_eq!(missing(healthz.clone(), json!([{"nonResourceURLs": ["/healthz"], "verbs": ["post"]}])).len(), 1);
        // A resource rule never holds a URL.
        assert_eq!(missing(json!([{"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]}]),
                           json!([{"nonResourceURLs": ["/healthz"], "verbs": ["get"]}])).len(), 1);
    }

    #[test]
    fn a_rule_granting_nothing_needs_nothing() {
        assert!(missing(json!([]), json!([{"apiGroups": [], "resources": ["pods"], "verbs": ["get"]},
                                         {"apiGroups": [""], "resources": ["pods"], "verbs": []}])).is_empty());
        assert!(missing(json!([]), json!([])).is_empty());
    }

    #[test]
    fn gc_field_updates_are_not_judged() {
        let old = json!({"metadata": {"name": "r", "resourceVersion": "5"}, "rules": []});
        let gc = json!({"metadata": {"name": "r", "resourceVersion": "5", "finalizers": [],
                                      "ownerReferences": [{"uid": "x"}]}, "rules": []});
        assert!(only_gc_fields_changed(&old, &gc));
        let more = json!({"metadata": {"name": "r"}, "rules": [{"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]}]});
        assert!(!only_gc_fields_changed(&old, &more));
        let label = json!({"metadata": {"name": "r", "labels": {"a": "b"}}, "rules": []});
        assert!(!only_gc_fields_changed(&old, &label));
    }

    /// The project roles nest: a project admin may share the project as
    /// admin, edit or view, and may not hand out cluster-admin (#98).
    #[test]
    fn a_project_admin_holds_admin_edit_and_view() {
        let roles = crate::handlers::project::bootstrap_cluster_roles();
        let rules = |n: &str| roles.iter().find(|r| r["metadata"]["name"] == n).unwrap()["rules"].clone();
        for r in ["admin", "edit", "view"] {
            assert!(missing(rules("admin"), rules(r)).is_empty(), "admin does not hold {r}");
        }
        assert!(!missing(rules("edit"), rules("admin")).is_empty());
        assert!(!missing(rules("view"), rules("edit")).is_empty());
    }

    #[test]
    fn applies_to_the_four_rbac_kinds_only() {
        for r in ["roles", "clusterroles", "rolebindings", "clusterrolebindings"] {
            assert!(applies(GROUP, r, None));
        }
        assert!(!applies("", "rolebindings", None));
        assert!(!applies(GROUP, "roles", Some("status")));
        assert!(!applies("project.openshift.io", "projectrequests", None));
    }
}
