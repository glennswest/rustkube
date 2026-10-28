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
//!
//! **What the datastore refused is tried again (#293).** The pass runs once,
//! but a document that failed because the datastore could not be read or
//! written — or whose kind is not served yet, because its CRD failed the same
//! way — is not lost with that boot: it is retried in the background, in
//! filename order, until it applies (a kind that stays unserved is given up
//! after ten minutes). A document the API itself refuses is not retried.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
    /// The datastore could not be read or written (#293): nothing is known
    /// about the object, and trying again later is the answer.
    Unavailable(String),
    /// The kind is not served (yet): its CRD may still arrive — later in the
    /// retries, or through another apiserver.
    NotServed(String),
}

impl Outcome {
    /// Worth another try later.
    fn retryable(&self) -> bool {
        matches!(self, Outcome::Unavailable(_) | Outcome::NotServed(_))
    }
}

/// The outcome of a failed store call or admission: a datastore that cannot
/// serve is `Unavailable`, anything else the API's own refusal.
fn failed(e: crate::error::ApiError) -> Outcome {
    if store_trouble(&e) {
        Outcome::Unavailable(e.message)
    } else {
        Outcome::Failed(e.message)
    }
}

/// A datastore that cannot serve right now (503), or one that answered with
/// an error of its own (500) — neither says anything about the manifest.
fn store_trouble(e: &crate::error::ApiError) -> bool {
    e.reason == "ServiceUnavailable" || e.reason == "InternalError"
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

/// The storage resource (the key segment) and scope for a manifest's kind.
///
/// Built-in kinds come from the discovery table, so there is one list of what
/// this apiserver serves rather than two that can disagree. Custom kinds come
/// from the CRD registry, which is why a CRD in the same pass has to be
/// registered as it is applied.
/// Where a manifest's object is stored: the storage resource, whether it is
/// namespaced, and for a custom resource its `(group, version, plural)`.
struct Target {
    resource: String,
    namespaced: bool,
    custom: Option<(String, String, String)>,
}

async fn resolve(obj: &Value, crds: &CrdRegistry) -> Result<Target, Outcome> {
    let api_version = obj["apiVersion"].as_str().unwrap_or("");
    let kind = obj["kind"].as_str().unwrap_or("");
    if api_version.is_empty() || kind.is_empty() {
        return Err(Outcome::Failed("manifest has no apiVersion or kind".to_string()));
    }
    let (group, version) = group_version(api_version);

    if let Some((plural, _, namespaced)) = crate::discovery::resources_for(&group, &version)
        .into_iter()
        .find(|(_, k, _)| *k == kind)
    {
        return Ok(Target { resource: plural.to_string(), namespaced, custom: None });
    }
    // A custom kind is stored under its group, as the CRD handlers store it
    // (#76).
    if let Some((plural, namespaced)) = crds.resource_for_kind(&group, &version, kind).await {
        return Ok(Target {
            resource: ResourceStorage::custom_resource(&group, &plural),
            namespaced,
            custom: Some((group, version, plural)),
        });
    }
    Err(Outcome::NotServed(format!("{api_version} {kind} is not served by this apiserver")))
}

/// Apply one manifest document.
///
/// **What the API would do, without the caller (#158).** A create goes
/// through the same built-in admission as a create through the API
/// (`builtin_admission::admit_create`): NamespaceLifecycle, a Namespace's
/// defaults, a Service's ClusterIP and node ports, a Pod's ServiceAccount,
/// token volume, tolerations, priority, LimitRanger, QoS, phase and
/// PodSecurity, a Secret's stringData, the validations — or, for a custom
/// resource, its CRD's strategy (schema defaults and pruning, status,
/// `generation`). A Reconcile update goes through the update checks PUT and
/// PATCH run (`resource::builtin_update`) or the CRD's update strategy.
/// What a manifest is not put through is what belongs to a requester: RBAC
/// and its escalation check, admission webhooks (their servers are not up
/// this early), and the requester stamp — these are the apiserver's own
/// writes.
pub async fn apply_one(
    storage: &ResourceStorage,
    crds: &CrdRegistry,
    service_cidr: &str,
    mut obj: Value,
) -> Outcome {
    let Target { resource, namespaced, custom } = match resolve(&obj, crds).await {
        Ok(r) => r,
        Err(outcome) => return outcome,
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
    let ns = namespaced.then_some(namespace.as_str());
    let key = if namespaced {
        ResourceStorage::namespaced_key(&resource, &namespace, &name)
    } else {
        ResourceStorage::cluster_key(&resource, &name)
    };
    let strategy = match &custom {
        Some((g, v, plural)) => crds.strategy(g, v, plural).await,
        None => crate::handlers::resource::Strategy::BuiltIn,
    };

    let mode = Mode::of(&obj);
    // Only NotFound means absent. Any other error says nothing about the
    // object, and reading it as absent sent server1's whole pass into creates
    // against a datastore that was not answering (#293).
    let existing = match storage.get(&key).await {
        Ok(current) => Some(current),
        Err(e) if e.reason == "NotFound" => None,
        Err(e) => return Outcome::Unavailable(e.message),
    };

    match existing {
        None => {
            crate::handlers::resource::ensure_metadata_pub(&mut obj, &name, ns);
            // A CustomResourceDefinition has to register as it lands, or a
            // custom resource later in this same pass cannot be resolved.
            let is_crd = obj["kind"].as_str() == Some("CustomResourceDefinition");
            if is_crd {
                crate::crd::establish_crd_status(&mut obj);
            }
            let admitted = match &custom {
                Some(_) => strategy.on_create(&mut obj),
                None => crate::builtin_admission::admit_create(storage, &resource, ns, &mut obj, service_cidr).await,
            };
            if let Err(e) = admitted {
                // Another apiserver applying the same manifest got there
                // first (its Service holds the address this one asked for):
                // the state wanted anyway.
                if storage.get(&key).await.is_ok() {
                    return Outcome::Unchanged;
                }
                return failed(e);
            }
            crate::handlers::resource::ensure_metadata_pub(&mut obj, &name, ns);
            if custom.is_none() {
                crate::generation::on_create(&resource, &mut obj); // #285
            }
            let is_service = resource == "services";
            match storage.create(&key, obj.clone()).await {
                Ok(_) => {
                    if is_crd {
                        crds.register(&obj).await;
                    }
                    Outcome::Created
                }
                Err(e) => {
                    // What admission claimed for a Service that was not
                    // written goes back, as on the API's create (#132).
                    if is_service {
                        crate::node_port::release_all(storage, &obj).await;
                    }
                    // Lost a race with another writer, which is the state
                    // we wanted anyway.
                    if e.reason == "AlreadyExists" || e.reason == "Conflict" {
                        Outcome::Unchanged
                    } else {
                        failed(e)
                    }
                }
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
            crate::handlers::resource::ensure_metadata_pub(&mut obj, &name, ns);
            // status is the cluster's, not the manifest's. A DaemonSet
            // manifest that carried an empty status would otherwise blank out
            // what the controller had recorded.
            if let Some(st) = current.get("status") {
                obj["status"] = st.clone();
            }
            // The update checks PUT and PATCH run (#158).
            let plan = match &custom {
                Some(_) => strategy.on_update(&mut obj, &current).map(|_| None),
                None => crate::handlers::resource::builtin_update(storage, service_cidr, &key, &current, &mut obj).await,
            };
            let plan = match plan {
                Ok(p) => p,
                Err(e) => return failed(e),
            };
            // A manifest the stored object already matches is not written
            // again (#290): every boot rewrote every Reconcile manifest, about
            // twenty fsyncs of 1–2 s each on server1's spinning disk.
            if same_object(&current, &obj) {
                if let Some(plan) = plan {
                    plan.abort(storage).await;
                }
                return Outcome::Unchanged;
            }
            let is_crd = obj["kind"].as_str() == Some("CustomResourceDefinition");
            let written = storage.update(&key, obj, None).await;
            if let Some(plan) = plan {
                if written.is_ok() { plan.commit(storage).await } else { plan.abort(storage).await }
            }
            match written {
                // A reconciled CRD may serve new versions; register what was
                // stored, as the API's update path does.
                Ok(stored) => {
                    if is_crd {
                        crds.register(&stored).await;
                    }
                    Outcome::Updated
                }
                Err(e) => failed(e),
            }
        }
    }
}

/// Whether writing `next` over `current` would change nothing: equal but for
/// `metadata.resourceVersion`, which only the store sets.
fn same_object(current: &Value, next: &Value) -> bool {
    let without_rv = |v: &Value| {
        let mut v = v.clone();
        if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.remove("resourceVersion");
        }
        v
    };
    without_rv(current) == without_rv(next)
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

/// One manifest document, and the file it came from.
struct Document {
    path: PathBuf,
    obj: Value,
}

impl Document {
    fn what(&self) -> String {
        format!(
            "{} {}",
            self.obj["kind"].as_str().unwrap_or("?"),
            self.obj["metadata"]["name"].as_str().unwrap_or("?")
        )
    }
}

/// What one pass did: the counts, and the documents worth another try.
#[derive(Default)]
struct Pass {
    created: usize,
    updated: usize,
    unchanged: usize,
    failed: usize,
    unavailable: usize,
    not_served: usize,
    retry: Vec<Document>,
}

impl Pass {
    fn not_applied(&self) -> usize {
        self.failed + self.unavailable + self.not_served
    }

    /// The failures by cause, for the summary line.
    fn causes(&self) -> String {
        let mut why = Vec::new();
        if self.unavailable > 0 {
            why.push(format!("{} datastore unavailable", self.unavailable));
        }
        if self.not_served > 0 {
            why.push(format!("{} kind not served", self.not_served));
        }
        if self.failed > 0 {
            why.push(format!("{} refused", self.failed));
        }
        why.join(", ")
    }
}

/// Apply documents in order, logging each one's outcome.
///
/// A failure the retries take up is a WARN (they say when it lands); one they
/// do not is an ERROR, as before.
async fn apply_all(
    storage: &ResourceStorage,
    crds: &CrdRegistry,
    service_cidr: &str,
    docs: Vec<Document>,
) -> Pass {
    let mut pass = Pass::default();
    for doc in docs {
        let what = doc.what();
        let outcome = apply_one(storage, crds, service_cidr, doc.obj.clone()).await;
        match &outcome {
            Outcome::Created => {
                tracing::info!("manifests: created {what}");
                pass.created += 1;
            }
            Outcome::Updated => {
                tracing::info!("manifests: reconciled {what}");
                pass.updated += 1;
            }
            Outcome::Unchanged => pass.unchanged += 1,
            Outcome::Failed(why) => {
                tracing::error!("manifests: {what} from {} was not applied: {why}", doc.path.display());
                pass.failed += 1;
            }
            Outcome::Unavailable(why) | Outcome::NotServed(why) => {
                tracing::warn!(
                    "manifests: {what} from {} was not applied: {why}; will retry",
                    doc.path.display()
                );
                if matches!(outcome, Outcome::Unavailable(_)) {
                    pass.unavailable += 1;
                } else {
                    pass.not_served += 1;
                }
            }
        }
        if outcome.retryable() {
            pass.retry.push(doc);
        }
    }
    pass
}

/// Apply every manifest in a directory, then retry in the background what
/// the datastore refused (#293).
///
/// Never fatal. A directory that does not exist is "nothing to apply", which
/// is the normal case for a node that ships no addons; a manifest that cannot
/// be applied is logged and the rest continue, because one bad addon must not
/// keep an apiserver from serving.
pub async fn apply_dir(
    storage: &Arc<ResourceStorage>,
    crds: &Arc<CrdRegistry>,
    service_cidr: &str,
    dir: &Path,
) {
    let files = manifest_files(dir);
    if files.is_empty() {
        tracing::debug!("manifests: nothing to apply in {}", dir.display());
        return;
    }
    let mut docs = Vec::new();
    let mut unreadable = 0;
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            tracing::error!("manifests: cannot read {}", path.display());
            unreadable += 1;
            continue;
        };
        docs.extend(documents(&text).into_iter().map(|obj| Document { path: path.clone(), obj }));
    }
    let mut pass = apply_all(storage, crds, service_cidr, docs).await;
    pass.failed += unreadable;
    let summary = format!(
        "manifests: {} file(s) from {} — {} created, {} reconciled, {} already present, {} failed",
        files.len(),
        dir.display(),
        pass.created,
        pass.updated,
        pass.unchanged,
        pass.not_applied(),
    );
    if pass.not_applied() == 0 {
        tracing::info!("{summary}");
    } else if pass.retry.is_empty() {
        tracing::error!("{summary} ({})", pass.causes());
    } else {
        tracing::warn!("{summary} ({}); retrying {} in the background", pass.causes(), pass.retry.len());
        tokio::spawn(retry(storage.clone(), crds.clone(), service_cidr.to_string(), pass.retry));
    }
}

/// How long a kind may stay unserved before its documents are given up.
const NOT_SERVED_FOR: Duration = Duration::from_secs(600);

/// Retry, in filename order, what the first pass could not apply for the
/// datastore's sake or for a kind not yet served, until it applies (#293).
///
/// Backoff 1 s doubling to 30 s. A datastore failure is retried for as long
/// as it lasts: these are the objects this node ships, and an apiserver
/// cannot do without its store anyway. Once only unserved kinds are left and
/// [`NOT_SERVED_FOR`] has passed, they are given up at ERROR — their CRD is
/// not coming.
async fn retry(
    storage: Arc<ResourceStorage>,
    crds: Arc<CrdRegistry>,
    service_cidr: String,
    mut pending: Vec<Document>,
) {
    let start = tokio::time::Instant::now();
    let mut delay = Duration::from_secs(1);
    let mut round = 0;
    while !pending.is_empty() {
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
        round += 1;
        let tried = pending.len();
        let pass = apply_all(&storage, &crds, &service_cidr, std::mem::take(&mut pending)).await;
        let applied = pass.created + pass.updated + pass.unchanged;
        if applied > 0 || pass.failed > 0 {
            let left = if pass.retry.is_empty() {
                String::new()
            } else {
                format!("; {} left ({})", pass.retry.len(), pass.causes())
            };
            tracing::info!(
                "manifests: retry {round}: {applied} of {tried} applied ({} created, {} reconciled, \
                 {} already present){left}",
                pass.created,
                pass.updated,
                pass.unchanged,
            );
        }
        pending = pass.retry;
        if pass.unavailable == 0 && !pending.is_empty() && start.elapsed() >= NOT_SERVED_FOR {
            for doc in &pending {
                tracing::error!(
                    "manifests: {} from {} given up after {NOT_SERVED_FOR:?}: its kind is still not served",
                    doc.what(),
                    doc.path.display()
                );
            }
            return;
        }
    }
    tracing::info!("manifests: every retried manifest applied, {:?} after the first pass", start.elapsed());
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

    fn cluster_role(name: &str) -> Value {
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
               "metadata": {"name": name}, "rules": []})
    }

    /// A datastore that does not answer is not "the object is absent" (#293):
    /// server1's pass created nothing and failed 114 objects that existed.
    #[tokio::test]
    async fn an_unreadable_store_is_unavailable_not_absent() {
        let store = Arc::new(crate::test_store::MemStore::default());
        let storage = ResourceStorage::new(store.clone());
        let crds = CrdRegistry::new();

        store.set_unavailable(true);
        let got = apply_one(&storage, &crds, "10.96.0.0/12", cluster_role("a")).await;
        assert!(matches!(got, Outcome::Unavailable(_)), "{got:?}");
        assert!(got.retryable());

        store.set_unavailable(false);
        assert_eq!(apply_one(&storage, &crds, "10.96.0.0/12", cluster_role("a")).await, Outcome::Created);
        assert_eq!(apply_one(&storage, &crds, "10.96.0.0/12", cluster_role("a")).await, Outcome::Unchanged);

        // A kind not served may yet be (its CRD); a nameless one never will.
        let cr = json!({"apiVersion": "cilium.io/v2alpha1", "kind": "CiliumL2AnnouncementPolicy",
                        "metadata": {"name": "default"}});
        let got = apply_one(&storage, &crds, "10.96.0.0/12", cr).await;
        assert!(matches!(got, Outcome::NotServed(_)) && got.retryable(), "{got:?}");
        let got = apply_one(&storage, &crds, "10.96.0.0/12", json!({"apiVersion": "v1", "kind": "ConfigMap",
                                                                     "metadata": {}})).await;
        assert!(matches!(got, Outcome::Failed(_)) && !got.retryable(), "{got:?}");
    }

    fn reconciled(mut obj: Value) -> Value {
        obj["metadata"]["annotations"] = json!({"addonmanager.kubernetes.io/mode": "Reconcile"});
        obj
    }

    async fn resource_version(storage: &ResourceStorage, key: &str) -> Value {
        storage.get(key).await.unwrap()["metadata"]["resourceVersion"].clone()
    }

    /// A Reconcile manifest the stored object already matches is not written
    /// again (#290): server1 rewrote about twenty on every boot, 1–2 s an
    /// fsync on its spinning disk. A changed one still is.
    #[tokio::test]
    async fn an_unchanged_reconcile_manifest_is_not_rewritten() {
        let store = Arc::new(crate::test_store::MemStore::default());
        let storage = ResourceStorage::new(store.clone());
        let crds = CrdRegistry::new();
        let cidr = "10.96.0.0/12";
        let ns = "cilium-secrets";
        // The kinds server1's pass reconciled, the Namespace first.
        let docs = [
            (json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}}),
             ResourceStorage::cluster_key("namespaces", ns)),
            (json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": "cilium", "namespace": ns}}),
             ResourceStorage::namespaced_key("serviceaccounts", ns, "cilium")),
            (json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg", "namespace": ns},
                    "data": {"a": "1"}}),
             ResourceStorage::namespaced_key("configmaps", ns, "cfg")),
            (cluster_role("cilium"), ResourceStorage::cluster_key("clusterroles", "cilium")),
            (json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
                    "metadata": {"name": "rb", "namespace": ns},
                    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "cilium"},
                    "subjects": [{"kind": "ServiceAccount", "name": "cilium", "namespace": ns}]}),
             ResourceStorage::namespaced_key("rolebindings", ns, "rb")),
            (json!({"apiVersion": "storage.k8s.io/v1", "kind": "CSIDriver",
                    "metadata": {"name": "stormblock.storm.io"}, "spec": {"attachRequired": false}}),
             ResourceStorage::cluster_key("csidrivers", "stormblock.storm.io")),
            (json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "kube-dns", "namespace": ns},
                    "spec": {"selector": {"k": "v"}, "ports": [{"name": "dns", "port": 53, "protocol": "UDP"}]}}),
             ResourceStorage::namespaced_key("services", ns, "kube-dns")),
        ];
        let mut versions = Vec::new();
        for (doc, key) in &docs {
            assert_eq!(apply_one(&storage, &crds, cidr, reconciled(doc.clone())).await, Outcome::Created, "{key}");
            versions.push(resource_version(&storage, key).await);
        }
        // The next boot's pass.
        for ((doc, key), rv) in docs.iter().zip(&versions) {
            assert_eq!(apply_one(&storage, &crds, cidr, reconciled(doc.clone())).await, Outcome::Unchanged, "{key}");
            assert_eq!(&resource_version(&storage, key).await, rv, "{key} was written again");
        }
        // The Namespace kept the finalizer its create defaulted.
        let stored = storage.get(&docs[0].1).await.unwrap();
        assert_eq!(stored["spec"]["finalizers"], json!(["kubernetes"]));

        // A manifest that changed lands.
        let mut cm = docs[2].0.clone();
        cm["data"]["a"] = json!("2");
        assert_eq!(apply_one(&storage, &crds, cidr, reconciled(cm)).await, Outcome::Updated);
        assert_eq!(storage.get(&docs[2].1).await.unwrap()["data"]["a"], "2");
    }

    /// What the datastore refused at boot lands once it answers, without a
    /// restart (#293).
    #[tokio::test(start_paused = true)]
    async fn refused_documents_are_retried_until_they_apply() {
        let dir = std::env::temp_dir().join(format!("rk-manifests-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("10-roles.yaml"),
            "apiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRole\nmetadata:\n  name: a\n---\n\
             apiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRole\nmetadata:\n  name: b\n",
        )
        .unwrap();
        let store = Arc::new(crate::test_store::MemStore::default());
        let storage = Arc::new(ResourceStorage::new(store.clone()));
        let crds = Arc::new(CrdRegistry::new());

        store.set_unavailable(true);
        apply_dir(&storage, &crds, "10.96.0.0/12", &dir).await;
        // Still down through the first retries: nothing lost, nothing given up.
        tokio::time::sleep(Duration::from_secs(20)).await;
        store.set_unavailable(false);
        tokio::time::sleep(Duration::from_secs(60)).await;
        for name in ["a", "b"] {
            assert!(
                storage.get(&ResourceStorage::cluster_key("clusterroles", name)).await.is_ok(),
                "clusterrole {name} not applied by the retries"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
