//! `authorization.k8s.io/v1` — asking the apiserver what an identity may do.
//!
//! The RBAC engine already decides this on every request; without these
//! endpoints there is no way to ask it without *performing* the action. A
//! console then has to infer access by probing — listing pods in every
//! namespace, one request each, as the viewer, and reading the status code —
//! which is 200 requests per viewer on a 200-namespace cluster and answers a
//! question next to the one that was asked (#59).
//!
//! Both are write-only virtual resources: nothing is stored, the authorizer is
//! consulted, and the answer comes back in `status`. The identity is the
//! caller's own, so no impersonation is involved and every authenticated user
//! may ask about themselves — which is why upstream binds `system:basic-user`
//! to `system:authenticated` and why this apiserver now does too.
//!
//! `SubjectAccessReview` — asking about *another* identity — is the privileged
//! variant and is deliberately not here.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::handlers::AppState;
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use std::sync::Arc;

/// `POST /apis/authorization.k8s.io/v1/selfsubjectaccessreviews`
///
/// "May I do X on Y?" — one question, one answer, from the same code path that
/// would have answered the real request.
pub async fn create_self_subject_access_review(
    Extension(user): Extension<UserInfo>,
    Extension(rbac): Extension<Arc<RbacEngine>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let spec = &body["spec"];
    let status = if let Some(attrs) = spec.get("resourceAttributes").filter(|v| !v.is_null()) {
        let req = AuthorizationRequest {
            verb: str_at(attrs, "verb"),
            resource: str_at(attrs, "resource"),
            subresource: opt_str_at(attrs, "subresource"),
            api_group: str_at(attrs, "group"),
            namespace: opt_str_at(attrs, "namespace"),
            name: opt_str_at(attrs, "name"),
        };
        let d = rbac.authorize_with_reason(&user, &req).await;
        json!({ "allowed": d.allowed, "denied": !d.allowed, "reason": d.reason })
    } else if let Some(attrs) = spec.get("nonResourceAttributes").filter(|v| !v.is_null()) {
        let path = str_at(attrs, "path");
        let verb = str_at(attrs, "verb");
        let d = rbac.authorize_non_resource(&user, &path, &verb).await;
        json!({ "allowed": d.allowed, "denied": !d.allowed, "reason": d.reason })
    } else {
        // Neither attribute set. Upstream treats this as an invalid spec and
        // answers with an evaluationError rather than a bare "no", because a
        // console that reads only `allowed` would otherwise hide a button for
        // a reason that is its own bug.
        json!({
            "allowed": false,
            "evaluationError": "spec must set resourceAttributes or nonResourceAttributes",
        })
    };

    Json(json!({
        "kind": "SelfSubjectAccessReview",
        "apiVersion": "authorization.k8s.io/v1",
        "metadata": { "creationTimestamp": null },
        // The spec is echoed back, as upstream does: a client that pipelines
        // several of these needs to tell the answers apart.
        "spec": spec.clone(),
        "status": status,
    }))
}

/// `POST /apis/authorization.k8s.io/v1/selfsubjectrulesreviews`
///
/// "What may I do in namespace N?" — one request instead of one per question,
/// which is what makes a console's namespace list affordable.
pub async fn create_self_subject_rules_review(
    Extension(user): Extension<UserInfo>,
    Extension(rbac): Extension<Arc<RbacEngine>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let namespace = opt_str_at(&body["spec"], "namespace");
    let set = rbac.rules_for(&user, namespace.as_deref()).await;

    Json(json!({
        "kind": "SelfSubjectRulesReview",
        "apiVersion": "authorization.k8s.io/v1",
        "metadata": { "creationTimestamp": null },
        "spec": body["spec"].clone(),
        "status": {
            "resourceRules": set.resource_rules,
            "nonResourceRules": set.non_resource_rules,
            "incomplete": set.incomplete,
        },
    }))
}

/// `POST /apis/authorization.k8s.io/v1/subjectaccessreviews`
///
/// "May *they* do X on Y?" — the privileged sibling of the self-review.
///
/// Deferred when the self-reviews landed (#59) because it is a different kind
/// of question: a self-review reveals nothing the asker could not learn by
/// trying the action, while this one reports on somebody else and so is
/// governed by ordinary RBAC on `subjectaccessreviews` — which no bootstrap
/// role grants except cluster-admin.
///
/// `oc adm policy who-can` is what asks it, and every admission webhook that
/// needs to know whether the requesting user may do a thing (#69).
pub async fn create_subject_access_review(
    State(state): State<AppState>,
    Extension(rbac): Extension<Arc<RbacEngine>>,
    Json(body): Json<Value>,
) -> Response {
    let _ = &state;
    let spec = &body["spec"];
    let status = decide(&rbac, &subject_of(spec), spec, None).await;
    review("SubjectAccessReview", spec, status)
}

/// `POST /apis/authorization.k8s.io/v1/namespaces/{ns}/localsubjectaccessreviews`
///
/// The same question, scoped to one namespace, so it can be granted to
/// somebody who administers that namespace and nothing else — which is the
/// whole reason it exists separately.
pub async fn create_local_subject_access_review(
    State(state): State<AppState>,
    Extension(rbac): Extension<Arc<RbacEngine>>,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let _ = &state;
    let spec = &body["spec"];
    // Upstream refuses a mismatch rather than honouring either one. A request
    // that asked about namespace A through namespace B's endpoint is a
    // confused caller, and answering it for A would let anyone who may ask
    // about B ask about the whole cluster.
    if let Some(attrs) = spec.get("resourceAttributes").filter(|v| !v.is_null()) {
        let asked = attrs["namespace"].as_str().unwrap_or("");
        if !asked.is_empty() && asked != namespace {
            return ApiError {
                status: StatusCode::BAD_REQUEST,
                reason: "BadRequest".into(),
                message: format!(
                    "spec.resourceAttributes.namespace ({asked}) must match the request namespace ({namespace})"
                ),
            }
            .into_response();
        }
    }
    let status = decide(&rbac, &subject_of(spec), spec, Some(&namespace)).await;
    review("LocalSubjectAccessReview", spec, status)
}

/// The identity a SubjectAccessReview is asking about.
///
/// Not the caller's — that is the whole difference from a self-review.
fn subject_of(spec: &Value) -> UserInfo {
    UserInfo {
        username: spec["user"].as_str().unwrap_or("").to_string(),
        groups: spec["groups"]
            .as_array()
            .map(|g| g.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
    }
}

/// Answer one review's spec for one identity.
///
/// `namespace` overrides what the attributes say, for the Local variant where
/// the path is authoritative.
async fn decide(
    rbac: &Arc<RbacEngine>,
    who: &UserInfo,
    spec: &Value,
    namespace: Option<&str>,
) -> Value {
    if let Some(attrs) = spec.get("resourceAttributes").filter(|v| !v.is_null()) {
        let req = AuthorizationRequest {
            verb: str_at(attrs, "verb"),
            resource: str_at(attrs, "resource"),
            subresource: opt_str_at(attrs, "subresource"),
            api_group: str_at(attrs, "group"),
            namespace: namespace.map(str::to_string).or_else(|| opt_str_at(attrs, "namespace")),
            name: opt_str_at(attrs, "name"),
        };
        let d = rbac.authorize_with_reason(who, &req).await;
        json!({ "allowed": d.allowed, "denied": !d.allowed, "reason": d.reason })
    } else if let Some(attrs) = spec.get("nonResourceAttributes").filter(|v| !v.is_null()) {
        let d = rbac
            .authorize_non_resource(who, &str_at(attrs, "path"), &str_at(attrs, "verb"))
            .await;
        json!({ "allowed": d.allowed, "denied": !d.allowed, "reason": d.reason })
    } else {
        json!({
            "allowed": false,
            "evaluationError": "spec must set resourceAttributes or nonResourceAttributes",
        })
    }
}

/// The envelope both reviews answer in.
fn review(kind: &str, spec: &Value, status: Value) -> Response {
    axum::Json(json!({
        "kind": kind,
        "apiVersion": "authorization.k8s.io/v1",
        "metadata": { "creationTimestamp": null },
        "spec": spec.clone(),
        "status": status,
    }))
    .into_response()
}

fn str_at(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

/// An absent field and an empty one are the same thing here: upstream's
/// attributes are non-optional strings that default to "", and a `namespace`
/// of "" means cluster-scoped, not "namespace named empty".
fn opt_str_at(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subject_is_the_one_asked_about_not_the_caller() {
        // The whole difference from a self-review: this reports on somebody
        // else, which is why ordinary RBAC governs it.
        let who = subject_of(&json!({"user": "alice", "groups": ["dev", "ops"]}));
        assert_eq!(who.username, "alice");
        assert_eq!(who.groups, vec!["dev".to_string(), "ops".to_string()]);
    }

    #[test]
    fn a_review_about_groups_alone_is_an_identity_too() {
        // "May anyone in ops do this" is a legitimate question, and the empty
        // username must not be read as the caller's.
        let who = subject_of(&json!({"groups": ["ops"]}));
        assert_eq!(who.username, "");
        assert_eq!(who.groups, vec!["ops".to_string()]);
        let none = subject_of(&json!({}));
        assert_eq!(none.username, "");
        assert!(none.groups.is_empty());
    }

    #[test]
    fn attributes_are_read_the_way_upstream_writes_them() {
        // `group` is the API group and `resource` the plural — the spelling
        // trips people because the field is not called apiGroup here.
        let attrs = json!({
            "namespace": "team-a", "verb": "list", "group": "apps",
            "resource": "deployments", "subresource": "", "name": ""
        });
        assert_eq!(str_at(&attrs, "group"), "apps");
        assert_eq!(str_at(&attrs, "resource"), "deployments");
        // Empty is absent, not a resource named "".
        assert_eq!(opt_str_at(&attrs, "subresource"), None);
        assert_eq!(opt_str_at(&attrs, "name"), None);
        assert_eq!(opt_str_at(&attrs, "namespace").as_deref(), Some("team-a"));
    }
}
