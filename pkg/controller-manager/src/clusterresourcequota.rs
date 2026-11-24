//! ClusterResourceQuota controller (#391, sc#50): each `quota.openshift.io/v1`
//! ClusterResourceQuota's status — `status.namespaces[]`, one entry per
//! namespace its selector picks with that namespace's usage, and
//! `status.total`, their sum — as OpenShift's cluster-policy-controller
//! `clusterquotareconciliation` keeps it.
//!
//! A namespace's usage is counted as a ResourceQuota with the quota's
//! `spec.quota` would count it there (`resourcequota::usage_in`, the rules
//! admission charges by). The same kinds are watched, and Namespaces: a
//! change to any of them, or a namespace relabelled into or out of a
//! selector, wakes every ClusterResourceQuota (there are few; each
//! recomputes from the feeds). Every quota is recomputed every 5 minutes as
//! well, which corrects a charge admission made for a create that was not
//! written; within 5 s of a charge a lower total is held back, as for a
//! ResourceQuota.
//!
//! A namespace keeps its place in `status.namespaces`; one newly selected is
//! added after the others, and one no longer selected is removed.

use crate::owned::{self, Controller, Dependency, Deps};
use crate::resourcequota::{settling, usage_in, RESYNC, WATCHED};
use crate::runner::ApiClient;
use apimachinery::quota::{self, Usage};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const PATH: &str = "/apis/quota.openshift.io/v1/clusterresourcequotas";
const NAMESPACES: &str = "/api/v1/namespaces";

pub struct ClusterResourceQuotaController {
    api: Arc<ApiClient>,
}

impl ClusterResourceQuotaController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }
}

/// `status.namespaces` for `selected` (name → usage): the stored entries'
/// order kept, new ones after them in name order.
fn namespaces_status(stored: &Value, hard: &Map<String, Value>, selected: &[(String, Usage)]) -> Vec<Value> {
    let mut order: Vec<&str> = stored.as_array().into_iter().flatten()
        .filter_map(|e| e["namespace"].as_str())
        .filter(|n| selected.iter().any(|(s, _)| s == n))
        .collect();
    for (n, _) in selected {
        if !order.contains(&n.as_str()) {
            order.push(n);
        }
    }
    order
        .into_iter()
        .map(|n| {
            let used = &selected.iter().find(|(s, _)| s == n).expect("selected").1;
            json!({"namespace": n, "status": {"hard": hard, "used": quota::resource_list(hard, used)}})
        })
        .collect()
}

/// The same status, whatever the quantities' spelling.
fn same_status(a: &Value, b: &Value) -> bool {
    let total = quota::same(&a["total"]["hard"], &b["total"]["hard"]) && quota::same(&a["total"]["used"], &b["total"]["used"]);
    let (ea, eb) = (a["namespaces"].as_array().cloned().unwrap_or_default(), b["namespaces"].as_array().cloned().unwrap_or_default());
    total
        && ea.len() == eb.len()
        && ea.iter().zip(&eb).all(|(x, y)| {
            x["namespace"] == y["namespace"]
                && quota::same(&x["status"]["hard"], &y["status"]["hard"])
                && quota::same(&x["status"]["used"], &y["status"]["used"])
        })
}

#[async_trait::async_trait]
impl Controller for ClusterResourceQuotaController {
    fn name(&self) -> &'static str {
        "clusterresourcequota"
    }
    fn primary(&self) -> &'static str {
        PATH
    }
    fn dependencies(&self) -> Vec<Dependency> {
        WATCHED
            .iter()
            .map(|(path, _, _)| *path)
            .chain([NAMESPACES])
            .map(|path| Dependency { path: path.into(), route: Arc::new(|_, primary| owned::all_keys(primary)) })
            .collect()
    }
    async fn reconcile(&self, crq: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        if !crq["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
        let name = crq["metadata"]["name"].as_str().unwrap_or("");
        let hard = crq["spec"]["quota"]["hard"].as_object().cloned().unwrap_or_default();
        // The quota as a ResourceQuota in each namespace.
        let as_quota = json!({"spec": crq["spec"]["quota"]});
        let mut namespaces: Vec<String> = deps
            .feed(WATCHED.len())
            .list()?
            .into_iter()
            .filter(|ns| quota::selects_namespace(&crq["spec"]["selector"], ns))
            .filter_map(|ns| ns["metadata"]["name"].as_str().map(str::to_string))
            .collect();
        namespaces.sort();
        let mut total = Usage::new();
        let mut selected = Vec::new();
        for ns in namespaces {
            let used = usage_in(&self.api, &as_quota, &ns, deps).await?;
            for (k, n) in &used {
                *total.entry(k.clone()).or_default() += n;
            }
            selected.push((ns, used));
        }
        let desired = json!({
            "total": {"hard": hard, "used": quota::resource_list(&hard, &total)},
            "namespaces": namespaces_status(&crq["status"]["namespaces"], &hard, &selected),
        });
        let stored = &crq["status"];
        let lowers = hard.keys().any(|k| quota::parse(k, &desired["total"]["used"][k]) < quota::parse(k, &stored["total"]["used"][k]));
        if lowers {
            if let Some(wait) = settling(crq) {
                apimachinery::reactor::requeue_after(wait);
                return Ok(());
            }
        }
        apimachinery::reactor::requeue_after(RESYNC);
        if same_status(stored, &desired) {
            return Ok(());
        }
        let mut updated = crq.clone();
        updated["status"] = desired;
        self.api.update_status(&format!("{PATH}/{name}"), &updated).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_keep_their_place() {
        let hard: Map<String, Value> = json!({"pods": "2"}).as_object().cloned().unwrap();
        let usage = |n: i128| Usage::from([("pods".to_string(), n)]);
        let stored = json!([{"namespace": "b"}, {"namespace": "gone"}, {"namespace": "a"}]);
        let list = namespaces_status(&stored, &hard, &[("a".into(), usage(1)), ("b".into(), usage(0)), ("c".into(), usage(1))]);
        let names: Vec<&str> = list.iter().map(|e| e["namespace"].as_str().unwrap()).collect();
        assert_eq!(names, ["b", "a", "c"], "kept order, the unselected dropped, the new last");
        assert_eq!(list[1]["status"], json!({"hard": {"pods": "2"}, "used": {"pods": "1"}}));
        assert_eq!(list[0]["status"]["used"], json!({"pods": "0"}));
    }

    #[test]
    fn a_status_is_the_same_whatever_its_spelling() {
        let a = json!({"total": {"hard": {"cpu": "1"}, "used": {"cpu": "500m"}}, "namespaces": [{"namespace": "x", "status": {"hard": {"cpu": "1"}, "used": {"cpu": "500m"}}}]});
        let b = json!({"total": {"hard": {"cpu": "1000m"}, "used": {"cpu": "0.5"}}, "namespaces": [{"namespace": "x", "status": {"hard": {"cpu": "1000m"}, "used": {"cpu": "0.5"}}}]});
        assert!(same_status(&a, &b));
        let mut c = b.clone();
        c["namespaces"][0]["namespace"] = json!("y");
        assert!(!same_status(&a, &c));
        assert!(!same_status(&a, &json!({"total": {}, "namespaces": null})), "a fresh quota is written");
    }
}
