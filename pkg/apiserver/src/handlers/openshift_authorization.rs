//! `authorization.openshift.io/v1` reviews (#106): what `oc adm policy
//! who-can` and `oc adm new-project`'s post-check ask, answered by the same
//! RBAC engine as the `authorization.k8s.io` reviews.
//!
//! - `SubjectAccessReview` / `LocalSubjectAccessReview` (`namespaces/{ns}/…`):
//!   may this user (`user`, `groups`; neither set: the caller) do `verb` on
//!   `resource` (`resourceAPIGroup`, `resourceName`, `namespace`), or on a
//!   non-resource `path` (`isNonResourceURL`)? Answered as
//!   `SubjectAccessReviewResponse` (`allowed`, `reason`).
//! - `ResourceAccessReview` / `LocalResourceAccessReview`: who may? Answered
//!   as `ResourceAccessReviewResponse` with `users`, `groups` and, spelled as
//!   OpenShift's API spells it, `evalutionError`.
//!
//! OpenShift's own kinds are named in its legacy (empty) API group as well
//! as their own — `oc` asks about `projects` with no group — so an empty
//! `resourceAPIGroup` for one of them is answered for both groups.
//! The endpoints are governed by ordinary RBAC (create on the review
//! resource in `authorization.openshift.io`), cluster-admin by default.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use std::sync::Arc;

const API_VERSION: &str = "authorization.openshift.io/v1";

/// The groups a legacy (empty) group stands for, for OpenShift's own kinds.
pub fn groups_for(resource: &str, group: &str) -> Vec<String> {
    if !group.is_empty() {
        return vec![group.to_string()];
    }
    let own = match resource {
        "projects" | "projectrequests" => Some("project.openshift.io"),
        "routes" | "routes/status" => Some("route.openshift.io"),
        "resourceaccessreviews" | "localresourceaccessreviews" | "subjectaccessreviews" | "localsubjectaccessreviews" => {
            Some("authorization.openshift.io")
        }
        _ => None,
    };
    let mut out = vec![String::new()];
    if let Some(g) = own {
        out.push(g.to_string());
    }
    out
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

/// The requests a review body asks about, one per group it stands for.
fn requests(body: &Value, namespace: Option<&str>) -> Vec<AuthorizationRequest> {
    let (resource, subresource) = match s(body, "resource").split_once('/') {
        Some((r, sub)) => (r.to_string(), Some(sub.to_string())),
        None => (s(body, "resource"), None),
    };
    let ns = namespace.map(str::to_string).or_else(|| Some(s(body, "namespace")).filter(|n| !n.is_empty()));
    let name = Some(s(body, "resourceName")).filter(|n| !n.is_empty());
    groups_for(&resource, &s(body, "resourceAPIGroup"))
        .into_iter()
        .map(|g| AuthorizationRequest {
            verb: s(body, "verb"),
            resource: resource.clone(),
            subresource: subresource.clone(),
            api_group: g,
            namespace: ns.clone(),
            name: name.clone(),
        })
        .collect()
}

fn mismatch(body: &Value, namespace: &str) -> Option<Response> {
    let asked = s(body, "namespace");
    (!asked.is_empty() && asked != namespace).then(|| {
        ApiError::bad_request(&format!("namespace ({asked}) must match the request namespace ({namespace})")).into_response()
    })
}

async fn sar(caller: &UserInfo, rbac: &RbacEngine, body: &Value, namespace: Option<&str>) -> Value {
    let groups: Vec<String> = body["groups"].as_array().into_iter().flatten().filter_map(|g| g.as_str().map(str::to_string)).collect();
    let who = if s(body, "user").is_empty() && groups.is_empty() {
        caller.clone()
    } else {
        UserInfo { username: s(body, "user"), groups }
    };
    let (mut allowed, mut reason) = (false, String::new());
    if body["isNonResourceURL"].as_bool() == Some(true) {
        let d = rbac.authorize_non_resource(&who, &s(body, "path"), &s(body, "verb")).await;
        (allowed, reason) = (d.allowed, d.reason);
    } else {
        for req in requests(body, namespace) {
            let d = rbac.authorize_with_reason(&who, &req).await;
            if d.allowed {
                (allowed, reason) = (true, d.reason);
                break;
            }
        }
    }
    let ns = namespace.map(str::to_string).unwrap_or_else(|| s(body, "namespace"));
    json!({"kind": "SubjectAccessReviewResponse", "apiVersion": API_VERSION, "namespace": ns, "allowed": allowed, "reason": reason})
}

async fn rar(rbac: &RbacEngine, body: &Value, namespace: Option<&str>) -> Value {
    let (mut users, mut groups, mut errors) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new(), Vec::new());
    for req in requests(body, namespace) {
        let w = rbac.who_can(&req).await;
        users.extend(w.users);
        groups.extend(w.groups);
        errors.extend(w.errors);
    }
    errors.dedup();
    let ns = namespace.map(str::to_string).unwrap_or_else(|| s(body, "namespace"));
    json!({"kind": "ResourceAccessReviewResponse", "apiVersion": API_VERSION, "namespace": ns,
           "users": users, "groups": groups, "evalutionError": errors.join("; ")})
}

/// `POST /apis/authorization.openshift.io/v1/subjectaccessreviews`
pub async fn subject_access_review(
    Extension(caller): Extension<UserInfo>,
    Extension(rbac): Extension<Arc<RbacEngine>>,
    Json(body): Json<Value>,
) -> Response {
    (axum::http::StatusCode::CREATED, Json(sar(&caller, &rbac, &body, None).await)).into_response()
}

/// `POST /apis/authorization.openshift.io/v1/namespaces/{ns}/localsubjectaccessreviews`
pub async fn local_subject_access_review(
    Extension(caller): Extension<UserInfo>,
    Extension(rbac): Extension<Arc<RbacEngine>>,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Some(r) = mismatch(&body, &namespace) {
        return r;
    }
    (axum::http::StatusCode::CREATED, Json(sar(&caller, &rbac, &body, Some(&namespace)).await)).into_response()
}

/// `POST /apis/authorization.openshift.io/v1/resourceaccessreviews`
pub async fn resource_access_review(Extension(rbac): Extension<Arc<RbacEngine>>, Json(body): Json<Value>) -> Response {
    (axum::http::StatusCode::CREATED, Json(rar(&rbac, &body, None).await)).into_response()
}

/// `POST /apis/authorization.openshift.io/v1/namespaces/{ns}/localresourceaccessreviews`
pub async fn local_resource_access_review(
    Extension(rbac): Extension<Arc<RbacEngine>>,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Some(r) = mismatch(&body, &namespace) {
        return r;
    }
    (axum::http::StatusCode::CREATED, Json(rar(&rbac, &body, Some(&namespace)).await)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_review_body_reads_as_its_requests() {
        let body = json!({"kind": "LocalResourceAccessReview", "verb": "get", "resource": "pods", "resourceAPIGroup": ""});
        let r = requests(&body, Some("work"));
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].verb.as_str(), r[0].resource.as_str(), r[0].api_group.as_str(), r[0].namespace.as_deref()),
                   ("get", "pods", "", Some("work")));
        // oc new-project's post-check: projects with no group asks both.
        let body = json!({"verb": "get", "resource": "projects", "namespace": "team", "resourceName": "team", "user": "alice"});
        let groups: Vec<String> = requests(&body, None).into_iter().map(|r| r.api_group).collect();
        assert_eq!(groups, vec!["".to_string(), "project.openshift.io".to_string()]);
        let sub = requests(&json!({"verb": "get", "resource": "pods/log"}), Some("n"));
        assert_eq!((sub[0].resource.as_str(), sub[0].subresource.as_deref()), ("pods", Some("log")));
        assert_eq!(groups_for("deployments", "apps"), vec!["apps".to_string()]);
    }
}
