//! ResourceQuota admission (#124), as upstream's plugin: a create in a
//! namespace is checked against every ResourceQuota there whose scopes
//! cover it, and charged to them before it is written.
//!
//! - **Refused (403)** when it would take a tracked resource past `hard`:
//!   `exceeded quota: <q>, requested: <r>=<n>, used: <r>=<n>, limited:
//!   <r>=<n>`; when a Pod sets no value for a compute resource the quota
//!   tracks: `failed quota: <q>: must specify <r> for: <containers>`; and
//!   while the quota controller has not yet computed the quota's status:
//!   `status unknown for quota: <q>`.
//! - **Charged** by adding the object's usage to the quota's `status.used`
//!   with a compare-and-swap on its resourceVersion, so two concurrent
//!   creates cannot both take the last of it. Every quota is checked before
//!   any is charged; a charge that loses its race is re-checked; a refusal
//!   after an earlier quota was charged gives that charge back.
//! - The quota is marked `quota.rustkube.io/charged-at`, and the quota
//!   controller waits that long (5 s) before lowering a usage it computed
//!   from objects the charged create may not have written yet. A charge for
//!   a create that then fails (a webhook, a taken name) is corrected by the
//!   controller's next pass, at the latest its 5-minute resync.
//!
//! Usage and scopes are `apimachinery::quota`'s, shared with the controller.
//!
//! **ClusterResourceQuotas** (#391) are checked and charged in the same
//! plan, as OpenShift's `quota.openshift.io/ClusterResourceQuota` plugin
//! runs the same evaluator: each one whose selector picks the namespace is
//! the ResourceQuota `{spec: spec.quota, status: status.total}` there, and a
//! charge adds to both `status.total.used` and the namespace's entry in
//! `status.namespaces`. Their refusals read as a ResourceQuota's.

use crate::error::ApiError;
use crate::storage::ResourceStorage;
use apimachinery::quota::{self, Usage};
use serde_json::{json, Value};

pub const CHARGED_AT: &str = "quota.rustkube.io/charged-at";
const COMPUTE: [&str; 9] = [
    "cpu", "memory", "ephemeral-storage", "requests.cpu", "requests.memory", "requests.ephemeral-storage",
    "limits.cpu", "limits.memory", "limits.ephemeral-storage",
];

fn list(u: &Usage, keys: &[&String]) -> String {
    keys.iter().map(|k| format!("{k}={}", quota::format(k, u.get(*k).copied().unwrap_or(0)))).collect::<Vec<_>>().join(",")
}

fn used_of(q: &Value) -> Usage {
    q["status"]["used"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), quota::parse(k, v))).collect()
}

fn hard_of(q: &Value) -> Usage {
    q["status"]["hard"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), quota::parse(k, v))).collect()
}

/// Why `q` refuses `delta` for `obj`, if it does.
fn refusal(q: &Value, resource: &str, obj: &Value, delta: &Usage) -> Option<String> {
    let qname = q["metadata"]["name"].as_str().unwrap_or("");
    if resource == "pods" {
        let mut why = Vec::new();
        for k in q["spec"]["hard"].as_object().into_iter().flatten().map(|(k, _)| k.as_str()).filter(|k| COMPUTE.contains(k)) {
            let missing = quota::missing(obj, k);
            if !missing.is_empty() {
                why.push(format!("{k} for: {}", missing.join(",")));
            }
        }
        if !why.is_empty() {
            return Some(format!("failed quota: {qname}: must specify {}", why.join("; ")));
        }
    }
    if !q["status"]["hard"].is_object() {
        return Some(format!("status unknown for quota: {qname}, resource: {}", delta.keys().next().map(String::as_str).unwrap_or("")));
    }
    let (hard, used) = (hard_of(q), used_of(q));
    let over: Vec<&String> = delta
        .iter()
        .filter(|(k, n)| hard.get(*k).is_some_and(|h| used.get(*k).copied().unwrap_or(0) + **n > *h))
        .map(|(k, _)| k)
        .collect();
    (!over.is_empty()).then(|| {
        format!("exceeded quota: {qname}, requested: {}, used: {}, limited: {}", list(delta, &over), list(&used, &over), list(&hard, &over))
    })
}

fn rev(v: &Value) -> Option<u64> {
    v["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok())
}

/// Which kind of quota a charge goes to.
#[derive(Clone)]
enum Kind {
    /// A ResourceQuota in the namespace.
    Namespaced,
    /// A ClusterResourceQuota selecting the namespace (#391).
    Cluster { namespace: String },
}

impl Kind {
    /// The quota as a ResourceQuota (`spec.hard`, `status.hard`/`used`).
    fn view(&self, stored: &Value) -> Value {
        match self {
            Kind::Namespaced => stored.clone(),
            Kind::Cluster { .. } => quota::cluster_quota_view(stored),
        }
    }
}

fn add(used: &mut Usage, delta: &Usage, sign: i128) {
    for (k, n) in delta {
        let e = used.entry(k.clone()).or_default();
        *e = (*e + sign * n).max(0);
    }
}

/// Add `delta` (negative to give it back) to the quota at `key`'s used, by
/// compare-and-swap; `Err` carries a refusal found on a re-read.
async fn charge(
    storage: &ResourceStorage,
    key: &str,
    kind: &Kind,
    resource: &str,
    obj: &Value,
    delta: &Usage,
    sign: i128,
) -> Result<(), ApiError> {
    for _ in 0..10 {
        let mut q = storage.get(key).await?;
        let view = kind.view(&q);
        if sign > 0 {
            if let Some(why) = refusal(&view, resource, obj, delta) {
                return Err(ApiError::forbidden(&why));
            }
        }
        let mut used = used_of(&view);
        add(&mut used, delta, sign);
        let Some(hard) = view["spec"]["hard"].as_object().cloned() else { return Ok(()) };
        match kind {
            Kind::Namespaced => q["status"]["used"] = quota::resource_list(&hard, &used),
            Kind::Cluster { namespace } => {
                q["status"]["total"]["used"] = quota::resource_list(&hard, &used);
                charge_namespace(&mut q, namespace, &hard, delta, sign);
            }
        }
        if !q["metadata"]["annotations"].is_object() {
            q["metadata"]["annotations"] = json!({});
        }
        q["metadata"]["annotations"][CHARGED_AT] = json!(chrono::Utc::now().to_rfc3339());
        let r = rev(&q);
        match storage.update(key, q, r).await {
            Ok(_) => return Ok(()),
            Err(e) if e.reason == "Conflict" => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::conflict("quota charge kept losing its race; try again"))
}

/// A ClusterResourceQuota's `status.namespaces[namespace]` with `delta`
/// added, the entry made (with the quota's hard) when there is none.
fn charge_namespace(q: &mut Value, namespace: &str, hard: &serde_json::Map<String, Value>, delta: &Usage, sign: i128) {
    if !q["status"]["namespaces"].is_array() {
        q["status"]["namespaces"] = json!([]);
    }
    let list = q["status"]["namespaces"].as_array_mut().expect("an array");
    let i = match list.iter().position(|e| e["namespace"] == namespace) {
        Some(i) => i,
        None => {
            list.push(json!({"namespace": namespace, "status": {"hard": hard, "used": {}}}));
            list.len() - 1
        }
    };
    let mut used = used_of(&list[i]);
    add(&mut used, delta, sign);
    list[i]["status"]["used"] = quota::resource_list(hard, &used);
}

/// The stored objects under `prefix`, from the watch cache when it answers.
async fn list_cached(storage: &ResourceStorage, prefix: &str) -> Vec<Value> {
    if let Ok((_, items)) = storage.watch_cache().snapshot(prefix).await {
        return items.iter().filter_map(|(_, b)| serde_json::from_slice(b).ok()).collect();
    }
    storage.list(prefix, 500, None).await.map(|(items, _, _)| items).unwrap_or_default()
}

/// The ClusterResourceQuotas whose selectors pick `namespace` (#391).
async fn cluster_quotas(storage: &ResourceStorage, namespace: &str) -> Vec<Value> {
    let crqs = list_cached(storage, &ResourceStorage::cluster_prefix(crate::cluster_quota::RESOURCE)).await;
    if crqs.is_empty() {
        return crqs;
    }
    let Ok(ns) = storage.get(&ResourceStorage::cluster_key("namespaces", namespace)).await else {
        return Vec::new();
    };
    crqs.into_iter().filter(|q| quota::selects_namespace(&q["spec"]["selector"], &ns)).collect()
}

/// Check a create of `obj` (`resource` in the core or a built-in group)
/// in `namespace` against its quotas, and charge them.
pub async fn admit(storage: &ResourceStorage, resource: &str, namespace: &str, obj: &Value) -> Result<(), ApiError> {
    let mut quotas: Vec<(Value, String, Kind)> = Vec::new();
    if let Ok((rqs, _, _)) = storage.list(&ResourceStorage::namespace_prefix("resourcequotas", namespace), 500, None).await {
        for q in rqs {
            let qname = q["metadata"]["name"].as_str().unwrap_or("").to_string();
            quotas.push((q, ResourceStorage::namespaced_key("resourcequotas", namespace, &qname), Kind::Namespaced));
        }
    }
    for crq in cluster_quotas(storage, namespace).await {
        let qname = crq["metadata"]["name"].as_str().unwrap_or("").to_string();
        let kind = Kind::Cluster { namespace: namespace.to_string() };
        quotas.push((kind.view(&crq), ResourceStorage::cluster_key(crate::cluster_quota::RESOURCE, &qname), kind));
    }
    if quotas.is_empty() {
        return Ok(());
    }
    let api_version = crate::handlers::resource::resource_to_api_version(resource);
    let group = api_version.split_once('/').map(|(g, _)| g).unwrap_or("");
    let usage = quota::usage(resource, group, obj);
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let forbidden = |why: &str| ApiError::forbidden(&format!("{resource} \"{name}\" is forbidden: {why}"));
    let mut plan = Vec::new();
    for (q, key, kind) in &quotas {
        if !quota::matches(q, resource, group, obj) {
            continue;
        }
        let delta = quota::tracked(q, &usage);
        let tracks_compute = resource == "pods"
            && q["spec"]["hard"].as_object().is_some_and(|h| h.keys().any(|k| COMPUTE.contains(&k.as_str())));
        if delta.is_empty() && !tracks_compute {
            continue;
        }
        if let Some(why) = refusal(q, resource, obj, &delta) {
            return Err(forbidden(&why));
        }
        plan.push((key.clone(), kind.clone(), delta));
    }
    let mut done: Vec<&(String, Kind, Usage)> = Vec::new();
    for item in &plan {
        if let Err(e) = charge(storage, &item.0, &item.1, resource, obj, &item.2, 1).await {
            for (key, kind, delta) in &done {
                let _ = charge(storage, key, kind, resource, obj, delta, -1).await;
            }
            return Err(if e.status == axum::http::StatusCode::FORBIDDEN { forbidden(&e.message) } else { e });
        }
        done.push(item);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(hard: Value, used: Value) -> Value {
        json!({"metadata": {"name": "q"}, "spec": {"hard": hard}, "status": {"hard": hard, "used": used}})
    }

    /// #391: a ClusterResourceQuota selecting the namespace is checked and
    /// charged with its ResourceQuotas; one that does not select it is not.
    #[tokio::test]
    async fn a_cluster_quota_is_charged_in_the_namespaces_it_selects() {
        use std::sync::Arc;
        let st = ResourceStorage::new(Arc::new(crate::test_store::MemStore::default()));
        for (ns, team) in [("a1", "a"), ("b1", "b")] {
            st.create(&ResourceStorage::cluster_key("namespaces", ns),
                      json!({"metadata": {"name": ns, "labels": {"team": team}}})).await.unwrap();
        }
        let crq = json!({"metadata": {"name": "q"},
            "spec": {"selector": {"labels": {"matchLabels": {"team": "a"}}, "annotations": null}, "quota": {"hard": {"pods": "1"}}},
            "status": {"total": {"hard": {"pods": "1"}, "used": {"pods": "0"}}, "namespaces": []}});
        let key = ResourceStorage::cluster_key(crate::cluster_quota::RESOURCE, "q");
        st.create(&key, crq).await.unwrap();
        let pod = |n: &str| json!({"metadata": {"name": n}, "spec": {"containers": [{"name": "c"}]}});
        admit(&st, "pods", "a1", &pod("p1")).await.unwrap();
        let q = st.get(&key).await.unwrap();
        assert_eq!(q["status"]["total"]["used"], json!({"pods": "1"}));
        assert_eq!(q["status"]["namespaces"], json!([{"namespace": "a1", "status": {"hard": {"pods": "1"}, "used": {"pods": "1"}}}]));
        assert!(q["metadata"]["annotations"][CHARGED_AT].is_string());
        let e = admit(&st, "pods", "a1", &pod("p2")).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(e.message, "pods \"p2\" is forbidden: exceeded quota: q, requested: pods=1, used: pods=1, limited: pods=1");
        // Not selected: not charged.
        admit(&st, "pods", "b1", &pod("p3")).await.unwrap();
        assert_eq!(st.get(&key).await.unwrap()["status"]["total"]["used"], json!({"pods": "1"}));
        // A refusal by the cluster quota gives a namespace quota's charge back.
        st.create(&ResourceStorage::namespaced_key("resourcequotas", "a1", "rq"), json!({"metadata": {"name": "rq", "namespace": "a1"},
            "spec": {"hard": {"pods": "5"}}, "status": {"hard": {"pods": "5"}, "used": {"pods": "1"}}})).await.unwrap();
        admit(&st, "pods", "a1", &pod("p4")).await.unwrap_err();
        let rq = st.get(&ResourceStorage::namespaced_key("resourcequotas", "a1", "rq")).await.unwrap();
        assert_eq!(rq["status"]["used"], json!({"pods": "1"}));
    }

    #[test]
    fn refusals_in_upstream_words() {
        let pod = json!({"spec": {"containers": [{"name": "c", "resources": {"requests": {"cpu": "500m"}}}]}});
        let u = quota::usage("pods", "", &pod);
        let quota = q(json!({"pods": "2", "requests.cpu": "1"}), json!({"pods": "1", "requests.cpu": "600m"}));
        assert_eq!(refusal(&quota, "pods", &pod, &quota::tracked(&quota, &u)).unwrap(),
                   "exceeded quota: q, requested: requests.cpu=500m, used: requests.cpu=600m, limited: requests.cpu=1");
        let fits = q(json!({"pods": "2", "requests.cpu": "1"}), json!({"pods": "1", "requests.cpu": "500m"}));
        assert!(refusal(&fits, "pods", &pod, &quota::tracked(&fits, &u)).is_none());
        let limits = q(json!({"limits.memory": "1Gi"}), json!({"limits.memory": "0"}));
        assert_eq!(refusal(&limits, "pods", &pod, &Usage::new()).unwrap(), "failed quota: q: must specify limits.memory for: c");
        let mut unknown = q(json!({"pods": "1"}), json!({}));
        unknown["status"] = json!({});
        assert!(refusal(&unknown, "pods", &pod, &quota::tracked(&unknown, &u)).unwrap().starts_with("status unknown for quota: q"));
    }
}
