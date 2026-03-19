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
    pub api_group: String,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

/// RBAC authorization engine.
pub struct RbacEngine {
    storage: Arc<ResourceStorage>,
}

impl RbacEngine {
    pub fn new(storage: Arc<ResourceStorage>) -> Self {
        Self { storage }
    }

    /// Check if the user is authorized for the given request.
    pub async fn authorize(&self, user: &UserInfo, req: &AuthorizationRequest) -> bool {
        // system:masters group always has full access
        if user.groups.iter().any(|g| g == "system:masters") {
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
    // Check apiGroups
    let api_groups = rule["apiGroups"].as_array();
    if let Some(groups) = api_groups {
        let matched = groups.iter().any(|g| {
            let g = g.as_str().unwrap_or("");
            g == "*" || g == req.api_group
        });
        if !matched {
            return false;
        }
    }

    // Check resources
    let resources = rule["resources"].as_array();
    if let Some(res) = resources {
        let matched = res.iter().any(|r| {
            let r = r.as_str().unwrap_or("");
            r == "*" || r == req.resource
        });
        if !matched {
            return false;
        }
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
                        auth_req.resource,
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

    let (api_group, resource, namespace, name) = parse_path_segments(&segments)?;

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
        api_group,
        namespace,
        name,
    })
}

/// Parse path segments into (api_group, resource, namespace, name).
fn parse_path_segments(
    segments: &[&str],
) -> Option<(String, String, Option<String>, Option<String>)> {
    match segments {
        // /api/v1/{resource}
        ["api", "v1", resource] => Some(("".into(), resource.to_string(), None, None)),
        // /api/v1/{resource}/{name}
        ["api", "v1", resource, name] => {
            Some(("".into(), resource.to_string(), None, Some(name.to_string())))
        }
        // /api/v1/namespaces/{ns}/{resource}
        ["api", "v1", "namespaces", ns, resource] => {
            Some(("".into(), resource.to_string(), Some(ns.to_string()), None))
        }
        // /api/v1/namespaces/{ns}/{resource}/{name}
        ["api", "v1", "namespaces", ns, resource, name] => Some((
            "".into(),
            resource.to_string(),
            Some(ns.to_string()),
            Some(name.to_string()),
        )),
        // /apis/{group}/{version}/{resource}
        ["apis", group, _version, resource] => {
            Some((group.to_string(), resource.to_string(), None, None))
        }
        // /apis/{group}/{version}/{resource}/{name}
        ["apis", group, _version, resource, name] => Some((
            group.to_string(),
            resource.to_string(),
            None,
            Some(name.to_string()),
        )),
        // /apis/{group}/{version}/namespaces/{ns}/{resource}
        ["apis", group, _version, "namespaces", ns, resource] => Some((
            group.to_string(),
            resource.to_string(),
            Some(ns.to_string()),
            None,
        )),
        // /apis/{group}/{version}/namespaces/{ns}/{resource}/{name}
        ["apis", group, _version, "namespaces", ns, resource, name] => Some((
            group.to_string(),
            resource.to_string(),
            Some(ns.to_string()),
            Some(name.to_string()),
        )),
        _ => None,
    }
}
