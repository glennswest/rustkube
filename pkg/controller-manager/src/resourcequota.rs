//! ResourceQuota controller (#124): each quota's `status.hard` (its
//! `spec.hard`) and `status.used`, computed from what its namespace holds.
//!
//! Usage and scopes are `apimachinery::quota`'s, the same rules the
//! apiserver's quota admission charges with. Pods, Services, Secrets,
//! ConfigMaps, PersistentVolumeClaims, ReplicationControllers,
//! ResourceQuotas and ReplicaSets are watched and wake the quotas of their
//! namespace; another `count/<resource>.<group>` is counted by a LIST when
//! the quota is reconciled. Every quota is recomputed every 5 minutes as
//! well, which corrects a charge admission made for a create that was
//! then not written.
//!
//! A usage lower than the stored one is not written within 5 s of an
//! admission charge (`quota.rustkube.io/charged-at`): the charged object may
//! not be in the feeds yet, and lowering the quota under it would let the
//! next create through.

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::Index;
use apimachinery::quota::{self, Usage};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const CHARGED_AT: &str = "quota.rustkube.io/charged-at";
const RESYNC: Duration = Duration::from_secs(300);
const SETTLE: Duration = Duration::from_secs(5);

/// The watched kinds: (path, plural, group), in `dependencies()` order.
const WATCHED: [(&str, &str, &str); 8] = [
    ("/api/v1/pods", "pods", ""),
    ("/api/v1/services", "services", ""),
    ("/api/v1/secrets", "secrets", ""),
    ("/api/v1/configmaps", "configmaps", ""),
    ("/api/v1/persistentvolumeclaims", "persistentvolumeclaims", ""),
    ("/api/v1/replicationcontrollers", "replicationcontrollers", ""),
    ("/api/v1/resourcequotas", "resourcequotas", ""),
    ("/apis/apps/v1/replicasets", "replicasets", "apps"),
];

pub struct ResourceQuotaController {
    api: Arc<ApiClient>,
}

impl ResourceQuotaController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    /// The number of `resource.group` objects in `ns`, by LIST.
    async fn count(&self, ns: &str, resource: &str, group: &str) -> anyhow::Result<i128> {
        let base = if group.is_empty() {
            "/api/v1".to_string()
        } else {
            let g = self.api.list(&format!("/apis/{group}")).await?;
            let v = g["preferredVersion"]["version"].as_str().unwrap_or("v1").to_string();
            format!("/apis/{group}/{v}")
        };
        let list = self.api.list(&format!("{base}/namespaces/{ns}/{resource}")).await?;
        Ok(list["items"].as_array().map_or(0, |i| i.len()) as i128)
    }
}

/// `count/<plural>` or `count/<plural>.<group>` into (plural, group).
fn count_target(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("count/")?;
    Some(rest.split_once('.').unwrap_or((rest, "")))
}

/// Whether the stored usage may be lowered now: not within [`SETTLE`] of an
/// admission charge. `Some(wait)` when it may not.
fn settling(q: &Value) -> Option<Duration> {
    let at = q["metadata"]["annotations"][CHARGED_AT].as_str()?;
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    let age = (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).to_std().unwrap_or_default();
    (age < SETTLE).then(|| SETTLE - age)
}

#[async_trait::async_trait]
impl Controller for ResourceQuotaController {
    fn name(&self) -> &'static str {
        "resourcequota"
    }
    fn primary(&self) -> &'static str {
        "/api/v1/resourcequotas"
    }
    fn dependencies(&self) -> Vec<Dependency> {
        WATCHED
            .iter()
            .map(|(path, _, _)| Dependency { path: (*path).into(), route: Arc::new(owned::same_namespace) })
            .collect()
    }
    async fn reconcile(&self, q: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        if !q["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
        let ns = q["metadata"]["namespace"].as_str().unwrap_or("default");
        let name = q["metadata"]["name"].as_str().unwrap_or("");
        let hard = q["spec"]["hard"].as_object().cloned().unwrap_or_default();
        let mut used = Usage::new();
        for (i, (_, resource, group)) in WATCHED.iter().enumerate() {
            for obj in deps.feed(i).select(&Index::Namespace(ns.into()))? {
                if !quota::matches(q, resource, group, &obj) {
                    continue;
                }
                for (k, n) in quota::tracked(q, &quota::usage(resource, group, &obj)) {
                    *used.entry(k).or_default() += n;
                }
            }
        }
        let scoped = q["spec"]["scopes"].as_array().is_some_and(|s| !s.is_empty()) || q["spec"]["scopeSelector"].is_object();
        for key in hard.keys() {
            let Some((resource, group)) = count_target(key) else { continue };
            if scoped || WATCHED.iter().any(|(_, r, g)| *r == resource && *g == group) {
                continue;
            }
            used.insert(key.clone(), self.count(ns, resource, group).await?);
        }
        let desired_used = quota::resource_list(&hard, &used);
        let desired_hard = Value::Object(hard.clone());
        let stored = &q["status"]["used"];
        let lowers = hard.keys().any(|k| quota::parse(k, &desired_used[k]) < quota::parse(k, &stored[k]));
        if lowers {
            if let Some(wait) = settling(q) {
                apimachinery::reactor::requeue_after(wait);
                return Ok(());
            }
        }
        apimachinery::reactor::requeue_after(RESYNC);
        if quota::same(&q["status"]["hard"], &desired_hard) && quota::same(stored, &desired_used) {
            return Ok(());
        }
        let mut updated = q.clone();
        updated["status"] = json!({"hard": desired_hard, "used": desired_used});
        self.api.update_status(&format!("/api/v1/namespaces/{ns}/resourcequotas/{name}"), &updated).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_keys_name_their_resource() {
        assert_eq!(count_target("count/replicasets.apps"), Some(("replicasets", "apps")));
        assert_eq!(count_target("count/secrets"), Some(("secrets", "")));
        assert_eq!(count_target("count/widgets.stable.example.com"), Some(("widgets", "stable.example.com")));
        assert_eq!(count_target("pods"), None);
    }

    #[test]
    fn a_fresh_charge_holds_a_lower_usage_back() {
        let now = chrono::Utc::now().to_rfc3339();
        assert!(settling(&json!({"metadata": {"annotations": {CHARGED_AT: now}}})).is_some());
        let old = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        assert!(settling(&json!({"metadata": {"annotations": {CHARGED_AT: old}}})).is_none());
        assert!(settling(&json!({"metadata": {}})).is_none());
    }
}
