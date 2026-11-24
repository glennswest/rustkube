//! `quota.openshift.io/v1` ClusterResourceQuotas (#391, sc#50).
//!
//! What `oc create clusterresourcequota` writes: a quota over every
//! namespace its `spec.selector` picks (namespace labels and annotations).
//! Ported from openshift-apiserver `pkg/quota` (validation, the registry
//! strategies, `appliedclusterresourcequota`, the printer) and library-go's
//! `clusterquotamapping` (read 2026-11-24).
//!
//! - **Stored** by the generic cluster handlers, with a `/status`
//!   subresource. The strategies are OpenShift's: a create starts with an
//!   empty status, a write of the object keeps the stored status, a status
//!   write keeps the stored spec ([`admit`]). Validated as
//!   `ValidateClusterResourceQuota`.
//! - **Status** is the controller-manager's `clusterresourcequota`
//!   controller's: `status.total` and one `status.namespaces[]` entry per
//!   selected namespace.
//! - **Enforced** by quota admission (`crate::quota_admission`), alongside
//!   the namespace's ResourceQuotas.
//! - **AppliedClusterResourceQuotas** ([`list_applied`], [`get_applied`])
//!   are the quotas that select a namespace, read in that namespace (`oc
//!   describe appliedclusterresourcequota`); get and list only, so a project
//!   member sees the quotas over their project without reading every
//!   ClusterResourceQuota.

use crate::error::ApiError;
use crate::storage::ResourceStorage;
use serde_json::{json, Value};

pub const RESOURCE: &str = "clusterresourcequotas";
pub const APPLIED: &str = "appliedclusterresourcequotas";
const API_VERSION: &str = "quota.openshift.io/v1";

const DNS1123_SUBDOMAIN: &str = "a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters, '-' or '.', and must start and end with an alphanumeric character (e.g. 'example.com', regex used for validation is '[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*')";
const QUALIFIED_NAME: &str = "name part must consist of alphanumeric characters, '-', '_' or '.', and must start and end with an alphanumeric character (e.g. 'MyName',  or 'my.name',  or '123-abc', regex used for validation is '([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9]')";

/// apimachinery `IsQualifiedName`: `[prefix/]name`.
fn qualified_name_reasons(key: &str) -> Vec<String> {
    let (prefix, name) = match key.split_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, key),
    };
    let mut why = Vec::new();
    if name.is_empty() {
        why.push("name part must be non-empty".to_string());
    } else {
        if name.len() > 63 {
            why.push("name part must be no more than 63 characters".to_string());
        }
        let ok = name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name.as_bytes()[name.len() - 1].is_ascii_alphanumeric();
        if !ok {
            why.push(QUALIFIED_NAME.to_string());
        }
    }
    if let Some(p) = prefix {
        if p.is_empty() {
            why.push("prefix part must be non-empty".to_string());
        } else if !crate::user_groups::is_dns_subdomain(p) {
            why.push(format!("prefix part {DNS1123_SUBDOMAIN}"));
        }
    }
    why
}

/// A label value: empty, or ≤ 63 of `[-A-Za-z0-9_.]` starting and ending
/// alphanumeric.
fn label_value_ok(v: &str) -> bool {
    v.is_empty()
        || (v.len() <= 63
            && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && v.as_bytes()[0].is_ascii_alphanumeric()
            && v.as_bytes()[v.len() - 1].is_ascii_alphanumeric())
}

/// The standard names a quota may track without a `/` (`IsStandardQuotaResourceName`).
fn standard_quota_name(k: &str) -> bool {
    matches!(
        k,
        "pods" | "services" | "replicationcontrollers" | "resourcequotas" | "secrets" | "configmaps"
            | "persistentvolumeclaims" | "services.nodeports" | "services.loadbalancers" | "cpu" | "memory"
            | "ephemeral-storage" | "requests.cpu" | "requests.memory" | "requests.storage"
            | "requests.ephemeral-storage" | "limits.cpu" | "limits.memory" | "limits.ephemeral-storage"
    ) || k.starts_with("hugepages-")
        || k.starts_with("requests.hugepages-")
}

/// Counted in whole units (`IsIntegerResourceName`).
fn integer_quota_name(k: &str) -> bool {
    matches!(
        k,
        "pods" | "services" | "replicationcontrollers" | "resourcequotas" | "secrets" | "configmaps"
            | "persistentvolumeclaims" | "services.nodeports" | "services.loadbalancers"
    ) || k.starts_with("count/")
}

const SCOPES: [&str; 6] = ["Terminating", "NotTerminating", "BestEffort", "NotBestEffort", "PriorityClass", "CrossNamespacePodAffinity"];

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `metav1` `ValidateLabelSelector`, under `path`.
fn validate_label_selector(sel: &Value, path: &str, errs: &mut Vec<String>) {
    if !sel.is_object() {
        errs.push(format!("{path}: Invalid value: {sel}: must be a label selector"));
        return;
    }
    for (k, v) in sel["matchLabels"].as_object().into_iter().flatten() {
        for r in qualified_name_reasons(k) {
            errs.push(format!("{path}.matchLabels: Invalid value: {k:?}: {r}"));
        }
        let v = s(v);
        if !label_value_ok(&v) {
            errs.push(format!("{path}.matchLabels: Invalid value: {v:?}: a valid label must be an empty string or consist of alphanumeric characters, '-', '_' or '.', and must start and end with an alphanumeric character"));
        }
    }
    for (i, e) in sel["matchExpressions"].as_array().into_iter().flatten().enumerate() {
        let p = format!("{path}.matchExpressions[{i}]");
        let op = e["operator"].as_str().unwrap_or("");
        let n = e["values"].as_array().map_or(0, Vec::len);
        match op {
            "In" | "NotIn" if n == 0 => {
                errs.push(format!("{p}.values: Required value: must be specified when `operator` is 'In' or 'NotIn'"))
            }
            "Exists" | "DoesNotExist" if n > 0 => {
                errs.push(format!("{p}.values: Forbidden: may not be specified when `operator` is 'Exists' or 'DoesNotExist'"))
            }
            "In" | "NotIn" | "Exists" | "DoesNotExist" => {}
            _ => errs.push(format!("{p}.operator: Invalid value: {op:?}: not a valid selector operator")),
        }
        let key = e["key"].as_str().unwrap_or("");
        for r in qualified_name_reasons(key) {
            errs.push(format!("{p}.key: Invalid value: {key:?}: {r}"));
        }
    }
}

/// `ValidateResourceQuotaSpec` under `spec.quota`: tracked names, their
/// amounts, and the scopes.
fn validate_quota_spec(q: &Value, errs: &mut Vec<String>) {
    for (k, v) in q["hard"].as_object().into_iter().flatten() {
        let p = format!("spec.quota.hard[{k}]");
        let why = qualified_name_reasons(k);
        if !why.is_empty() {
            for r in why {
                errs.push(format!("{p}: Invalid value: {k:?}: {r}"));
            }
        } else if !k.contains('/') && !standard_quota_name(k) {
            errs.push(format!("{p}: Invalid value: {k:?}: must be a standard resource for quota"));
        }
        let text = s(v);
        if text.trim_start().starts_with('-') {
            errs.push(format!("{p}: Invalid value: {text:?}: must be greater than or equal to 0"));
        } else if integer_quota_name(k) && apimachinery::quantity::parse_cpu_millis(&text) % 1000 != 0 {
            errs.push(format!("{p}: Invalid value: {text:?}: must be an integer"));
        }
    }
    for (i, scope) in q["scopes"].as_array().into_iter().flatten().enumerate() {
        let scope = scope.as_str().unwrap_or("");
        if !SCOPES.contains(&scope) {
            errs.push(format!("spec.quota.scopes[{i}]: Invalid value: {scope:?}: unsupported scope"));
        }
    }
    for (i, e) in q["scopeSelector"]["matchExpressions"].as_array().into_iter().flatten().enumerate() {
        let scope = e["scopeName"].as_str().unwrap_or("");
        if !SCOPES.contains(&scope) {
            errs.push(format!("spec.quota.scopeSelector.matchExpressions[{i}].scopeName: Invalid value: {scope:?}: unsupported scope"));
        }
    }
}

/// `ValidateClusterResourceQuota`.
fn validate(obj: &Value) -> Result<(), ApiError> {
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let mut errs: Vec<String> = Vec::new();
    if name.is_empty() {
        errs.push("metadata.name: Required value: name or generateName is required".into());
    } else if !crate::user_groups::is_dns_subdomain(name) {
        errs.push(format!("metadata.name: Invalid value: {name:?}: {DNS1123_SUBDOMAIN}"));
    }
    if obj["metadata"]["namespace"].as_str().is_some_and(|n| !n.is_empty()) {
        errs.push("metadata.namespace: Forbidden: not allowed on this type".into());
    }
    let sel = &obj["spec"]["selector"];
    let labels = &sel["labels"];
    let criteria = |l: &Value| l["matchLabels"].as_object().map_or(0, |m| m.len()) + l["matchExpressions"].as_array().map_or(0, Vec::len);
    let annotations = sel["annotations"].as_object();
    let has_criteria = (!labels.is_null() && criteria(labels) > 0) || annotations.is_some_and(|a| !a.is_empty());
    if !has_criteria {
        errs.push("spec.selector: Required value: must restrict the selected projects".into());
    }
    if !labels.is_null() {
        validate_label_selector(labels, "spec.selector.labels", &mut errs);
        if labels.is_object() && criteria(labels) == 0 {
            errs.push(format!("spec.selector.labels: Invalid value: {labels}: must restrict the selected projects"));
        }
    }
    match &sel["annotations"] {
        Value::Null => {}
        Value::Object(a) => {
            for k in a.keys() {
                for r in qualified_name_reasons(&k.to_lowercase()) {
                    errs.push(format!("spec.selector.annotations: Invalid value: {k:?}: {r}"));
                }
            }
        }
        other => errs.push(format!("spec.selector.annotations: Invalid value: {other}: must be a map of strings")),
    }
    validate_quota_spec(&obj["spec"]["quota"], &mut errs);
    if errs.is_empty() {
        return Ok(());
    }
    let what = if errs.len() == 1 { errs.remove(0) } else { format!("[{}]", errs.join(", ")) };
    Err(ApiError::invalid(&format!("ClusterResourceQuota.quota.openshift.io {name:?} is invalid: {what}")))
}

/// The strategies and validation of a write: a create (`existing` none)
/// starts with an empty status; a write of the object keeps the stored
/// status; a `/status` write keeps the stored spec. `selector.labels` and
/// `selector.annotations` are written `null` when absent, as OpenShift
/// serializes them.
pub fn admit(obj: &mut Value, existing: Option<&Value>, status_write: bool) -> Result<(), ApiError> {
    if !obj.is_object() {
        return Err(ApiError::bad_request("a ClusterResourceQuota must be an object"));
    }
    match existing {
        None => obj["status"] = json!({"total": {}, "namespaces": null}),
        Some(old) if status_write => obj["spec"] = old["spec"].clone(),
        Some(old) => match old.get("status") {
            Some(st) => obj["status"] = st.clone(),
            None => {
                obj.as_object_mut().expect("an object").remove("status");
            }
        },
    }
    if !obj["spec"].is_object() {
        obj["spec"] = json!({});
    }
    if !obj["spec"]["selector"].is_object() {
        obj["spec"]["selector"] = json!({});
    }
    for k in ["labels", "annotations"] {
        let sel = obj["spec"]["selector"].as_object_mut().expect("an object");
        sel.entry(k).or_insert(Value::Null);
    }
    if !obj["spec"]["quota"].is_object() {
        obj["spec"]["quota"] = json!({});
    }
    validate(obj)
}

/// A ClusterResourceQuota as the AppliedClusterResourceQuota a namespace
/// reads (OpenShift copies the object and changes the kind).
fn applied(crq: &Value) -> Value {
    let mut a = crq.clone();
    a["apiVersion"] = json!(API_VERSION);
    a["kind"] = json!("AppliedClusterResourceQuota");
    a
}

/// The ClusterResourceQuotas selecting `namespace`, sorted by name; none when
/// the namespace does not exist.
async fn selecting(storage: &ResourceStorage, namespace: &str) -> Result<Vec<Value>, ApiError> {
    let ns = match storage.get(&ResourceStorage::cluster_key("namespaces", namespace)).await {
        Ok(ns) => ns,
        Err(e) if e.status == axum::http::StatusCode::NOT_FOUND => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let (items, next, _) = storage.list(&ResourceStorage::cluster_prefix(RESOURCE), 500, token.as_deref()).await?;
        out.extend(items.into_iter().filter(|q| apimachinery::quota::selects_namespace(&q["spec"]["selector"], &ns)));
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    out.sort_by(|a, b| a["metadata"]["name"].as_str().cmp(&b["metadata"]["name"].as_str()));
    Ok(out)
}

/// LIST `namespaces/{ns}/appliedclusterresourcequotas`. A `labelSelector`
/// is applied to the quotas' own labels; a field selector or a watch is
/// refused (OpenShift serves neither).
pub async fn list_applied(storage: &ResourceStorage, namespace: &str, query: &str) -> Result<Value, ApiError> {
    let mut label_selector = None;
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "watch" if v == "true" || v == "1" => {
                return Err(ApiError {
                    status: axum::http::StatusCode::METHOD_NOT_ALLOWED,
                    reason: "MethodNotAllowed".into(),
                    message: "the server does not allow this method on the requested resource".into(),
                    continue_token: None,
                    details: None,
                })
            }
            "fieldSelector" if !v.is_empty() => {
                return Err(ApiError::bad_request("field selectors are not supported on appliedclusterresourcequotas"))
            }
            "labelSelector" if !v.is_empty() => label_selector = Some(crate::selector::parse_label_selector(&v)),
            _ => {}
        }
    }
    let items: Vec<Value> = selecting(storage, namespace)
        .await?
        .iter()
        .filter(|q| match &label_selector {
            Some(reqs) => {
                let empty = serde_json::Map::new();
                crate::selector::matches_label_selector(q["metadata"]["labels"].as_object().unwrap_or(&empty), reqs)
            }
            None => true,
        })
        .map(applied)
        .collect();
    Ok(json!({"apiVersion": API_VERSION, "kind": "AppliedClusterResourceQuotaList", "metadata": {}, "items": items}))
}

/// GET `namespaces/{ns}/appliedclusterresourcequotas/{name}`: the quota, when
/// it selects the namespace.
pub async fn get_applied(storage: &ResourceStorage, namespace: &str, name: &str) -> Result<Value, ApiError> {
    selecting(storage, namespace)
        .await?
        .iter()
        .find(|q| q["metadata"]["name"] == name)
        .map(applied)
        .ok_or_else(|| ApiError::not_found("appliedclusterresourcequota.quota.openshift.io", name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// As `oc create clusterresourcequota q --project-label-selector=team=a --hard=pods=1` sends it.
    fn oc(name: &str, labels: Value, annotations: Value, hard: Value) -> Value {
        json!({"apiVersion": API_VERSION, "kind": "ClusterResourceQuota", "metadata": {"name": name, "creationTimestamp": null},
               "spec": {"selector": {"labels": labels, "annotations": annotations}, "quota": {"hard": hard}}, "status": {"total": {}}})
    }

    fn err(mut v: Value) -> String {
        admit(&mut v, None, false).unwrap_err().message
    }

    #[test]
    fn an_oc_created_quota_is_valid_and_starts_with_no_status() {
        let mut q = oc("q", json!({"matchLabels": {"team": "a"}}), Value::Null, json!({"pods": "1"}));
        q["status"] = json!({"total": {"used": {"pods": "9"}}});
        admit(&mut q, None, false).unwrap();
        assert_eq!(q["status"], json!({"total": {}, "namespaces": null}), "a client's status is not kept");
        assert_eq!(q["spec"]["selector"]["annotations"], Value::Null);
        let mut q = json!({"metadata": {"name": "by-requester"}, "spec": {"selector": {"annotations": {"openshift.io/requester": "alice"}},
                           "quota": {"hard": {"count/deployments.apps": "3", "requests.cpu": "500m"}}}});
        admit(&mut q, None, false).unwrap();
        assert!(q["spec"]["selector"].as_object().unwrap().contains_key("labels"));
    }

    #[test]
    fn writes_keep_what_is_not_theirs() {
        let stored = json!({"metadata": {"name": "q"}, "spec": {"selector": {"labels": {"matchLabels": {"team": "a"}}, "annotations": null},
                            "quota": {"hard": {"pods": "1"}}}, "status": {"total": {"hard": {"pods": "1"}, "used": {"pods": "1"}}, "namespaces": null}});
        let mut update = stored.clone();
        update["spec"]["quota"]["hard"]["pods"] = json!("2");
        update["status"] = json!({});
        admit(&mut update, Some(&stored), false).unwrap();
        assert_eq!(update["status"], stored["status"], "an object write keeps the status");
        assert_eq!(update["spec"]["quota"]["hard"]["pods"], "2");
        let mut status = stored.clone();
        status["spec"]["quota"]["hard"]["pods"] = json!("5");
        status["status"]["total"]["used"]["pods"] = json!("0");
        admit(&mut status, Some(&stored), true).unwrap();
        assert_eq!(status["spec"], stored["spec"], "a status write keeps the spec");
        assert_eq!(status["status"]["total"]["used"]["pods"], "0");
    }

    #[test]
    fn invalid_quotas_are_refused_as_openshift_refuses_them() {
        assert_eq!(
            err(oc("q", Value::Null, Value::Null, json!({"pods": "1"}))),
            r#"ClusterResourceQuota.quota.openshift.io "q" is invalid: spec.selector: Required value: must restrict the selected projects"#
        );
        let m = err(oc("q", json!({}), Value::Null, json!({"pods": "1"})));
        assert!(m.contains("spec.selector.labels: Invalid value: {}: must restrict the selected projects"), "{m}");
        let sel = |labels: Value| err(oc("q", labels, Value::Null, json!({})));
        assert!(sel(json!({"matchExpressions": [{"key": "team", "operator": "Equals", "values": ["a"]}]}))
            .contains(r#"spec.selector.labels.matchExpressions[0].operator: Invalid value: "Equals": not a valid selector operator"#));
        assert!(sel(json!({"matchExpressions": [{"key": "team", "operator": "In"}]}))
            .contains("matchExpressions[0].values: Required value"));
        assert!(sel(json!({"matchExpressions": [{"key": "team", "operator": "Exists", "values": ["a"]}]}))
            .contains("matchExpressions[0].values: Forbidden"));
        assert!(sel(json!({"matchLabels": {"bad key!": "a"}})).contains("spec.selector.labels.matchLabels: Invalid value: \"bad key!\""));
        let hard = |h: Value| err(oc("q", json!({"matchLabels": {"team": "a"}}), Value::Null, h));
        assert!(hard(json!({"pods": "1.5"})).contains(r#"spec.quota.hard[pods]: Invalid value: "1.5": must be an integer"#));
        assert!(hard(json!({"pods": "-1"})).contains("must be greater than or equal to 0"));
        assert!(hard(json!({"widgets": "1"})).contains(r#"spec.quota.hard[widgets]: Invalid value: "widgets": must be a standard resource for quota"#));
        assert!(err(oc("Q_1", json!({"matchLabels": {"team": "a"}}), Value::Null, json!({}))).contains("a lowercase RFC 1123 subdomain"));
        let mut scoped = oc("q", json!({"matchLabels": {"team": "a"}}), Value::Null, json!({}));
        scoped["spec"]["quota"]["scopes"] = json!(["Nope"]);
        assert!(err(scoped).contains(r#"spec.quota.scopes[0]: Invalid value: "Nope": unsupported scope"#));
    }

    #[tokio::test]
    async fn a_namespace_reads_the_quotas_that_select_it() {
        let st = ResourceStorage::new(Arc::new(crate::test_store::MemStore::default()));
        for (name, labels) in [("a", json!({"team": "a"})), ("b", json!({"team": "b"}))] {
            st.create(&ResourceStorage::cluster_key("namespaces", name), json!({"metadata": {"name": name, "labels": labels}})).await.unwrap();
        }
        for (name, team) in [("qa", "a"), ("qb", "b")] {
            let mut q = oc(name, json!({"matchLabels": {"team": team}}), Value::Null, json!({"pods": "1"}));
            q["metadata"]["labels"] = json!({"tier": name});
            admit(&mut q, None, false).unwrap();
            st.create(&ResourceStorage::cluster_key(RESOURCE, name), q).await.unwrap();
        }
        let list = list_applied(&st, "a", "").await.unwrap();
        assert_eq!(list["kind"], "AppliedClusterResourceQuotaList");
        let names: Vec<&str> = list["items"].as_array().unwrap().iter().map(|q| q["metadata"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["qa"]);
        assert_eq!(list["items"][0]["kind"], "AppliedClusterResourceQuota");
        assert!(list_applied(&st, "a", "labelSelector=tier%3Dqb").await.unwrap()["items"].as_array().unwrap().is_empty());
        assert_eq!(list_applied(&st, "a", "labelSelector=tier%3Dqa").await.unwrap()["items"].as_array().unwrap().len(), 1);
        assert_eq!(get_applied(&st, "a", "qa").await.unwrap()["spec"]["quota"]["hard"]["pods"], "1");
        let e = get_applied(&st, "a", "qb").await.unwrap_err();
        assert_eq!(e.message, r#"appliedclusterresourcequota.quota.openshift.io "qb" not found"#);
        assert!(list_applied(&st, "ghost", "").await.unwrap()["items"].as_array().unwrap().is_empty());
        assert_eq!(list_applied(&st, "a", "watch=true").await.unwrap_err().status, axum::http::StatusCode::METHOD_NOT_ALLOWED);
    }
}
