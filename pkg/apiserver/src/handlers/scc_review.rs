//! The three `security.openshift.io/v1` PodSecurityPolicy reviews (#373):
//! which SCC would admit a pod template, answered by the providers pod
//! admission uses ([`crate::scc_policy`]). Namespaced, create only, as
//! openshift-apiserver serves them; `oc adm policy scc-subject-review` and
//! `scc-review` are their clients.
//!
//! - **PodSecurityPolicySubjectReview** — for `spec.user` with
//!   `spec.groups` (as given: no `system:authenticated` is added), and the
//!   template's ServiceAccount when it names one.
//! - **PodSecurityPolicySelfSubjectReview** — the same for the requester.
//! - **PodSecurityPolicyReview** — for each ServiceAccount in
//!   `spec.serviceAccountNames` (each must exist), else the template's, else
//!   `default`: every SCC that admits the template for that ServiceAccount
//!   alone.
//!
//! OpenShift's quirks are kept: a subject review whose first applicable SCC
//! refused the template still says `CantAssignSecurityContextConstraintProvider`
//! when a later one allowed it, and `status.template` carries the assigned
//! pod spec only when the template names a ServiceAccount.

use super::AppState;
use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::rbac_engine::RbacEngine;
use crate::scc_policy::{self, Provider};
use crate::storage::ResourceStorage;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use std::sync::Arc;

const API_VERSION: &str = "security.openshift.io/v1";

/// A Go `PodTemplateSpec{}` as it serializes.
fn empty_template() -> Value {
    json!({"metadata": {"creationTimestamp": null}, "spec": {"containers": null}})
}

/// The review's own metadata must be empty (its namespace aside).
fn metadata_errors(body: &Value) -> Vec<String> {
    let empty = body["metadata"]
        .as_object()
        .is_none_or(|m| m.iter().all(|(k, v)| v.is_null() || k == "namespace" || k == "managedFields"));
    if empty {
        Vec::new()
    } else {
        vec![format!("metadata: Invalid value: {}: must be empty", body["metadata"])]
    }
}

fn refuse(kind: &str, errs: Vec<String>) -> Result<(), ApiError> {
    if errs.is_empty() {
        return Ok(());
    }
    let what = if errs.len() == 1 { errs[0].clone() } else { format!("[{}]", errs.join(", ")) };
    Err(ApiError::invalid(&format!("{kind} \"\" is invalid: {what}")))
}

/// The namespace with its UID and group blocks, as providers need it.
async fn load_namespace(storage: &ResourceStorage, ns: &str) -> Result<Value, ApiError> {
    match storage.get(&ResourceStorage::cluster_key("namespaces", ns)).await {
        Ok(n) => crate::scc::namespace_with_ranges(storage, n).await,
        Err(e) if e.status == StatusCode::NOT_FOUND => Err(ApiError::internal(&format!(
            "error fetching namespace {ns:?}: namespaces {ns:?} not found"
        ))),
        Err(e) => Err(e),
    }
}

/// `FindApplicableSCCs` + `CreateProvidersFromConstraints`: the SCCs one of
/// `users` may use, sorted, as providers for `ns`.
async fn providers(
    storage: &ResourceStorage,
    ns: &Value,
    users: &[UserInfo],
    rbac: Option<&RbacEngine>,
) -> Result<Vec<Provider>, ApiError> {
    let ns_name = ns["metadata"]["name"].as_str().unwrap_or_default();
    let mut matched = Vec::new();
    for scc in scc_policy::list(storage, true).await? {
        let mut applies = users.is_empty();
        for u in users {
            if !applies && scc_policy::applies_to(&scc, u, ns_name, rbac).await {
                applies = true;
            }
        }
        if applies {
            matched.push(scc);
        }
    }
    let mut out = Vec::new();
    let mut errs = Vec::new();
    for scc in &matched {
        match Provider::new(ns, scc) {
            Ok(p) => out.push(p),
            Err(e) => errs.push(e),
        }
    }
    if !errs.is_empty() {
        return Err(ApiError::internal(&scc_policy::aggregate(&errs)));
    }
    Ok(out)
}

/// `FillPodSecurityPolicySubjectReviewStatus`: whether `provider` admits the
/// template's spec; fills `status` either way, as OpenShift does.
fn fill(status: &mut Value, provider: &Provider, template: &Value) -> bool {
    let mut pod = json!({"spec": template["spec"].clone()});
    if !provider.assign(&mut pod).is_empty() {
        status["reason"] = json!("CantAssignSecurityContextConstraintProvider");
        return false;
    }
    status["allowedBy"] = scc_policy::reference(&provider.scc);
    if template["spec"]["serviceAccountName"].as_str().is_some_and(|s| !s.is_empty()) {
        status["template"]["spec"] = pod["spec"].take();
    }
    true
}

fn template_sa(body: &Value) -> Option<String> {
    body["spec"]["template"]["spec"]["serviceAccountName"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// Answer a subject review (`self` for the requester) for `users`.
async fn subject_status(
    storage: &ResourceStorage,
    namespace: &str,
    users: Vec<UserInfo>,
    rbac: Option<&RbacEngine>,
    body: &Value,
) -> Result<Value, ApiError> {
    let ns = load_namespace(storage, namespace).await?;
    let mut status = json!({"template": empty_template()});
    for p in providers(storage, &ns, &users, rbac).await? {
        if fill(&mut status, &p, &body["spec"]["template"]) {
            break;
        }
    }
    Ok(status)
}

fn respond(mut body: Value, kind: &str, status: Value) -> Response {
    body["kind"] = json!(kind);
    body["apiVersion"] = json!(API_VERSION);
    if !body["metadata"].is_object() {
        body["metadata"] = json!({});
    }
    body["status"] = status;
    (StatusCode::CREATED, Json(body)).into_response()
}

/// `POST /apis/security.openshift.io/v1/namespaces/{ns}/podsecuritypolicysubjectreviews`
pub async fn subject_review(
    State(state): State<AppState>,
    rbac: Option<Extension<Arc<RbacEngine>>>,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    const KIND: &str = "PodSecurityPolicySubjectReview";
    let user = body["spec"]["user"].as_str().unwrap_or("").to_string();
    let groups: Vec<String> =
        body["spec"]["groups"].as_array().into_iter().flatten().filter_map(|g| g.as_str().map(str::to_string)).collect();
    let sa = template_sa(&body);
    let mut errs = Vec::new();
    if user.is_empty() && groups.is_empty() && sa.is_none() {
        errs.push("spec.user: Required value: at least one of user, groups or serviceAccountName must be specified".into());
    }
    errs.extend(metadata_errors(&body));
    refuse(KIND, errs)?;
    let mut users = Vec::new();
    if !user.is_empty() || !groups.is_empty() {
        users.push(UserInfo { username: user, groups });
    }
    if let Some(sa) = sa {
        users.push(scc_policy::service_account_user(&namespace, &sa));
    }
    let rbac = rbac.as_ref().map(|Extension(r)| r.as_ref());
    let status = subject_status(&state.storage, &namespace, users, rbac, &body).await?;
    Ok(respond(body, KIND, status))
}

/// `POST /apis/security.openshift.io/v1/namespaces/{ns}/podsecuritypolicyselfsubjectreviews`
pub async fn self_subject_review(
    State(state): State<AppState>,
    Extension(caller): Extension<UserInfo>,
    rbac: Option<Extension<Arc<RbacEngine>>>,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    const KIND: &str = "PodSecurityPolicySelfSubjectReview";
    refuse(KIND, metadata_errors(&body))?;
    let mut users = vec![caller];
    if let Some(sa) = template_sa(&body) {
        users.push(scc_policy::service_account_user(&namespace, &sa));
    }
    let rbac = rbac.as_ref().map(|Extension(r)| r.as_ref());
    let status = subject_status(&state.storage, &namespace, users, rbac, &body).await?;
    Ok(respond(body, KIND, status))
}

/// `POST /apis/security.openshift.io/v1/namespaces/{ns}/podsecuritypolicyreviews`
pub async fn review(
    State(state): State<AppState>,
    rbac: Option<Extension<Arc<RbacEngine>>>,
    Path(namespace): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    const KIND: &str = "PodSecurityPolicyReview";
    let names: Vec<Value> = body["spec"]["serviceAccountNames"].as_array().cloned().unwrap_or_default();
    let mut errs = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let n = n.as_str().unwrap_or("");
        let path = format!("spec.serviceAccountNames[{i}]");
        if n.is_empty() {
            errs.push(format!("{path}: Invalid value: \"\""));
        } else if !crate::user_groups::is_dns_subdomain(n) {
            errs.push(format!(
                "{path}: Invalid value: {n:?}: a lowercase RFC 1123 subdomain must consist of lower case alphanumeric \
                 characters, '-' or '.', and must start and end with an alphanumeric character (e.g. 'example.com', \
                 regex used for validation is '[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*')"
            ));
        }
    }
    errs.extend(metadata_errors(&body));
    refuse(KIND, errs)?;
    let storage = &state.storage;
    let wanted: Vec<String> = if names.is_empty() {
        vec![template_sa(&body).unwrap_or_else(|| "default".into())]
    } else {
        names.iter().filter_map(|n| n.as_str().map(str::to_string)).collect()
    };
    let mut missing = Vec::new();
    for sa in &wanted {
        let key = ResourceStorage::namespaced_key("serviceaccounts", &namespace, sa);
        match storage.get(&key).await {
            Ok(_) => {}
            Err(e) if e.status == StatusCode::NOT_FOUND => {
                missing.push(format!("unable to retrieve ServiceAccount {sa}: serviceaccounts {sa:?} not found"))
            }
            Err(e) => return Err(e),
        }
    }
    if !missing.is_empty() {
        return Err(ApiError::bad_request(&scc_policy::aggregate(&missing)));
    }
    let ns = load_namespace(storage, &namespace).await?;
    let rbac = rbac.as_ref().map(|Extension(r)| r.as_ref());
    let mut allowed = Vec::new();
    for sa in &wanted {
        let user = scc_policy::service_account_user(&namespace, sa);
        // A provider that cannot be made skips the ServiceAccount (the
        // shadowed `errs` in openshift-apiserver drops the error).
        let Ok(usable) = providers(storage, &ns, &[user], rbac).await else { continue };
        for p in usable {
            let mut status = json!({"template": empty_template()});
            if fill(&mut status, &p, &body["spec"]["template"]) {
                status["name"] = json!(sa);
                allowed.push(status);
            }
        }
    }
    let allowed = if allowed.is_empty() { Value::Null } else { Value::Array(allowed) };
    Ok(respond(body, KIND, json!({"allowedServiceAccounts": allowed})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_store::MemStore;

    async fn cluster() -> AppState {
        let state = AppState {
            storage: Arc::new(ResourceStorage::new(Arc::new(MemStore::default()))),
            crd_registry: Arc::new(crate::crd::CrdRegistry::new()),
            service_cidr: "10.96.0.0/12".into(),
            admission: Default::default(),
            aggregator: Arc::new(crate::aggregation::Aggregator::new(None)),
            resource_metrics: Arc::new(crate::resource_metrics::ResourceMetrics::new("http", 9096, None, None).unwrap()),
        };
        scc_policy::bootstrap(&state.storage).await;
        let ns = json!({"metadata": {"name": "work"}});
        state.storage.create(&ResourceStorage::cluster_key("namespaces", "work"), ns).await.unwrap();
        for sa in ["default", "builder"] {
            state
                .storage
                .create(&ResourceStorage::namespaced_key("serviceaccounts", "work", sa), json!({"metadata": {"name": sa, "namespace": "work"}}))
                .await
                .unwrap();
        }
        state
    }

    fn template(spec: Value) -> Value {
        json!({"metadata": {"creationTimestamp": null}, "spec": spec})
    }

    async fn body(r: Response) -> Value {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&b).unwrap()
    }

    fn user(name: &str, groups: &[&str]) -> UserInfo {
        UserInfo { username: name.into(), groups: groups.iter().map(|g| g.to_string()).collect() }
    }

    #[tokio::test]
    async fn a_self_review_names_the_scc_admission_would_choose() {
        let state = cluster().await;
        let req = json!({"kind": "PodSecurityPolicySelfSubjectReview", "apiVersion": API_VERSION, "metadata": {"creationTimestamp": null},
                         "spec": {"template": template(json!({"containers": [{"name": "a", "image": "x"}]}))}});
        let r = self_subject_review(State(state.clone()), Extension(user("alice", &["system:authenticated"])), None,
                                    Path("work".into()), Json(req.clone())).await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        let out = body(r).await;
        assert_eq!(out["kind"], "PodSecurityPolicySelfSubjectReview");
        assert_eq!(out["status"]["allowedBy"]["name"], "restricted-v2");
        assert_eq!(out["status"]["allowedBy"]["kind"], "SecurityContextConstraints");
        assert_eq!(out["status"]["allowedBy"]["apiVersion"], "security.openshift.io/v1");
        // No ServiceAccount in the template: an empty template, as OpenShift.
        assert_eq!(out["status"]["template"], empty_template());
        // An administrator: anyuid.
        let r = self_subject_review(State(state), Extension(user("system:admin", &["system:masters"])), None,
                                    Path("work".into()), Json(req)).await.unwrap();
        assert_eq!(body(r).await["status"]["allowedBy"]["name"], "anyuid");
    }

    #[tokio::test]
    async fn a_subject_review_keeps_the_reason_and_shows_the_template() {
        let state = cluster().await;
        // A root pod for an administrator user with the template's SA: anyuid
        // admits it at once.
        let req = json!({"spec": {"user": "system:admin", "groups": ["system:masters"],
                         "template": template(json!({"serviceAccountName": "builder",
                                                      "containers": [{"name": "a", "securityContext": {"privileged": true}}]}))}});
        let out = body(subject_review(State(state.clone()), None, Path("work".into()), Json(req)).await.unwrap()).await;
        assert_eq!(out["status"]["allowedBy"]["name"], "privileged");
        assert_eq!(out["status"]["reason"], "CantAssignSecurityContextConstraintProvider", "anyuid refused it first");
        assert_eq!(out["status"]["template"]["spec"]["serviceAccountName"], "builder");
        // Nobody: refused with OpenShift's field error.
        let e = subject_review(State(state.clone()), None, Path("work".into()),
                               Json(json!({"spec": {"template": template(json!({"containers": []}))}}))).await.unwrap_err();
        assert_eq!(e.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(e.message, "PodSecurityPolicySubjectReview \"\" is invalid: spec.user: Required value: at least one of user, groups or serviceAccountName must be specified");
        // A user nothing applies to: no allowedBy.
        let req = json!({"spec": {"user": "default", "template": template(json!({"containers": [{"name": "a"}]}))}});
        let out = body(subject_review(State(state), None, Path("work".into()), Json(req)).await.unwrap()).await;
        assert!(out["status"]["allowedBy"].is_null(), "{out}");
    }

    #[tokio::test]
    async fn a_review_lists_each_service_account_and_every_scc_that_admits() {
        let state = cluster().await;
        // builder is listed in a custom SCC; default only has what RBAC
        // gives a ServiceAccount (nothing without an authorizer).
        let mut custom = scc_policy::builtins().into_iter().find(|s| s["metadata"]["name"] == "anyuid").unwrap();
        custom["metadata"] = json!({"name": "builders"});
        custom["users"] = json!(["system:serviceaccount:work:builder"]);
        scc_policy::admit(&mut custom).unwrap();
        state.storage.create(&ResourceStorage::cluster_key("securitycontextconstraints", "builders"), custom).await.unwrap();
        let req = json!({"spec": {"serviceAccountNames": ["default", "builder"],
                         "template": template(json!({"containers": [{"name": "a"}]}))}});
        let out = body(review(State(state.clone()), None, Path("work".into()), Json(req)).await.unwrap()).await;
        let allowed = out["status"]["allowedServiceAccounts"].as_array().unwrap();
        assert_eq!(allowed.len(), 1, "{out}");
        assert_eq!(allowed[0]["name"], "builder");
        assert_eq!(allowed[0]["allowedBy"]["name"], "builders");
        // Nothing admits default: null, as OpenShift serializes it.
        let req = json!({"spec": {"template": template(json!({"containers": [{"name": "a"}]}))}});
        let out = body(review(State(state.clone()), None, Path("work".into()), Json(req)).await.unwrap()).await;
        assert!(out["status"]["allowedServiceAccounts"].is_null());
        // A missing ServiceAccount is a 400.
        let req = json!({"spec": {"serviceAccountNames": ["ghost"], "template": template(json!({"containers": []}))}});
        let e = review(State(state), None, Path("work".into()), Json(req)).await.unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.message, "unable to retrieve ServiceAccount ghost: serviceaccounts \"ghost\" not found");
    }
}
