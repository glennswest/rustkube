//! Applying a directory of manifests at startup.
//!
//! **Why the apiserver owns this.** A cluster component that ships in the disk
//! image — the pod network is the first, and the reason this exists — has to
//! become API objects before anything can schedule it. The apiserver is the
//! only process that is up before the controllers and the kubelet, is already
//! the writer of record for every object, and already bootstraps namespaces
//! and RBAC this way. Putting the applier anywhere else means a second thing
//! that must be running and must be able to authenticate before the cluster
//! can bring up its own network — which is the bootstrap order that does not
//! work.
//!
//! This is **not** a reconciler. It runs once, at startup, and its job is
//! "these objects exist". Two modes, spelled the way the upstream addon
//! manager spells them, on the object's own annotations:
//!
//! | `addonmanager.kubernetes.io/mode` | behaviour |
//! |---|---|
//! | `EnsureExists` (default) | create if absent; never touch an existing object |
//! | `Reconcile` | create if absent; overwrite the spec if present |
//!
//! The default is deliberate: an operator who edits a DaemonSet to debug a
//! node should not have it reverted on the next apiserver restart. `Reconcile`
//! is what makes a version bump in a new system pallet actually land, so the
//! manifests that define a shipped component carry it and the ones that seed
//! configuration do not.
//!
//! Ordering is **filename order**, which is why the shipped manifests are
//! numbered. A CustomResourceDefinition has to be applied before an instance
//! of it, and a namespace before anything in it; nothing here infers that, so
//! the filenames carry it. A CRD applied in this pass is registered as it goes,
//! so a custom resource later in the same pass resolves.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::crd::CrdRegistry;
use crate::storage::ResourceStorage;

/// What to do when the object is already there.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    /// Leave it exactly as it is.
    EnsureExists,
    /// Overwrite it with the manifest.
    Reconcile,
}

impl Mode {
    fn of(obj: &Value) -> Mode {
        let ann = &obj["metadata"]["annotations"];
        // The upstream key first, then ours — a manifest copied from an
        // upstream chart should behave the way it says it does.
        let raw = ann["addonmanager.kubernetes.io/mode"]
            .as_str()
            .or_else(|| ann["storm.io/manifest-mode"].as_str())
            .unwrap_or("EnsureExists");
        match raw {
            "Reconcile" => Mode::Reconcile,
            _ => Mode::EnsureExists,
        }
    }
}

/// What one manifest document did.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Created,
    Updated,
    Unchanged,
    /// Could not be applied, and why. Reported rather than fatal: one bad
    /// manifest must not stop an apiserver from starting, or a typo in an
    /// addon takes the cluster with it.
    Failed(String),
}

/// Split a file's contents into documents.
///
/// YAML multi-document (`---`) because that is how every real manifest set is
/// written; a JSON file is one document. Empty documents — a trailing `---`,
/// or a file that is only comments — are dropped rather than reported, since
/// they are normal in generated manifests.
pub fn documents(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(text) {
        match serde_yaml::Value::deserialize(doc) {
            Ok(serde_yaml::Value::Null) => {}
            Ok(v) => match serde_json::to_value(v) {
                Ok(Value::Null) => {}
                Ok(json) => out.push(json),
                Err(_) => {}
            },
            Err(_) => {}
        }
    }
    out
}

use serde::Deserialize;

/// `apiVersion` split into (group, version). `v1` is the core group, whose
/// name is the empty string — the same convention the discovery tables use.
pub fn group_version(api_version: &str) -> (String, String) {
    match api_version.split_once('/') {
        Some((g, v)) => (g.to_string(), v.to_string()),
        None => (String::new(), api_version.to_string()),
    }
}

/// The storage path and scope for a manifest's kind.
///
/// Built-in kinds come from the discovery table, so there is one list of what
/// this apiserver serves rather than two that can disagree. Custom kinds come
/// from the CRD registry, which is why a CRD in the same pass has to be
/// registered as it is applied.
async fn resolve(
    obj: &Value,
    crds: &CrdRegistry,
) -> Result<(String, bool), String> {
    let api_version = obj["apiVersion"].as_str().unwrap_or("");
    let kind = obj["kind"].as_str().unwrap_or("");
    if api_version.is_empty() || kind.is_empty() {
        return Err("manifest has no apiVersion or kind".to_string());
    }
    let (group, version) = group_version(api_version);

    if let Some((plural, _, namespaced)) = crate::discovery::resources_for(&group, &version)
        .into_iter()
        .find(|(_, k, _)| *k == kind)
    {
        return Ok((plural.to_string(), namespaced));
    }
    if let Some((plural, namespaced)) = crds.resource_for_kind(&group, &version, kind).await {
        return Ok((plural, namespaced));
    }
    Err(format!("{api_version} {kind} is not served by this apiserver"))
}

/// Apply one manifest document.
pub async fn apply_one(
    storage: &ResourceStorage,
    crds: &CrdRegistry,
    mut obj: Value,
) -> Outcome {
    let (plural, namespaced) = match resolve(&obj, crds).await {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(e),
    };
    let Some(name) = obj["metadata"]["name"].as_str().map(str::to_owned) else {
        return Outcome::Failed("manifest has no metadata.name".to_string());
    };

    // A namespaced object with no namespace goes to `default`, which is what
    // `kubectl apply -f` does with no `-n`.
    let namespace = obj["metadata"]["namespace"]
        .as_str()
        .unwrap_or("default")
        .to_string();
    let key = if namespaced {
        ResourceStorage::namespaced_key(&plural, &namespace, &name)
    } else {
        ResourceStorage::cluster_key(&plural, &name)
    };

    let mode = Mode::of(&obj);
    let existing = storage.get(&key).await.ok();

    match existing {
        None => {
            crate::handlers::resource::ensure_metadata_pub(
                &mut obj,
                &name,
                namespaced.then_some(namespace.as_str()),
            );
            // A CustomResourceDefinition has to register as it lands, or a
            // custom resource later in this same pass cannot be resolved.
            let is_crd = obj["kind"].as_str() == Some("CustomResourceDefinition");
            if is_crd {
                let mut with_status = obj.clone();
                crate::crd::establish_crd_status(&mut with_status);
                obj = with_status;
            }
            match storage.create(&key, obj.clone()).await {
                Ok(_) => {
                    if is_crd {
                        crds.register(&obj).await;
                    }
                    Outcome::Created
                }
                // Lost a race with another writer, which is the state we
                // wanted anyway.
                Err(e) if e.reason == "AlreadyExists" || e.reason == "Conflict" => {
                    Outcome::Unchanged
                }
                Err(e) => Outcome::Failed(e.message),
            }
        }
        Some(_) if mode == Mode::EnsureExists => Outcome::Unchanged,
        Some(current) => {
            // Reconcile: the manifest wins, but identity does not come from
            // the manifest. uid and creationTimestamp belong to the object
            // that already exists, and a new uid would make every controller
            // treat it as a different object.
            for field in ["uid", "creationTimestamp"] {
                if let Some(v) = current["metadata"].get(field) {
                    obj["metadata"][field] = v.clone();
                }
            }
            crate::handlers::resource::ensure_metadata_pub(
                &mut obj,
                &name,
                namespaced.then_some(namespace.as_str()),
            );
            // status is the cluster's, not the manifest's. A DaemonSet
            // manifest that carried an empty status would otherwise blank out
            // what the controller had recorded.
            if let Some(st) = current.get("status") {
                obj["status"] = st.clone();
            }
            match storage.update(&key, obj, None).await {
                Ok(_) => Outcome::Updated,
                Err(e) => Outcome::Failed(e.message),
            }
        }
    }
}

/// The manifest files in a directory, in the order they must be applied.
///
/// Sorted by filename, which is how the shipped manifests encode ordering.
/// Subdirectories are not descended: a flat directory keeps the order visible
/// in one listing.
pub fn manifest_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("yaml") | Some("yml") | Some("json")
            )
        })
        .collect();
    files.sort();
    files
}

/// Apply every manifest in a directory.
///
/// Never fatal. A directory that does not exist is "nothing to apply", which
/// is the normal case for a node that ships no addons; a manifest that cannot
/// be applied is logged and the rest continue, because one bad addon must not
/// keep an apiserver from serving.
pub async fn apply_dir(storage: &ResourceStorage, crds: &CrdRegistry, dir: &Path) {
    let files = manifest_files(dir);
    if files.is_empty() {
        tracing::debug!("manifests: nothing to apply in {}", dir.display());
        return;
    }
    let (mut created, mut updated, mut unchanged, mut failed) = (0, 0, 0, 0);
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            tracing::error!("manifests: cannot read {}", path.display());
            failed += 1;
            continue;
        };
        for obj in documents(&text) {
            let what = format!(
                "{} {}",
                obj["kind"].as_str().unwrap_or("?"),
                obj["metadata"]["name"].as_str().unwrap_or("?")
            );
            match apply_one(storage, crds, obj).await {
                Outcome::Created => {
                    tracing::info!("manifests: created {what}");
                    created += 1;
                }
                Outcome::Updated => {
                    tracing::info!("manifests: reconciled {what}");
                    updated += 1;
                }
                Outcome::Unchanged => unchanged += 1,
                Outcome::Failed(why) => {
                    tracing::error!(
                        "manifests: {what} from {} was not applied: {why}",
                        path.display()
                    );
                    failed += 1;
                }
            }
        }
    }
    tracing::info!(
        "manifests: {} file(s) from {} — {created} created, {updated} reconciled, \
         {unchanged} already present, {failed} failed",
        files.len(),
        dir.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_multi_document_file_is_split_and_empties_dropped() {
        let text = "\
apiVersion: v1
kind: Namespace
metadata:
  name: a
---
# just a comment
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: b
---
";
        let docs = documents(text);
        assert_eq!(docs.len(), 2, "{docs:?}");
        assert_eq!(docs[0]["kind"], "Namespace");
        assert_eq!(docs[1]["kind"], "DaemonSet");
    }

    #[test]
    fn json_is_one_document() {
        let docs = documents(r#"{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"a"}}"#);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["metadata"]["name"], "a");
    }

    /// The core group's name is the empty string, and getting that wrong makes
    /// every `v1` object unresolvable.
    #[test]
    fn the_core_group_is_the_empty_string() {
        assert_eq!(group_version("v1"), (String::new(), "v1".to_string()));
        assert_eq!(
            group_version("apps/v1"),
            ("apps".to_string(), "v1".to_string())
        );
        assert_eq!(
            group_version("cilium.io/v2alpha1"),
            ("cilium.io".to_string(), "v2alpha1".to_string())
        );
    }

    /// EnsureExists is the default, because an operator's edit to a live
    /// object must survive an apiserver restart.
    #[test]
    fn ensure_exists_is_the_default_and_reconcile_is_opt_in() {
        assert_eq!(Mode::of(&json!({"metadata": {}})), Mode::EnsureExists);
        assert_eq!(
            Mode::of(&json!({"metadata": {"annotations": {
                "addonmanager.kubernetes.io/mode": "Reconcile"}}})),
            Mode::Reconcile
        );
        assert_eq!(
            Mode::of(&json!({"metadata": {"annotations": {
                "storm.io/manifest-mode": "Reconcile"}}})),
            Mode::Reconcile
        );
        // An unrecognised value is the safe one, not an error.
        assert_eq!(
            Mode::of(&json!({"metadata": {"annotations": {
                "addonmanager.kubernetes.io/mode": "Nonsense"}}})),
            Mode::EnsureExists
        );
    }

    /// Filename order is the ordering contract, so 10- sorts before 20- and a
    /// non-manifest file is ignored.
    #[test]
    fn files_are_applied_in_filename_order() {
        let dir = std::env::temp_dir().join(format!("rk-manifests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["20-b.yaml", "10-a.yaml", "notes.txt", "30-c.json"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let names: Vec<String> = manifest_files(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["10-a.yaml", "20-b.yaml", "30-c.json"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A directory that is not there is "nothing to apply", not a failure —
    /// most nodes ship no addons.
    #[test]
    fn a_missing_directory_is_empty_not_an_error() {
        assert!(manifest_files(Path::new("/nonexistent/manifests")).is_empty());
    }
}
