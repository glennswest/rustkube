//! Deployment controller.
//!
//! Turns a Deployment into ReplicaSets, one per pod template, and moves
//! replicas between them according to the deployment strategy. The arithmetic
//! lives in [`crate::rollout`]; this file is the I/O around it.
//!
//! Three things this controller must get right, and each is a promise someone
//! relies on:
//!
//! - **A rolling update actually rolls.** New pods come up before old ones go
//!   down, within `maxSurge` and `maxUnavailable`. The previous version of
//!   this controller created the new ReplicaSet at full size and set the old
//!   one to zero in the same pass, which is not a rolling update — it is an
//!   outage with extra steps, and it ignored both settings.
//! - **History is kept.** Every ReplicaSet carries
//!   `deployment.kubernetes.io/revision`, and old ones are retained up to
//!   `revisionHistoryLimit`. This is what makes `kubectl rollout history` and
//!   `kubectl rollout undo` work at all: undo is a client-side operation that
//!   finds the ReplicaSet for a revision and writes its template back onto the
//!   Deployment. With no revisions there is nothing to find, and rollback is
//!   impossible however the client asks for it.
//! - **Status is the truth.** `updatedReplicas` used to report the *desired*
//!   count, so a Deployment whose pods were all failing still read as fully
//!   updated. Every field now comes from the ReplicaSets' own status.

use crate::rollout::{self, RsView};
use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, warn};

/// The annotation `kubectl rollout history` reads.
const REVISION: &str = "deployment.kubernetes.io/revision";

pub struct DeploymentController {
    api: Arc<ApiClient>,
    recorder: crate::events::EventRecorder,
}

impl DeploymentController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self {
            recorder: crate::events::EventRecorder::new(api.clone(), "deployment-controller"),
            api,
        }
    }

    pub async fn run(&self) {
        info!("Deployment controller started");
        let mut interval = time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;
            if let Err(e) = self.reconcile_all().await {
                error!("Deployment reconcile error: {e}");
            }
        }
    }

    async fn reconcile_all(&self) -> anyhow::Result<()> {
        let ns_list: Value = self.api.list("/api/v1/namespaces").await?;
        let namespaces = ns_list["items"].as_array().cloned().unwrap_or_default();

        for ns in &namespaces {
            let ns_name = ns["metadata"]["name"].as_str().unwrap_or("default");
            if let Err(e) = self.reconcile_namespace(ns_name).await {
                debug!("Deployment reconcile in {ns_name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_namespace(&self, namespace: &str) -> anyhow::Result<()> {
        let deploy_list: Value = self
            .api
            .list(&format!("/apis/apps/v1/namespaces/{namespace}/deployments"))
            .await?;
        let deployments = deploy_list["items"].as_array().cloned().unwrap_or_default();

        let rs_list: Value = self
            .api
            .list(&format!("/apis/apps/v1/namespaces/{namespace}/replicasets"))
            .await?;
        let replicasets = rs_list["items"].as_array().cloned().unwrap_or_default();

        for deploy in &deployments {
            if let Err(e) = self.reconcile_deployment(namespace, deploy, &replicasets).await {
                let name = deploy["metadata"]["name"].as_str().unwrap_or("?");
                warn!("Failed to reconcile deployment {namespace}/{name}: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_deployment(
        &self,
        namespace: &str,
        deploy: &Value,
        existing_rs: &[Value],
    ) -> anyhow::Result<()> {
        let deploy_name = deploy["metadata"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("deployment missing name"))?;
        let deploy_uid = deploy["metadata"]["uid"].as_str().unwrap_or("");
        let desired = deploy["spec"]["replicas"].as_u64().unwrap_or(1);
        let selector = &deploy["spec"]["selector"];
        let pod_template = &deploy["spec"]["template"];
        let paused = deploy["spec"]["paused"].as_bool().unwrap_or(false);
        let history_limit = deploy["spec"]["revisionHistoryLimit"].as_u64().unwrap_or(10);

        let owned: Vec<&Value> = existing_rs
            .iter()
            .filter(|rs| {
                rs["metadata"]["ownerReferences"]
                    .as_array()
                    .map(|refs| refs.iter().any(|r| r["uid"].as_str() == Some(deploy_uid)))
                    .unwrap_or(false)
            })
            .collect();

        let template_hash = compute_template_hash(pod_template);
        let rs_name = format!("{deploy_name}-{template_hash}");

        let all_views: Vec<RsView> = owned.iter().map(|rs| view_of(rs)).collect();

        // The ReplicaSet for the current template — which may be one that
        // already exists at an older revision, because that is exactly what a
        // rollback is: the template goes back to a previous value, and the
        // ReplicaSet that already carries it becomes current again.
        let current = owned
            .iter()
            .find(|rs| rs["metadata"]["name"].as_str() == Some(&rs_name))
            .copied();

        let new_view = match current {
            Some(rs) => {
                // Re-selected after being scaled down: it becomes the newest
                // revision rather than reclaiming its old number, so
                // `rollout history` stays ordered and unambiguous.
                let mut v = view_of(rs);
                let highest = all_views.iter().map(|r| r.revision).max().unwrap_or(0);
                if v.revision < highest {
                    let bumped = rollout::next_revision(&all_views);
                    self.set_revision(namespace, &rs_name, rs, bumped).await?;
                    info!(
                        "Deployment {namespace}/{deploy_name} rolled back to template \
                         {template_hash}, now revision {bumped}"
                    );
                    self.recorder
                        .event(
                            deploy,
                            "Normal",
                            "DeploymentRollback",
                            &format!("Rolled back to replica set {rs_name} as revision {bumped}"),
                        )
                        .await;
                    v.revision = bumped;
                }
                v
            }
            None => {
                let revision = rollout::next_revision(&all_views);
                self.create_replicaset(
                    namespace,
                    deploy,
                    deploy_name,
                    deploy_uid,
                    &rs_name,
                    &template_hash,
                    selector,
                    pod_template,
                    revision,
                )
                .await?;
                // Created at zero and scaled by the plan below, so the very
                // first pass of a rollout still respects maxSurge instead of
                // arriving at full size.
                RsView { name: rs_name.clone(), revision, spec_replicas: 0, available: 0 }
            }
        };

        let olds: Vec<RsView> =
            all_views.iter().filter(|v| v.name != rs_name).cloned().collect();

        // Paused stops the rollout, not the bookkeeping: status still tracks
        // reality, which is the whole reason to pause — to look at it.
        if !paused {
            let (surge, unavailable, recreate) = strategy(deploy, desired);
            let plan = if recreate {
                rollout::plan_recreate(desired, &new_view, &olds)
            } else {
                rollout::plan_rolling(desired, surge, unavailable, &new_view, &olds)
            };

            // A ReplicaSet created in this very pass is not in `owned` — it
            // was listed before the create. It gets scaled on the next tick,
            // two seconds later, which keeps the surge accounting honest
            // rather than creating it at full size to save a tick.
            if plan.new_replicas != new_view.spec_replicas {
                if let Some(rs) = by_name(&owned, &rs_name) {
                    self.scale(namespace, rs, plan.new_replicas).await?;
                }
                info!(
                    "Scaled up replica set {rs_name} to {} (deployment {namespace}/{deploy_name})",
                    plan.new_replicas
                );
                self.recorder
                    .event(
                        deploy,
                        "Normal",
                        "ScalingReplicaSet",
                        &format!("Scaled up replica set {rs_name} to {}", plan.new_replicas),
                    )
                    .await;
            }
            for (name, n) in &plan.old {
                let Some(rs) = by_name(&owned, name) else { continue };
                self.scale(namespace, rs, *n).await?;
                info!("Scaled down replica set {name} to {n}");
                self.recorder
                    .event(
                        deploy,
                        "Normal",
                        "ScalingReplicaSet",
                        &format!("Scaled down replica set {name} to {n}"),
                    )
                    .await;
            }

            // Prune only once the rollout has settled. Deleting history while
            // a rollout is in flight can remove the ReplicaSet someone is
            // about to roll back to.
            let settled = plan.old.is_empty() && plan.new_replicas == desired;
            if settled {
                for name in rollout::prunable(&olds, history_limit) {
                    let path =
                        format!("/apis/apps/v1/namespaces/{namespace}/replicasets/{name}");
                    if let Err(e) = self.api.delete(&path).await {
                        debug!("pruning replica set {name}: {e}");
                    } else {
                        info!("Pruned replica set {name} (revisionHistoryLimit {history_limit})");
                    }
                }
            }
        }

        self.update_status(namespace, deploy, deploy_name, desired, &new_view, &olds, paused)
            .await;
        Ok(())
    }

    /// Write `spec.replicas` on a ReplicaSet.
    ///
    /// Takes the object rather than re-fetching it: the caller listed every
    /// ReplicaSet in the namespace one step ago, and a second GET per scale
    /// would be a request per ReplicaSet per two-second pass.
    async fn scale(&self, namespace: &str, rs: &Value, replicas: u64) -> anyhow::Result<()> {
        if rs["spec"]["replicas"].as_u64() == Some(replicas) {
            return Ok(());
        }
        let name = rs["metadata"]["name"].as_str().unwrap_or_default();
        let mut updated = rs.clone();
        updated["spec"]["replicas"] = json!(replicas);
        self.api
            .update(
                &format!("/apis/apps/v1/namespaces/{namespace}/replicasets/{name}"),
                &updated,
            )
            .await?;
        Ok(())
    }

    async fn set_revision(
        &self,
        namespace: &str,
        name: &str,
        rs: &Value,
        revision: u64,
    ) -> anyhow::Result<()> {
        let mut updated = rs.clone();
        if updated["metadata"]["annotations"].is_null() {
            updated["metadata"]["annotations"] = json!({});
        }
        updated["metadata"]["annotations"][REVISION] = json!(revision.to_string());
        self.api
            .update(
                &format!("/apis/apps/v1/namespaces/{namespace}/replicasets/{name}"),
                &updated,
            )
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_replicaset(
        &self,
        namespace: &str,
        deploy: &Value,
        deploy_name: &str,
        deploy_uid: &str,
        rs_name: &str,
        template_hash: &str,
        selector: &Value,
        pod_template: &Value,
        revision: u64,
    ) -> anyhow::Result<()> {
        let mut rs_labels = selector["matchLabels"].clone();
        if rs_labels.is_null() {
            rs_labels = json!({});
        }
        if let Some(obj) = rs_labels.as_object_mut() {
            obj.insert("pod-template-hash".into(), json!(template_hash));
        }

        // The hash goes on the *pod* template too, not only on the ReplicaSet.
        // Without it the pods a ReplicaSet creates do not carry the label its
        // own selector requires — harmless here because ownership is tracked
        // by ownerReference, but it means nothing can select the pods of one
        // revision, which is what a canary or a per-revision Service needs.
        let mut template = pod_template.clone();
        if template["metadata"]["labels"].is_null() {
            template["metadata"]["labels"] = json!({});
        }
        if let Some(obj) = template["metadata"]["labels"].as_object_mut() {
            obj.insert("pod-template-hash".into(), json!(template_hash));
        }

        let rs = json!({
            "apiVersion": "apps/v1",
            "kind": "ReplicaSet",
            "metadata": {
                "name": rs_name,
                "namespace": namespace,
                "labels": rs_labels,
                "annotations": { REVISION: revision.to_string() },
                "ownerReferences": [{
                    "apiVersion": "apps/v1",
                    "kind": "Deployment",
                    "name": deploy_name,
                    "uid": deploy_uid,
                    "controller": true,
                    "blockOwnerDeletion": true
                }]
            },
            "spec": {
                "replicas": 0,
                "selector": { "matchLabels": rs_labels },
                "template": template
            }
        });

        self.api
            .create(
                &format!("/apis/apps/v1/namespaces/{namespace}/replicasets"),
                &rs,
            )
            .await?;
        info!("Created ReplicaSet {namespace}/{rs_name} at revision {revision}");
        self.recorder
            .event(
                deploy,
                "Normal",
                "ScalingReplicaSet",
                &format!("Created replica set {rs_name}"),
            )
            .await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_status(
        &self,
        namespace: &str,
        deploy: &Value,
        deploy_name: &str,
        desired: u64,
        new: &RsView,
        olds: &[RsView],
        paused: bool,
    ) {
        // Every count comes from the ReplicaSets rather than from the spec.
        // Reporting the desired count as `updatedReplicas` — which this
        // controller used to do — makes a Deployment whose pods all crash
        // read as fully rolled out.
        let path = format!("/apis/apps/v1/namespaces/{namespace}/replicasets");
        let list: Value = match self.api.list(&path).await {
            Ok(l) => l,
            Err(_) => return,
        };
        let items = list["items"].as_array().cloned().unwrap_or_default();
        let mine: Vec<&Value> = items
            .iter()
            .filter(|rs| {
                let n = rs["metadata"]["name"].as_str().unwrap_or("");
                n == new.name || olds.iter().any(|o| o.name == n)
            })
            .collect();

        let sum = |field: &str| -> u64 {
            mine.iter().map(|rs| rs["status"][field].as_u64().unwrap_or(0)).sum()
        };
        let replicas = sum("replicas");
        let ready = sum("readyReplicas");
        let available = sum("availableReplicas");
        let updated = mine
            .iter()
            .find(|rs| rs["metadata"]["name"].as_str() == Some(&new.name))
            .and_then(|rs| rs["status"]["replicas"].as_u64())
            .unwrap_or(0);

        let (_, max_unavailable, _) = strategy(deploy, desired);
        let min_available = desired.saturating_sub(max_unavailable);
        let is_available = available >= min_available.max(1) || desired == 0;
        let complete = updated == desired && replicas == desired && available == desired;

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let (prog_status, prog_reason) = if paused {
            ("Unknown", "DeploymentPaused")
        } else if complete {
            ("True", "NewReplicaSetAvailable")
        } else {
            ("True", "ReplicaSetUpdated")
        };

        // The Deployment's own revision annotation is what `rollout history`
        // reports as current. It is metadata, so it cannot ride on the status
        // write — and it is written only when it actually changes, because a
        // whole-object PUT every two seconds would revert any concurrent
        // `kubectl scale` or `kubectl set image`.
        let want_rev = new.revision.to_string();
        if deploy["metadata"]["annotations"][REVISION].as_str() != Some(&want_rev) {
            let mut ann = deploy.clone();
            if ann["metadata"]["annotations"].is_null() {
                ann["metadata"]["annotations"] = json!({});
            }
            ann["metadata"]["annotations"][REVISION] = json!(want_rev);
            let _ = self
                .api
                .update(
                    &format!("/apis/apps/v1/namespaces/{namespace}/deployments/{deploy_name}"),
                    &ann,
                )
                .await;
        }

        let mut updated_deploy = deploy.clone();
        updated_deploy["status"] = json!({
            "replicas": replicas,
            "updatedReplicas": updated,
            "readyReplicas": ready,
            "availableReplicas": available,
            "unavailableReplicas": desired.saturating_sub(available),
            "observedGeneration": deploy["metadata"]["generation"].as_u64().unwrap_or(1),
            "conditions": [{
                "type": "Available",
                "status": if is_available { "True" } else { "False" },
                "reason": if is_available { "MinimumReplicasAvailable" }
                          else { "MinimumReplicasUnavailable" },
                "lastTransitionTime": now,
            }, {
                "type": "Progressing",
                "status": prog_status,
                "reason": prog_reason,
                "lastTransitionTime": now,
            }]
        });

        // status only — see ApiClient::update_status.
        let _ = self
            .api
            .update_status(
                &format!("/apis/apps/v1/namespaces/{namespace}/deployments/{deploy_name}"),
                &updated_deploy,
            )
            .await;
    }
}

/// `(maxSurge, maxUnavailable, is_recreate)` for a Deployment.
///
/// Upstream's defaults are 25% each, and `RollingUpdate` is the default
/// strategy. A `Recreate` Deployment has no surge or unavailable settings —
/// the strategy is the answer.
fn strategy(deploy: &Value, desired: u64) -> (u64, u64, bool) {
    let ty = deploy["spec"]["strategy"]["type"]
        .as_str()
        .unwrap_or("RollingUpdate");
    if ty == "Recreate" {
        return (0, desired, true);
    }
    let ru = &deploy["spec"]["strategy"]["rollingUpdate"];
    let surge = if ru["maxSurge"].is_null() {
        rollout::int_or_percent(&json!("25%"), desired, true)
    } else {
        rollout::int_or_percent(&ru["maxSurge"], desired, true)
    };
    let unavailable = if ru["maxUnavailable"].is_null() {
        rollout::int_or_percent(&json!("25%"), desired, false)
    } else {
        rollout::int_or_percent(&ru["maxUnavailable"], desired, false)
    };
    // Both zero is a rollout that cannot move: upstream rejects it at
    // validation, and here the safe reading is to allow one pod of surge
    // rather than to spin forever making no progress.
    if surge == 0 && unavailable == 0 {
        return (1, 0, false);
    }
    (surge, unavailable, false)
}

/// One of the listed ReplicaSets, by name.
fn by_name<'a>(owned: &[&'a Value], name: &str) -> Option<&'a Value> {
    owned
        .iter()
        .find(|rs| rs["metadata"]["name"].as_str() == Some(name))
        .copied()
}

fn view_of(rs: &Value) -> RsView {
    RsView {
        name: rs["metadata"]["name"].as_str().unwrap_or("").to_string(),
        revision: rs["metadata"]["annotations"][REVISION]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        spec_replicas: rs["spec"]["replicas"].as_u64().unwrap_or(0),
        available: rs["status"]["availableReplicas"].as_u64().unwrap_or(0),
    }
}

/// A short, stable hash of the pod template, used as the ReplicaSet's name
/// suffix and its `pod-template-hash` label.
///
/// **Stability is the requirement, not speed.** The hash is baked into the
/// ReplicaSet's name, so if it changes for the same template the controller
/// stops recognising the ReplicaSet it created and makes another one —
/// silently orphaning the first and its pods. `DefaultHasher` is explicitly
/// documented as not stable across Rust releases, so it cannot be used here;
/// FNV-1a is fixed by its specification and will produce the same digits in
/// ten years.
fn compute_template_hash(template: &Value) -> String {
    // serde_json's default Map is a BTreeMap, so a template's keys serialise
    // in a fixed order and two equal templates always hash alike.
    let s = serde_json::to_string(template).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:010x}", h & 0xFF_FFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_strategy_is_a_rolling_update_at_25_percent() {
        let d = json!({"spec": {}});
        assert_eq!(strategy(&d, 8), (2, 2, false));
        // Rounding: surge up so a single replica can roll, unavailable down so
        // its only pod is not taken away.
        assert_eq!(strategy(&d, 1), (1, 0, false));
    }

    #[test]
    fn recreate_is_reported_as_such() {
        let d = json!({"spec": {"strategy": {"type": "Recreate"}}});
        let (_, _, recreate) = strategy(&d, 3);
        assert!(recreate);
    }

    #[test]
    fn explicit_surge_and_unavailable_are_honoured() {
        let d = json!({"spec": {"strategy": {"type": "RollingUpdate", "rollingUpdate": {
            "maxSurge": 3, "maxUnavailable": "50%"}}}});
        assert_eq!(strategy(&d, 4), (3, 2, false));
    }

    /// Zero surge and zero unavailable cannot make progress. Upstream rejects
    /// it in validation; here the safe reading is one pod of surge.
    #[test]
    fn a_rollout_that_could_not_move_gets_one_pod_of_surge() {
        let d = json!({"spec": {"strategy": {"rollingUpdate": {
            "maxSurge": 0, "maxUnavailable": 0}}}});
        assert_eq!(strategy(&d, 3), (1, 0, false));
    }

    /// The hash is a ReplicaSet's *name*. If it moved between builds the
    /// controller would stop recognising its own ReplicaSets and orphan them,
    /// so this asserts a fixed value rather than merely self-consistency.
    #[test]
    fn the_template_hash_is_stable_across_builds() {
        let t = json!({"metadata": {"labels": {"app": "web"}},
                       "spec": {"containers": [{"name": "c", "image": "busybox"}]}});
        // Pinned, and cross-checked against an independent FNV-1a of
        // serde_json's output — so this asserts the algorithm, not merely
        // that the function agrees with itself.
        assert_eq!(compute_template_hash(&t), "604fe2c1f8");
    }

    #[test]
    fn different_templates_hash_differently() {
        let a = json!({"spec": {"containers": [{"image": "busybox:1"}]}});
        let b = json!({"spec": {"containers": [{"image": "busybox:2"}]}});
        assert_ne!(compute_template_hash(&a), compute_template_hash(&b));
    }

    #[test]
    fn a_replicaset_without_a_revision_annotation_reads_as_zero() {
        let rs = json!({"metadata": {"name": "r"}, "spec": {"replicas": 2}});
        let v = view_of(&rs);
        assert_eq!(v.revision, 0);
        assert_eq!(v.spec_replicas, 2);
        assert_eq!(v.available, 0);
    }

    #[test]
    fn a_view_reads_revision_replicas_and_availability() {
        let rs = json!({
            "metadata": {"name": "r", "annotations": {REVISION: "7"}},
            "spec": {"replicas": 3},
            "status": {"availableReplicas": 2}});
        assert_eq!(view_of(&rs), RsView {
            name: "r".into(), revision: 7, spec_replicas: 3, available: 2 });
    }
}
