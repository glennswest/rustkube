//! RBAC authorization engine.
//!
//! Evaluates whether a user is allowed to perform a specific action
//! by checking ClusterRoleBindings, RoleBindings, and their referenced roles.

use crate::auth::UserInfo;
use crate::storage::ResourceStorage;
use serde_json::Value;
use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// An authorization request describing what action is being attempted.
#[derive(Debug)]
pub struct AuthorizationRequest {
    pub verb: String,
    pub resource: String,
    /// The subresource, when the request names one — `exec` in `pods/exec`.
    ///
    /// RBAC treats `pods` and `pods/exec` as different resources, and the
    /// distinction is the difference between "can edit a workload" and "can
    /// get a shell inside it". A rule granting `pods` must not grant `exec`.
    pub subresource: Option<String>,
    pub api_group: String,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

/// RBAC authorization engine.
pub struct RbacEngine {
    storage: Arc<ResourceStorage>,
    /// Dev only: `system:anonymous` is cluster-admin.
    ///
    /// **Held here rather than only written as a ClusterRoleBinding.** The
    /// binding is still created at bootstrap so `kubectl get clusterrolebindings`
    /// shows the truth, but authorization must not *depend* on it: a stored
    /// object can be deleted by anything with permission, and the lookup that
    /// reads it fails closed when the datastore hiccups. On this test rig the
    /// grant stopped working mid-boot and every request became
    /// `system:anonymous is not allowed to ...`, which reads as a broken
    /// authorizer rather than a missing row.
    ///
    /// A flag given once at startup is a decision, not data. It belongs in the
    /// engine.
    dev_anonymous_admin: bool,
}

impl RbacEngine {
    pub fn new(storage: Arc<ResourceStorage>) -> Self {
        Self { storage, dev_anonymous_admin: false }
    }

    /// Bind `system:anonymous` to cluster-admin. Dev rigs only; see the field.
    pub fn with_anonymous_admin(mut self, on: bool) -> Self {
        self.dev_anonymous_admin = on;
        self
    }

    /// Check if the user is authorized for the given request.
    pub async fn authorize(&self, user: &UserInfo, req: &AuthorizationRequest) -> bool {
        // system:masters group always has full access
        if user.groups.iter().any(|g| g == "system:masters") {
            return true;
        }

        // The dev rig's standing grant, decided at startup rather than looked
        // up. See `dev_anonymous_admin`.
        if self.dev_anonymous_admin && user.username == "system:anonymous" {
            return true;
        }

        // Check ClusterRoleBindings
        if self.check_cluster_role_bindings(user, req).await {
            return true;
        }

        // Check namespace-scoped RoleBindings
        if let Some(ns) = &req.namespace {
            if self.check_role_bindings(user, req, ns).await {
                return true;
            }
        }

        false
    }

    async fn check_cluster_role_bindings(&self, user: &UserInfo, req: &AuthorizationRequest) -> bool {
        let prefix = ResourceStorage::cluster_prefix("clusterrolebindings");
        let (bindings, _, _) = match self.storage.list(&prefix, 1000, None).await {
            Ok(r) => r,
            Err(_) => return false,
        };

        for binding in &bindings {
            if !subjects_match(binding, user) {
                continue;
            }
            let role_name = binding["roleRef"]["name"].as_str().unwrap_or("");
            let role_kind = binding["roleRef"]["kind"].as_str().unwrap_or("");
            if role_kind == "ClusterRole" {
                if let Ok(role) = self
                    .storage
                    .get(&ResourceStorage::cluster_key("clusterroles", role_name))
                    .await
                {
                    if rules_permit(&role, req) {
                        return true;
                    }
                }
            }
        }
        false
    }

    async fn check_role_bindings(
        &self,
        user: &UserInfo,
        req: &AuthorizationRequest,
        namespace: &str,
    ) -> bool {
        let prefix = ResourceStorage::namespace_prefix("rolebindings", namespace);
        let (bindings, _, _) = match self.storage.list(&prefix, 1000, None).await {
            Ok(r) => r,
            Err(_) => return false,
        };

        for binding in &bindings {
            if !subjects_match(binding, user) {
                continue;
            }
            let role_name = binding["roleRef"]["name"].as_str().unwrap_or("");
            let role_kind = binding["roleRef"]["kind"].as_str().unwrap_or("");

            let role = match role_kind {
                "ClusterRole" => {
                    self.storage
                        .get(&ResourceStorage::cluster_key("clusterroles", role_name))
                        .await
                        .ok()
                }
                "Role" => {
                    self.storage
                        .get(&ResourceStorage::namespaced_key("roles", namespace, role_name))
                        .await
                        .ok()
                }
                _ => None,
            };

            if let Some(role) = role {
                if rules_permit(&role, req) {
                    return true;
                }
            }
        }
        false
    }
}

/// Check if any subject in a binding matches the user.
fn subjects_match(binding: &Value, user: &UserInfo) -> bool {
    let subjects = match binding["subjects"].as_array() {
        Some(s) => s,
        None => return false,
    };
    for subject in subjects {
        let kind = subject["kind"].as_str().unwrap_or("");
        let name = subject["name"].as_str().unwrap_or("");
        match kind {
            "User" if name == user.username => return true,
            "Group" if user.groups.iter().any(|g| g == name) => return true,
            "ServiceAccount" => {
                let ns = subject["namespace"].as_str().unwrap_or("default");
                let sa_user = format!("system:serviceaccount:{ns}:{name}");
                if sa_user == user.username {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Check if any rule in a role permits the request.
fn rules_permit(role: &Value, req: &AuthorizationRequest) -> bool {
    let rules = match role["rules"].as_array() {
        Some(r) => r,
        None => return false,
    };
    for rule in rules {
        if rule_matches(rule, req) {
            return true;
        }
    }
    false
}

/// Check if a single rule matches the request.
fn rule_matches(rule: &Value, req: &AuthorizationRequest) -> bool {
    let list = |field: &str| -> &[Value] {
        rule[field].as_array().map(|v| v.as_slice()).unwrap_or(&[])
    };
    let api_groups = list("apiGroups");
    let resources = list("resources");

    // A rule that names no apiGroups or no resources does not grant a resource
    // request — it is a non-resource rule (`nonResourceURLs`), or nothing.
    //
    // These two checks used to be skipped when the field was absent, on the
    // reading that an absent list means "unconstrained". It means the
    // opposite: upstream matches a request against the listed groups and
    // resources, and an empty list matches none of them. The leniency made the
    // bootstrap `system:discovery` role — whose only rule is
    // `{nonResourceURLs: [...], verbs: ["get"]}` — match *every* resource GET,
    // and `system:anonymous` is bound to it. So with `--anonymous-auth=true`,
    // which the #16 hardening was supposed to make safe on its own, anyone who
    // could reach the port could read any resource in the cluster, secrets
    // included. Absent is now empty, and empty grants nothing.
    if api_groups.is_empty() || resources.is_empty() {
        return false;
    }

    if !api_groups.iter().any(|g| {
        let g = g.as_str().unwrap_or("");
        g == "*" || g == req.api_group
    }) {
        return false;
    }

    // A subresource is matched as `resource/subresource`, which is how the
    // rule is written (`pods/exec`, `nodes/status`) and how upstream compares
    // it. `*` matches anything, and `pods/*` grants every subresource of pods
    // without granting pods itself.
    let target = match &req.subresource {
        Some(sub) => format!("{}/{}", req.resource, sub),
        None => req.resource.clone(),
    };
    if !resources.iter().any(|r| {
        let r = r.as_str().unwrap_or("");
        r == "*"
            || r == target
            || (r.ends_with("/*") && target.starts_with(&r[..r.len() - 1]))
    }) {
        return false;
    }

    // Check verbs
    let verbs = rule["verbs"].as_array();
    if let Some(vs) = verbs {
        let matched = vs.iter().any(|v| {
            let v = v.as_str().unwrap_or("");
            v == "*" || v == req.verb
        });
        if !matched {
            return false;
        }
    }

    // Check resourceNames if specified
    if let Some(names) = rule["resourceNames"].as_array() {
        if !names.is_empty() {
            if let Some(req_name) = &req.name {
                let matched = names.iter().any(|n| n.as_str() == Some(req_name.as_str()));
                if !matched {
                    return false;
                }
            }
        }
    }

    true
}

/// RBAC middleware — checks authorization after authentication.
pub async fn rbac_middleware(mut request: Request, next: Next) -> Result<Response, Response> {
    // Extract authorization info from the request path
    let path = request.uri().path().to_string();
    let method = request.method().clone();

    // Skip RBAC for health/discovery endpoints
    if path == "/healthz"
        || path == "/livez"
        || path == "/readyz"
        || path == "/version"
        || path == "/api"
        || path == "/apis"
    {
        return Ok(next.run(request).await);
    }

    // Skip RBAC for API discovery paths (GET on /api/v1, /apis/apps/v1, etc. without resource)
    if is_discovery_path(&path) && method == axum::http::Method::GET {
        return Ok(next.run(request).await);
    }

    let user = request
        .extensions()
        .get::<UserInfo>()
        .cloned()
        .unwrap_or(UserInfo {
            username: "system:anonymous".into(),
            groups: vec!["system:unauthenticated".into()],
        });

    let auth_req = parse_authorization_request(&path, &method);

    // A path under the API that this cannot parse is **denied**, not waved
    // through.
    //
    // It used to be waved through, and that was the whole authorization story
    // for every subresource: `pods/exec`, `pods/log`, `nodes/status` and every
    // CRD `/status` fell off the end of the path parser and skipped RBAC
    // entirely. With exec now served, that is a shell in any pod for anyone
    // who can reach the port. Failing closed is the only safe default for a
    // shape the authorizer does not understand — an unknown path is a reason
    // to refuse, not a reason to stop asking.
    if auth_req.is_none() && (path.starts_with("/api/") || path.starts_with("/apis/")) {
        return Err(crate::error::ApiError {
            status: StatusCode::FORBIDDEN,
            reason: "Forbidden".into(),
            message: format!("{} cannot be authorized for {path}: unrecognized API path", user.username),
        }
        .into_response());
    }

    if let Some(auth_req) = auth_req {
        if let Some(rbac) = request.extensions().get::<Arc<RbacEngine>>() {
            let rbac = rbac.clone();
            if !rbac.authorize(&user, &auth_req).await {
                let status = crate::error::ApiError {
                    status: StatusCode::FORBIDDEN,
                    reason: "Forbidden".into(),
                    message: format!(
                        "{} is not allowed to {} {} in the namespace \"{}\"",
                        user.username,
                        auth_req.verb,
                        match &auth_req.subresource {
                            Some(sub) => format!("{}/{sub}", auth_req.resource),
                            None => auth_req.resource.clone(),
                        },
                        auth_req.namespace.as_deref().unwrap_or(""),
                    ),
                };
                return Err(status.into_response());
            }
        }
    }

    // Insert user info for handlers to use
    request.extensions_mut().insert(user);
    Ok(next.run(request).await)
}

/// Check if a path is an API discovery path (no resource component).
fn is_discovery_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1"
            | "/apis/apps/v1"
            | "/apis/batch/v1"
            | "/apis/coordination.k8s.io/v1"
            | "/apis/rbac.authorization.k8s.io/v1"
            | "/apis/rustkube.io/v1alpha1"
            | "/apis/apiextensions.k8s.io/v1"
    )
}

/// Parse an authorization request from the HTTP path and method.
fn parse_authorization_request(
    path: &str,
    method: &axum::http::Method,
) -> Option<AuthorizationRequest> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    let (api_group, resource, namespace, name, subresource) = parse_path_segments(&segments)?;

    let verb = match method.as_str() {
        "GET" => {
            if name.is_some() {
                "get"
            } else {
                "list"
            }
        }
        "POST" => "create",
        "PUT" => "update",
        "PATCH" => "patch",
        "DELETE" => "delete",
        _ => return None,
    };

    Some(AuthorizationRequest {
        verb: verb.to_string(),
        resource,
        subresource,
        api_group,
        namespace,
        name,
    })
}

/// Parse path segments into (api_group, resource, namespace, name).
#[allow(clippy::type_complexity)]
fn parse_path_segments(
    segments: &[&str],
) -> Option<(
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    match segments {
        // /api/v1/{resource}
        ["api", "v1", resource] => Some(("".into(), resource.to_string(), None, None, None)),
        // /api/v1/{resource}/{name}
        ["api", "v1", resource, name] => Some((
            "".into(),
            resource.to_string(),
            None,
            Some(name.to_string()),
            None,
        )),
        // /api/v1/namespaces/{name}/{status|finalize} — the two subresources
        // of a Namespace, which have the same shape as a namespaced resource
        // list and would otherwise be read as one ("can you list finalizes in
        // namespace kube-system"). Only these two names are ambiguous, because
        // only these two exist.
        ["api", "v1", "namespaces", name, sub @ ("status" | "finalize")] => Some((
            "".into(),
            "namespaces".into(),
            None,
            Some(name.to_string()),
            Some(sub.to_string()),
        )),
        // /api/v1/namespaces/{ns}/{resource}
        ["api", "v1", "namespaces", ns, resource] => Some((
            "".into(),
            resource.to_string(),
            Some(ns.to_string()),
            None,
            None,
        )),
        // /api/v1/namespaces/{ns}/{resource}/{name}
        ["api", "v1", "namespaces", ns, resource, name] => Some((
            "".into(),
            resource.to_string(),
            Some(ns.to_string()),
            Some(name.to_string()),
            None,
        )),
        // /api/v1/namespaces/{ns}/{resource}/{name}/{subresource}
        ["api", "v1", "namespaces", ns, resource, name, sub] => Some((
            "".into(),
            resource.to_string(),
            Some(ns.to_string()),
            Some(name.to_string()),
            Some(sub.to_string()),
        )),
        // /api/v1/{resource}/{name}/{subresource} — cluster-scoped, e.g.
        // `nodes/node-a/status`. **After** the namespaces arms above: this has
        // the same five-segment shape as `namespaces/{ns}/{resource}`, and
        // whichever is written first wins, so a listing of pods in a namespace
        // would otherwise be read as a subresource of a namespace.
        ["api", "v1", resource, name, sub] => Some((
            "".into(),
            resource.to_string(),
            None,
            Some(name.to_string()),
            Some(sub.to_string()),
        )),
        // /apis/{group}/{version}/{resource}
        ["apis", group, _version, resource] => Some((
            group.to_string(),
            resource.to_string(),
            None,
            None,
            None,
        )),
        // /apis/{group}/{version}/{resource}/{name}
        ["apis", group, _version, resource, name] => Some((
            group.to_string(),
            resource.to_string(),
            None,
            Some(name.to_string()),
            None,
        )),
        // /apis/{group}/{version}/{resource}/{name}/{subresource}
        ["apis", group, _version, resource, name, sub] => Some((
            group.to_string(),
            resource.to_string(),
            None,
            Some(name.to_string()),
            Some(sub.to_string()),
        )),
        // /apis/{group}/{version}/namespaces/{ns}/{resource}
        ["apis", group, _version, "namespaces", ns, resource] => Some((
            group.to_string(),
            resource.to_string(),
            Some(ns.to_string()),
            None,
            None,
        )),
        // /apis/{group}/{version}/namespaces/{ns}/{resource}/{name}
        ["apis", group, _version, "namespaces", ns, resource, name] => Some((
            group.to_string(),
            resource.to_string(),
            Some(ns.to_string()),
            Some(name.to_string()),
            None,
        )),
        // /apis/{group}/{version}/namespaces/{ns}/{resource}/{name}/{subresource}
        ["apis", group, _version, "namespaces", ns, resource, name, sub] => Some((
            group.to_string(),
            resource.to_string(),
            Some(ns.to_string()),
            Some(name.to_string()),
            Some(sub.to_string()),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod subresource_tests {
    use super::*;
    use serde_json::json;

    fn req(verb: &str, resource: &str, subresource: Option<&str>) -> AuthorizationRequest {
        AuthorizationRequest {
            verb: verb.into(),
            resource: resource.into(),
            subresource: subresource.map(str::to_string),
            api_group: "".into(),
            namespace: Some("default".into()),
            name: Some("p1".into()),
        }
    }

    #[test]
    #[test]
    fn a_non_resource_rule_grants_no_resource() {
        // The real bootstrap `system:discovery` role, which `system:anonymous`
        // is bound to. Its only rule lists nonResourceURLs, so it must not
        // grant a resource GET — this used to return true for every resource
        // in the cluster, secrets included.
        let role = json!({"rules": [{
            "nonResourceURLs": ["/api", "/apis", "/api/*", "/apis/*", "/healthz", "/version"],
            "verbs": ["get"]
        }]});
        for resource in ["secrets", "pods", "nodes"] {
            let r = AuthorizationRequest {
                verb: "get".into(),
                resource: resource.into(),
                subresource: None,
                api_group: "".into(),
                namespace: Some("kube-system".into()),
                name: None,
            };
            assert!(!rules_permit(&role, &r), "{resource} was granted by a non-resource rule");
        }
    }

    #[test]
    fn an_empty_resource_list_grants_nothing() {
        // Absent and empty are the same thing, and neither is a wildcard.
        let role = json!({"rules": [{"apiGroups": ["*"], "resources": [], "verbs": ["*"]}]});
        assert!(!rules_permit(&role, &req("get", "pods", None)));
        let role = json!({"rules": [{"resources": ["*"], "verbs": ["*"]}]});
        assert!(!rules_permit(&role, &req("get", "pods", None)));
    }

    #[test]
    fn an_explicit_wildcard_still_grants_everything() {
        // cluster-admin is written with real "*" entries and must be unaffected.
        let role = json!({"rules": [{"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]}]});
        assert!(rules_permit(&role, &req("get", "secrets", None)));
        assert!(rules_permit(&role, &req("create", "pods", Some("exec"))));
    }

    #[test]
    fn a_rule_for_pods_does_not_grant_exec() {
        // The distinction that matters: "can edit a workload" is not "can get
        // a shell inside it".
        let rule = json!({"apiGroups": [""], "resources": ["pods"], "verbs": ["*"]});
        assert!(rule_matches(&rule, &req("get", "pods", None)));
        assert!(!rule_matches(&rule, &req("create", "pods", Some("exec"))));
    }

    #[test]
    fn a_rule_naming_the_subresource_grants_it() {
        let rule = json!({"apiGroups": [""], "resources": ["pods/exec"], "verbs": ["create"]});
        assert!(rule_matches(&rule, &req("create", "pods", Some("exec"))));
        assert!(!rule_matches(&rule, &req("create", "pods", Some("attach"))));
        assert!(!rule_matches(&rule, &req("get", "pods", None)));
    }

    #[test]
    fn wildcards_still_grant_everything() {
        let rule = json!({"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]});
        assert!(rule_matches(&rule, &req("create", "pods", Some("exec"))));
        let subs = json!({"apiGroups": [""], "resources": ["pods/*"], "verbs": ["*"]});
        assert!(rule_matches(&subs, &req("create", "pods", Some("exec"))));
        assert!(rule_matches(&subs, &req("get", "pods", Some("log"))));
        assert!(
            !rule_matches(&subs, &req("delete", "pods", None)),
            "pods/* is every subresource of pods, not pods itself"
        );
    }

    #[test]
    fn subresource_paths_are_parsed_rather_than_falling_off_the_end() {
        let exec = parse_authorization_request(
            "/api/v1/namespaces/kube-system/pods/cilium-abc/exec",
            &axum::http::Method::POST,
        )
        .expect("exec path must parse — an unparsed path used to skip RBAC entirely");
        assert_eq!(exec.resource, "pods");
        assert_eq!(exec.subresource.as_deref(), Some("exec"));
        assert_eq!(exec.verb, "create");
        assert_eq!(exec.namespace.as_deref(), Some("kube-system"));

        let status = parse_authorization_request(
            "/api/v1/nodes/node-a/status",
            &axum::http::Method::PUT,
        )
        .expect("cluster-scoped subresource must parse");
        assert_eq!(status.resource, "nodes");
        assert_eq!(status.subresource.as_deref(), Some("status"));
        assert_eq!(status.verb, "update");

        let crd = parse_authorization_request(
            "/apis/cilium.io/v2/namespaces/default/ciliumnetworkpolicies/p/status",
            &axum::http::Method::PATCH,
        )
        .expect("grouped namespaced subresource must parse");
        assert_eq!(crd.api_group, "cilium.io");
        assert_eq!(crd.resource, "ciliumnetworkpolicies");
        assert_eq!(crd.subresource.as_deref(), Some("status"));
    }

    #[test]
    fn a_websocket_exec_is_a_get_on_the_same_subresource() {
        // Newer clients open exec with GET (WebSocket) rather than POST
        // (SPDY); both must land on pods/exec.
        let ws = parse_authorization_request(
            "/api/v1/namespaces/default/pods/p1/exec",
            &axum::http::Method::GET,
        )
        .unwrap();
        assert_eq!(ws.subresource.as_deref(), Some("exec"));
        assert_eq!(ws.verb, "get");
    }
}

#[cfg(test)]
mod namespace_subresource_tests {
    use super::*;

    #[test]
    fn a_namespace_subresource_is_not_a_resource_list() {
        // `/api/v1/namespaces/kube-system/finalize` has the same shape as
        // `/api/v1/namespaces/kube-system/pods`, and reading it as the latter
        // asks whether the caller may list "finalizes".
        let fin = parse_authorization_request(
            "/api/v1/namespaces/kube-system/finalize",
            &axum::http::Method::PUT,
        )
        .unwrap();
        assert_eq!(fin.resource, "namespaces");
        assert_eq!(fin.subresource.as_deref(), Some("finalize"));
        assert_eq!(fin.name.as_deref(), Some("kube-system"));

        let pods = parse_authorization_request(
            "/api/v1/namespaces/kube-system/pods",
            &axum::http::Method::GET,
        )
        .unwrap();
        assert_eq!(pods.resource, "pods");
        assert_eq!(pods.subresource, None);
        assert_eq!(pods.namespace.as_deref(), Some("kube-system"));
    }
}
