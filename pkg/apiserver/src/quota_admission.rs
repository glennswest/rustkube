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

/// Add `delta` (negative to give it back) to the quota at `key`'s used, by
/// compare-and-swap; `Err` carries a refusal found on a re-read.
async fn charge(
    storage: &ResourceStorage,
    key: &str,
    resource: &str,
    obj: &Value,
    delta: &Usage,
    sign: i128,
) -> Result<(), ApiError> {
    for _ in 0..10 {
        let mut q = storage.get(key).await?;
        if sign > 0 {
            if let Some(why) = refusal(&q, resource, obj, delta) {
                return Err(ApiError::forbidden(&why));
            }
        }
        let mut used = used_of(&q);
        for (k, n) in delta {
            let e = used.entry(k.clone()).or_default();
            *e = (*e + sign * n).max(0);
        }
        let Some(hard) = q["spec"]["hard"].as_object().cloned() else { return Ok(()) };
        q["status"]["used"] = quota::resource_list(&hard, &used);
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

/// Check a create of `obj` (`resource` in the core or a built-in group)
/// in `namespace` against its quotas, and charge them.
pub async fn admit(storage: &ResourceStorage, resource: &str, namespace: &str, obj: &Value) -> Result<(), ApiError> {
    let Ok((quotas, _, _)) = storage.list(&ResourceStorage::namespace_prefix("resourcequotas", namespace), 500, None).await else {
        return Ok(());
    };
    if quotas.is_empty() {
        return Ok(());
    }
    let api_version = crate::handlers::resource::resource_to_api_version(resource);
    let group = api_version.split_once('/').map(|(g, _)| g).unwrap_or("");
    let usage = quota::usage(resource, group, obj);
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let forbidden = |why: &str| ApiError::forbidden(&format!("{resource} \"{name}\" is forbidden: {why}"));
    let mut plan = Vec::new();
    for q in &quotas {
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
        let qname = q["metadata"]["name"].as_str().unwrap_or("").to_string();
        plan.push((ResourceStorage::namespaced_key("resourcequotas", namespace, &qname), delta));
    }
    let mut done: Vec<&(String, Usage)> = Vec::new();
    for item in &plan {
        if let Err(e) = charge(storage, &item.0, resource, obj, &item.1, 1).await {
            for (key, delta) in &done {
                let _ = charge(storage, key, resource, obj, delta, -1).await;
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
