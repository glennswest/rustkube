//! Garbage collector — owner references, in all three propagation policies.
//!
//! Deleting a Deployment deletes its ReplicaSets, which delete their Pods.
//! *How* that happens is the `propagationPolicy` on the delete, and until now
//! only one of the three worked:
//!
//! - **Background** (the default): the owner goes immediately and the children
//!   are collected afterwards, when a pass notices their owner's UID is gone.
//!   This is what was here.
//! - **Foreground**: the children go *first*, and the owner is held by a
//!   `foregroundDeletion` finalizer until they have. `kubectl delete
//!   --cascade=foreground` and every `--wait` that means "gone means gone"
//!   depend on it.
//! - **Orphan**: the children survive, with the reference to their dead owner
//!   stripped, held by an `orphan` finalizer until it has been.
//!
//! The apiserver has set those finalizers since v0.7.30 and nothing ever
//! removed them, so a foreground or orphan delete stuck in `Terminating`
//! forever — the object never went, and neither did anything waiting on it.
//!
//! The kinds are discovered rather than listed, which is the other half of the
//! fix. The hardcoded table could not see custom resources, and a child whose
//! owner is a kind the collector cannot see looks exactly like a child whose
//! owner is gone: a KubeVirt `VirtualMachineInstance` owns its launcher pod,
//! and a collector that has never heard of a VMI would delete that pod every
//! pass. Ownership is now only acted on for kinds actually observed, so an
//! owner we cannot see is a reason to leave its children alone.

use crate::runner::ApiClient;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Reconcile interval.
const GC_INTERVAL: Duration = Duration::from_secs(30);

/// Events older than this are reaped (upstream default ~1h).
const EVENT_TTL: chrono::Duration = chrono::Duration::hours(1);

const FOREGROUND_FINALIZER: &str = "foregroundDeletion";
const ORPHAN_FINALIZER: &str = "orphan";

/// Resources never walked: Events have their own TTL pass below and there are
/// thousands of them, and a Node or a Namespace is not a dependent of
/// anything.
const SKIP_RESOURCES: &[&str] = &["events", "componentstatuses", "bindings", "tokenreviews"];

/// One resource type the collector can see.
#[derive(Clone, Debug)]
struct Resource {
    /// e.g. `v1` or `apps/v1`.
    group_version: String,
    /// Plural name, e.g. `replicasets`.
    name: String,
    /// e.g. `ReplicaSet`.
    kind: String,
    namespaced: bool,
}

impl Resource {
    fn list_path(&self) -> String {
        if self.group_version == "v1" {
            format!("/api/v1/{}", self.name)
        } else {
            format!("/apis/{}/{}", self.group_version, self.name)
        }
    }

    fn object_path(&self, namespace: &str, name: &str) -> String {
        let root = if self.group_version == "v1" {
            "/api/v1".to_string()
        } else {
            format!("/apis/{}", self.group_version)
        };
        if self.namespaced {
            format!("{root}/namespaces/{namespace}/{}/{name}", self.name)
        } else {
            format!("{root}/{}/{name}", self.name)
        }
    }
}

/// An object, with enough context to find it again.
struct Object {
    resource: Resource,
    value: Value,
}

impl Object {
    fn uid(&self) -> &str {
        self.value["metadata"]["uid"].as_str().unwrap_or("")
    }
    fn name(&self) -> &str {
        self.value["metadata"]["name"].as_str().unwrap_or("")
    }
    fn namespace(&self) -> &str {
        self.value["metadata"]["namespace"].as_str().unwrap_or("")
    }
    fn path(&self) -> String {
        self.resource.object_path(self.namespace(), self.name())
    }
    fn deleting(&self) -> bool {
        !self.value["metadata"]["deletionTimestamp"].is_null()
    }
    fn finalizers(&self) -> Vec<&str> {
        self.value["metadata"]["finalizers"]
            .as_array()
            .map(|fs| fs.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
    fn owner_refs(&self) -> Vec<&Value> {
        self.value["metadata"]["ownerReferences"]
            .as_array()
            .map(|os| os.iter().collect())
            .unwrap_or_default()
    }
}

pub struct GarbageCollector {
    api: Arc<ApiClient>,
}

impl GarbageCollector {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        info!("Starting garbage collector");
        loop {
            self.collect().await;
            tokio::time::sleep(GC_INTERVAL).await;
        }
    }

    async fn collect(&self) {
        let resources = self.discover().await;
        if resources.is_empty() {
            return; // apiserver unreachable; a pass over nothing deletes nothing
        }

        let mut objects: Vec<Object> = Vec::new();
        // Kinds we actually managed to list. An owner of any other kind is
        // unknown, not absent.
        let mut seen_kinds: HashSet<String> = HashSet::new();
        for resource in &resources {
            let list = match self.api.list(&resource.list_path()).await {
                Ok(l) => l,
                Err(e) => {
                    debug!("gc: cannot list {}: {e}", resource.list_path());
                    continue;
                }
            };
            let Some(items) = list["items"].as_array() else {
                continue;
            };
            seen_kinds.insert(format!("{}/{}", resource.group_version, resource.kind));
            for item in items {
                objects.push(Object {
                    resource: resource.clone(),
                    value: item.clone(),
                });
            }
        }

        let live: HashSet<String> = objects
            .iter()
            .map(|o| o.uid().to_string())
            .filter(|u| !u.is_empty())
            .collect();

        // uid -> indices of its dependents.
        let mut dependents: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, obj) in objects.iter().enumerate() {
            for owner in obj.owner_refs() {
                if let Some(uid) = owner["uid"].as_str() {
                    dependents.entry(uid.to_string()).or_default().push(i);
                }
            }
        }

        debug!(
            "gc: {} objects across {} kinds, {} with dependents",
            objects.len(),
            seen_kinds.len(),
            dependents.len()
        );
        self.process_finalizers(&objects, &dependents).await;
        self.background_cascade(&objects, &live, &seen_kinds).await;
        self.expire_events().await;
    }

    /// Every listable, deletable resource the apiserver serves — built-ins and
    /// custom resources alike.
    async fn discover(&self) -> Vec<Resource> {
        let mut out = Vec::new();
        if let Ok(core) = self.api.list("/api/v1").await {
            out.extend(parse_resource_list("v1", &core));
        }
        let groups = match self.api.list("/apis").await {
            Ok(g) => g,
            Err(_) => return out,
        };
        let mut versions: Vec<String> = Vec::new();
        for group in groups["groups"].as_array().cloned().unwrap_or_default() {
            // The preferred version is the one clients use, so it is the one
            // whose objects exist.
            if let Some(gv) = group["preferredVersion"]["groupVersion"].as_str() {
                versions.push(gv.to_string());
            } else if let Some(gv) = group["versions"][0]["groupVersion"].as_str() {
                versions.push(gv.to_string());
            }
        }
        for gv in versions {
            if let Ok(list) = self.api.list(&format!("/apis/{gv}")).await {
                out.extend(parse_resource_list(&gv, &list));
            }
        }
        out
    }

    /// Foreground and orphan deletion: the two policies that hold an object
    /// with a finalizer until the collector has done something about its
    /// dependents.
    async fn process_finalizers(
        &self,
        objects: &[Object],
        dependents: &HashMap<String, Vec<usize>>,
    ) {
        for owner in objects.iter().filter(|o| o.deleting()) {
            let finalizers = owner.finalizers();
            let foreground = finalizers.contains(&FOREGROUND_FINALIZER);
            let orphan = finalizers.contains(&ORPHAN_FINALIZER);
            debug!(
                "gc: {} {}/{} is terminating with finalizers {:?}",
                owner.resource.kind,
                owner.namespace(),
                owner.name(),
                finalizers
            );
            if !foreground && !orphan {
                continue;
            }
            let children: Vec<&Object> = dependents
                .get(owner.uid())
                .map(|idx| idx.iter().map(|i| &objects[*i]).collect())
                .unwrap_or_default();

            if orphan {
                // Cut the reference, then let the owner go. Cutting first
                // matters: a child left pointing at a UID that no longer
                // exists is collected by the background pass, which is the
                // opposite of what orphaning means.
                let mut all_cut = true;
                for child in &children {
                    if !self.strip_owner(child, owner.uid()).await {
                        all_cut = false;
                    }
                }
                if all_cut {
                    self.remove_finalizer(owner, ORPHAN_FINALIZER).await;
                    info!(
                        "gc: orphaned {} dependents of {}/{}",
                        children.len(),
                        owner.resource.kind,
                        owner.name()
                    );
                }
                continue;
            }

            // Foreground: the dependents go first, and the owner is held until
            // the ones that said they block it are gone.
            let mut blocking = 0usize;
            for child in &children {
                let blocks = child.owner_refs().iter().any(|r| {
                    r["uid"].as_str() == Some(owner.uid())
                        && r["blockOwnerDeletion"].as_bool() == Some(true)
                });
                if blocks {
                    blocking += 1;
                }
                if child.deleting() {
                    continue; // already on its way
                }
                let path = child.path();
                // The policy propagates: a foreground delete two levels deep
                // that turns into a background delete one level down is not a
                // foreground delete.
                let opts = json!({
                    "apiVersion": "meta.k8s.io/v1",
                    "kind": "DeleteOptions",
                    "propagationPolicy": "Foreground"
                });
                match self.api.delete_with_options(&path, &opts).await {
                    Ok(r) if r.status().is_success() || r.status().as_u16() == 404 => {
                        info!(
                            "gc: foreground delete of {} {}/{} (owner {} is going)",
                            child.resource.kind,
                            child.namespace(),
                            child.name(),
                            owner.name()
                        );
                    }
                    Ok(r) => warn!("gc: delete {path} returned {}", r.status()),
                    Err(e) => warn!("gc: delete {path} failed: {e}"),
                }
            }
            if blocking == 0 {
                self.remove_finalizer(owner, FOREGROUND_FINALIZER).await;
                debug!(
                    "gc: {}/{} has no blocking dependents left",
                    owner.resource.kind,
                    owner.name()
                );
            }
        }
    }

    /// Background propagation: a child whose controlling owner no longer
    /// exists is garbage.
    async fn background_cascade(
        &self,
        objects: &[Object],
        live: &HashSet<String>,
        seen_kinds: &HashSet<String>,
    ) {
        for obj in objects {
            if obj.deleting() {
                continue;
            }
            let owners = obj.owner_refs();
            if owners.is_empty() {
                continue;
            }
            // Upstream rule: an object is garbage when *every* owner is gone.
            // One live owner keeps it, whichever one is the controller.
            let mut any_live = false;
            let mut all_known = true;
            for owner in &owners {
                let kind = format!(
                    "{}/{}",
                    owner["apiVersion"].as_str().unwrap_or(""),
                    owner["kind"].as_str().unwrap_or("")
                );
                if !seen_kinds.contains(&kind) {
                    all_known = false;
                    break;
                }
                if live.contains(owner["uid"].as_str().unwrap_or("")) {
                    any_live = true;
                    break;
                }
            }
            if any_live || !all_known {
                continue;
            }

            let path = obj.path();
            match self.api.delete(&path).await {
                Ok(r) if r.status().is_success() || r.status().as_u16() == 404 => info!(
                    "gc: deleted {} {}/{} — its owner is gone",
                    obj.resource.kind,
                    obj.namespace(),
                    obj.name()
                ),
                Ok(r) => debug!("gc: delete {path} returned {}", r.status()),
                Err(e) => warn!("gc: failed to delete {path}: {e}"),
            }
        }
    }

    /// Remove one owner reference from a dependent. Returns whether the
    /// dependent is now free of that owner.
    async fn strip_owner(&self, child: &Object, owner_uid: &str) -> bool {
        let remaining: Vec<Value> = child
            .owner_refs()
            .into_iter()
            .filter(|r| r["uid"].as_str() != Some(owner_uid))
            .cloned()
            .collect();
        if remaining.len() == child.owner_refs().len() {
            return true; // nothing to do
        }
        // The whole list is sent: a merge patch cannot remove a list entry.
        let patch = json!({"metadata": {"ownerReferences": remaining}});
        match self.api.patch(&child.path(), &patch).await {
            Ok(_) => true,
            Err(e) => {
                warn!("gc: could not orphan {}: {e}", child.path());
                false
            }
        }
    }

    async fn remove_finalizer(&self, obj: &Object, finalizer: &str) {
        let remaining: Vec<Value> = obj
            .finalizers()
            .into_iter()
            .filter(|f| *f != finalizer)
            .map(|f| json!(f))
            .collect();
        let patch = json!({"metadata": {"finalizers": remaining}});
        if let Err(e) = self.api.patch(&obj.path(), &patch).await {
            warn!("gc: could not clear {finalizer} on {}: {e}", obj.path());
        }
    }

    /// Delete Events whose lastTimestamp is older than `EVENT_TTL`.
    async fn expire_events(&self) {
        let list = match self.api.list("/api/v1/events").await {
            Ok(l) => l,
            Err(_) => return,
        };
        let Some(items) = list["items"].as_array() else {
            return;
        };
        let now = chrono::Utc::now();
        for ev in items {
            let ts = ev["lastTimestamp"]
                .as_str()
                .or_else(|| ev["eventTime"].as_str())
                .or_else(|| ev["metadata"]["creationTimestamp"].as_str());
            let stale = ts
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)) > EVENT_TTL)
                .unwrap_or(false);
            if !stale {
                continue;
            }
            let ns = ev["metadata"]["namespace"].as_str().unwrap_or("default");
            let name = ev["metadata"]["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let _ = self
                .api
                .delete(&format!("/api/v1/namespaces/{ns}/events/{name}"))
                .await;
        }
    }
}

/// Pull the walkable resources out of an `APIResourceList`.
///
/// Subresources (`pods/status`) are not objects, and a resource the server
/// will not let us list or delete is one we can do nothing about.
fn parse_resource_list(group_version: &str, list: &Value) -> Vec<Resource> {
    list["resources"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| {
                    let name = r["name"].as_str()?;
                    if name.contains('/') || SKIP_RESOURCES.contains(&name) {
                        return None;
                    }
                    let verbs: Vec<&str> = r["verbs"]
                        .as_array()
                        .map(|v| v.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if !verbs.contains(&"list") || !verbs.contains(&"delete") {
                        return None;
                    }
                    Some(Resource {
                        group_version: group_version.to_string(),
                        name: name.to_string(),
                        kind: r["kind"].as_str().unwrap_or("").to_string(),
                        namespaced: r["namespaced"].as_bool().unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resource(gv: &str, name: &str, kind: &str, namespaced: bool) -> Resource {
        Resource {
            group_version: gv.into(),
            name: name.into(),
            kind: kind.into(),
            namespaced,
        }
    }

    #[test]
    fn paths_are_built_for_core_and_grouped_resources() {
        let pods = resource("v1", "pods", "Pod", true);
        assert_eq!(pods.list_path(), "/api/v1/pods");
        assert_eq!(pods.object_path("kube-system", "p1"), "/api/v1/namespaces/kube-system/pods/p1");

        let rs = resource("apps/v1", "replicasets", "ReplicaSet", true);
        assert_eq!(rs.list_path(), "/apis/apps/v1/replicasets");
        assert_eq!(
            rs.object_path("default", "web-abc"),
            "/apis/apps/v1/namespaces/default/replicasets/web-abc"
        );

        let pv = resource("v1", "persistentvolumes", "PersistentVolume", false);
        assert_eq!(pv.object_path("", "pv1"), "/api/v1/persistentvolumes/pv1");
    }

    #[test]
    fn discovery_skips_subresources_and_read_only_kinds() {
        let list = json!({"resources": [
            {"name": "pods", "kind": "Pod", "namespaced": true,
             "verbs": ["create", "delete", "get", "list"]},
            {"name": "pods/status", "kind": "Pod", "namespaced": true,
             "verbs": ["get", "patch", "update"]},
            {"name": "componentstatuses", "kind": "ComponentStatus", "namespaced": false,
             "verbs": ["get", "list", "delete"]},
            {"name": "bindings", "kind": "Binding", "namespaced": true,
             "verbs": ["create"]},
            {"name": "events", "kind": "Event", "namespaced": true,
             "verbs": ["list", "delete"]},
        ]});
        let got = parse_resource_list("v1", &list);
        let names: Vec<&str> = got.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["pods"]);
    }

    #[test]
    fn custom_resources_are_discovered_like_any_other() {
        let list = json!({"resources": [
            {"name": "ciliumnetworkpolicies", "kind": "CiliumNetworkPolicy",
             "namespaced": true, "verbs": ["create", "delete", "get", "list", "watch"]}
        ]});
        let got = parse_resource_list("cilium.io/v2", &list);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "CiliumNetworkPolicy");
        assert_eq!(got[0].list_path(), "/apis/cilium.io/v2/ciliumnetworkpolicies");
    }
}
