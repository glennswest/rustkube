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
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
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

fn str_at(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

/// An absent field and an empty one are the same thing here: upstream's
/// attributes are non-optional strings that default to "", and a `namespace`
/// of "" means cluster-scoped, not "namespace named empty".
fn opt_str_at(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}
