//! OpenShift's `restricted-v2` on rustkube (#321): per-namespace UID and
//! supplemental-group ranges, and pod admission that runs a pod as a UID from
//! its namespace's range, so OpenShift manifests — no `runAsUser`, no
//! `fsGroup`, no pinned UID, no root — run unchanged.
//!
//! **Ranges.** Every Namespace carries OpenShift's annotations,
//!
//! - `openshift.io/sa.scc.uid-range`: `1000680000/10000`
//! - `openshift.io/sa.scc.supplemental-groups`: `1000680000/10000`
//!
//! the same block for both, from OpenShift's pool (`1000000000-1999999999`
//! in blocks of 10000). Blocks come from a cluster-wide allocator stored at
//! `/registry/rangeallocations/scc-uid` (OpenShift's `RangeAllocation`
//! `scc-uid`, kept here as a counter rather than a bitmap): it only counts up,
//! so a block is never handed to a second namespace, even after the first is
//! deleted. Each step is a compare-and-swap, so apiservers of a multi-master
//! cluster never hand out the same block. When the record is missing (a new
//! cluster, or a store it was lost from) it starts after the highest block any
//! namespace holds. A namespace gets its block when it is created (any path:
//! POST, apply, ProjectRequest), at boot for namespaces stored before this,
//! and at a pod's admission if it still has none. A namespace created with
//! the annotations already set (by someone allowed to create namespaces)
//! keeps them, as OpenShift's allocation controller does.
//!
//! **Which pods.** A namespace is governed unless it is a platform namespace
//! (`kube-*`, `openshift*`, `storm-*`, `*-system`) or carries
//! `pod-security.kubernetes.io/enforce: privileged`; an explicit `restricted`
//! or `baseline` label makes even a platform namespace governed. Mirror pods
//! (the kubelet's static pods) are never changed. Writes the apiserver makes
//! for itself (manifests) have no requester and are not governed.
//!
//! **Which SCC** (#373): the stored `security.openshift.io/v1`
//! SecurityContextConstraints, each made a provider for the pod's namespace
//! and tried in OpenShift's order (priority, then restrictiveness, then
//! name; [`crate::scc_policy`]). An SCC is usable when it lists the pod's
//! ServiceAccount or the requester (or one of their groups), or RBAC grants
//! `use` of it — the ServiceAccount is asked first, and the pod records which
//! one it was (`security.openshift.io/validated-scc-subject-type`). The first
//! usable SCC the pod satisfies admits it, with what that SCC fills in; a pod
//! naming `openshift.io/required-scc` is tried against that SCC alone.
//! rustkube ships `anyuid` (priority 10), `restricted-v2` and `privileged`,
//! created at boot when missing and otherwise left as an administrator made
//! them; `restricted-v2` is granted to `system:authenticated` by the
//! ClusterRoleBinding `system:openshift:scc:restricted-v2`. `cluster-admin`
//! and `system:masters` may use every SCC, so a pod an administrator creates
//! directly is admitted under `anyuid` (or `privileged`) unchanged but for
//! the dropped `MKNOD`, exactly as on OpenShift, while a pod a controller
//! creates for a Deployment, StatefulSet or Job gets `restricted-v2` unless
//! its ServiceAccount was granted more: a RoleBinding to
//! `system:openshift:scc:anyuid` (or `…:privileged`), the ClusterRoles
//! bootstrapped for it, or the SCC's own `users`/`groups`.
//!
//! **restricted-v2** (what it sets when the pod leaves it unset, and refuses
//! otherwise, in OpenShift's words):
//!
//! - each container's `runAsUser`: the first UID of the range
//!   (`MustRunAsRange`); a UID outside it is refused;
//! - each container's `runAsNonRoot: true`;
//! - the pod's `fsGroup`: the first group of the supplemental range, and only
//!   that one (`supplementalGroups` are `RunAsAny`);
//! - `allowPrivilegeEscalation: false`; `capabilities.drop` gains `ALL`; only
//!   `NET_BIND_SERVICE` may be added;
//! - the pod's `seccompProfile: RuntimeDefault` (and OpenShift's
//!   `seccomp.security.alpha.kubernetes.io/pod` annotation); any other
//!   profile is refused;
//! - no privileged containers, host network/PID/IPC, host ports, unsafe
//!   sysctls, and only configMap, csi, downwardAPI, emptyDir, ephemeral,
//!   image, persistentVolumeClaim, projected and secret volumes.
//!
//! The pod is annotated `openshift.io/scc: <provider>`. SELinux levels
//! (`openshift.io/sa.scc.mcs`) are not assigned: stormcos nodes do not run
//! SELinux. The kubelet applying `fsGroup` to a claim is rustkube-node#171.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::scc_policy::{self, Provider};
use crate::storage::ResourceStorage;
use serde_json::{json, Value};

pub const UID_RANGE: &str = "openshift.io/sa.scc.uid-range";
pub const SUPPLEMENTAL_GROUPS: &str = "openshift.io/sa.scc.supplemental-groups";
pub const SCC_ANNOTATION: &str = "openshift.io/scc";
const ENFORCE: &str = "pod-security.kubernetes.io/enforce";
const MIRROR: &str = "kubernetes.io/config.mirror";

/// OpenShift's pool: `1000000000-1999999999/10000`.
const POOL_START: u64 = 1_000_000_000;
const POOL_END: u64 = 1_999_999_999;
const BLOCK: u64 = 10_000;
const BLOCKS: u64 = (POOL_END - POOL_START + 1) / BLOCK;
pub const ALLOCATION_KEY: &str = "/registry/rangeallocations/scc-uid";

/// An inclusive ID range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRange {
    pub min: u64,
    pub max: u64,
}

impl IdRange {
    fn contains(&self, id: u64) -> bool {
        (self.min..=self.max).contains(&id)
    }
}

/// One range as OpenShift writes it: `start/size` or `start-end`.
pub fn parse_range(text: &str) -> Option<IdRange> {
    let text = text.trim();
    if let Some((start, size)) = text.split_once('/') {
        let (start, size): (u64, u64) = (start.trim().parse().ok()?, size.trim().parse().ok()?);
        if size == 0 {
            return None;
        }
        Some(IdRange { min: start, max: start.checked_add(size - 1)? })
    } else if let Some((start, end)) = text.split_once('-') {
        let (min, max): (u64, u64) = (start.trim().parse().ok()?, end.trim().parse().ok()?);
        (min <= max).then_some(IdRange { min, max })
    } else {
        None
    }
}

/// A comma-separated list of ranges (the supplemental-groups annotation may
/// hold several); `None` unless every entry parses.
pub fn parse_ranges(text: &str) -> Option<Vec<IdRange>> {
    let ranges: Option<Vec<IdRange>> = text.split(',').map(parse_range).collect();
    ranges.filter(|r| !r.is_empty())
}

/// The namespace's UID range and supplemental-group ranges, if it has both.
pub fn namespace_ranges(ns: &Value) -> Option<(IdRange, Vec<IdRange>)> {
    let a = &ns["metadata"]["annotations"];
    let uid = parse_ranges(a[UID_RANGE].as_str()?)?;
    let groups = parse_ranges(a[SUPPLEMENTAL_GROUPS].as_str()?)?;
    Some((uid[0], groups))
}

fn block_text(block: u64) -> String {
    format!("{}/{BLOCK}", POOL_START + block * BLOCK)
}

/// The pool block a namespace's UID range starts in, if it is one of ours.
fn block_of(ns: &Value) -> Option<u64> {
    let (uid, _) = namespace_ranges(ns)?;
    if (POOL_START..=POOL_END).contains(&uid.min) {
        Some((uid.min - POOL_START) / BLOCK)
    } else {
        None
    }
}

/// The allocation record: `next`, the first block never handed out, and
/// `released`, blocks handed out to a create that then failed before the
/// namespace was stored (#377) — never on a namespace, so free to give again.
fn allocation_record(next: u64, released: &[u64]) -> Value {
    json!({
        "apiVersion": "security.openshift.io/v1",
        "kind": "RangeAllocation",
        "metadata": {"name": "scc-uid"},
        "range": format!("{POOL_START}-{POOL_END}/{BLOCK}"),
        "next": next,
        "released": released,
    })
}

fn released_of(rec: &Value) -> Vec<u64> {
    rec["released"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default()
}

/// Take a block from the cluster-wide allocator: a released one first, else
/// the next never handed out. A block once stored on a namespace is never
/// handed out again.
pub async fn allocate_block(storage: &ResourceStorage) -> Result<u64, ApiError> {
    for _ in 0..32 {
        let (next, mut released, rev) = match storage.get(ALLOCATION_KEY).await {
            Ok(rec) => (
                rec["next"].as_u64().unwrap_or(0),
                released_of(&rec),
                rec["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse::<u64>().ok()),
            ),
            Err(e) if e.status == axum::http::StatusCode::NOT_FOUND => {
                (first_free_block(storage).await?, Vec::new(), None)
            }
            Err(e) => return Err(e),
        };
        let (block, next) = if !released.is_empty() {
            (released.remove(0), next)
        } else if next >= BLOCKS {
            return Err(ApiError::internal(&format!(
                "the SCC UID pool {POOL_START}-{POOL_END}/{BLOCK} is exhausted: every block has been given to a namespace"
            )));
        } else {
            (next, next + 1)
        };
        let rec = allocation_record(next, &released);
        let written = match rev {
            Some(rev) => storage.update(ALLOCATION_KEY, rec, Some(rev)).await,
            None => storage.create(ALLOCATION_KEY, rec).await,
        };
        match written {
            Ok(_) => return Ok(block),
            Err(e) if e.reason == "Conflict" || e.reason == "AlreadyExists" => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::internal("SCC UID allocation: too many concurrent allocations, try again"))
}

/// Give back `block`, taken for a namespace create that failed before the
/// namespace was stored (#377): `next` steps back when it was the last one
/// handed out, else it joins `released`. Best effort — a block that cannot be
/// given back is one block, not a leak a client can repeat.
pub async fn release_block(storage: &ResourceStorage, block: u64) {
    for _ in 0..32 {
        let rec = match storage.get(ALLOCATION_KEY).await {
            Ok(rec) => rec,
            Err(e) => {
                tracing::warn!("SCC UID block {block} not given back: {}", e.message);
                return;
            }
        };
        let rev = rec["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse::<u64>().ok());
        let mut next = rec["next"].as_u64().unwrap_or(0);
        let mut released = released_of(&rec);
        if block >= next || released.contains(&block) {
            return; // never handed out, or already back
        }
        if block + 1 == next {
            next = block;
        } else {
            released.push(block);
            released.sort_unstable();
        }
        // The top of the released list joins `next` as it steps back.
        while released.last().is_some_and(|b| b + 1 == next) {
            next -= 1;
            released.pop();
        }
        match storage.update(ALLOCATION_KEY, allocation_record(next, &released), rev).await {
            Ok(_) => return,
            Err(e) if e.reason == "Conflict" => continue,
            Err(e) => {
                tracing::warn!("SCC UID block {block} not given back: {}", e.message);
                return;
            }
        }
    }
    tracing::warn!("SCC UID block {block} not given back: the allocation record kept changing");
}

/// A namespace create that took `block` failed with `err`: give the block
/// back unless the namespace may have been written. A 4xx — an admission
/// refusal, or the name taken (AlreadyExists) — wrote nothing; a 5xx from
/// the datastore may have, and keeps its block.
pub async fn create_failed(storage: &ResourceStorage, block: Option<u64>, err: &ApiError) {
    if let Some(block) = block {
        if err.status.is_client_error() {
            release_block(storage, block).await;
        }
    }
}

/// With no allocation record: the block after the highest any namespace holds.
async fn first_free_block(storage: &ResourceStorage) -> Result<u64, ApiError> {
    let prefix = ResourceStorage::cluster_prefix("namespaces");
    let mut token: Option<String> = None;
    let mut next = 0;
    loop {
        let (items, more, _) = storage.list(&prefix, 500, token.as_deref()).await?;
        for ns in &items {
            if let Some(b) = block_of(ns) {
                next = next.max(b + 1);
            }
        }
        match more {
            Some(t) => token = Some(t),
            None => return Ok(next),
        }
    }
}

/// Give `ns` (a Namespace about to be written) its ranges, unless it has
/// valid ones. The block taken, when one was — the caller gives it back
/// with `create_failed` if the namespace is not then written.
pub async fn ensure_ranges(storage: &ResourceStorage, ns: &mut Value) -> Result<Option<u64>, ApiError> {
    if namespace_ranges(ns).is_some() {
        return Ok(None);
    }
    let block = allocate_block(storage).await?;
    set_ranges(ns, block);
    Ok(Some(block))
}

/// A Namespace create's ranges (#321), refused first when the name is taken
/// (#377): every `oc new-project` of an existing name used to spend a block
/// on a 409, and the pool is finite.
pub async fn ranges_for_new_namespace(storage: &ResourceStorage, ns: &mut Value) -> Result<Option<u64>, ApiError> {
    if let Some(name) = ns["metadata"]["name"].as_str() {
        match storage.get(&ResourceStorage::cluster_key("namespaces", name)).await {
            Ok(_) => return Err(ApiError::already_exists("namespaces", name)),
            Err(e) if e.status == axum::http::StatusCode::NOT_FOUND => {}
            Err(e) => return Err(e),
        }
    }
    ensure_ranges(storage, ns).await
}

fn set_ranges(ns: &mut Value, block: u64) {
    if !ns["metadata"].is_object() {
        ns["metadata"] = json!({});
    }
    if !ns["metadata"]["annotations"].is_object() {
        ns["metadata"]["annotations"] = json!({});
    }
    let text = block_text(block);
    ns["metadata"]["annotations"][UID_RANGE] = json!(text);
    ns["metadata"]["annotations"][SUPPLEMENTAL_GROUPS] = json!(text);
}

/// A Namespace update keeps the stored ranges unless it sets valid ones of
/// its own: a PUT or a reconciled manifest that does not mention them must
/// not strip them, or the namespace's next pod would get a new block and
/// lose the files its volumes hold under the old UIDs.
pub fn keep_ranges(existing: &Value, body: &mut Value) {
    if namespace_ranges(body).is_some() {
        return;
    }
    let stored = &existing["metadata"]["annotations"];
    if stored[UID_RANGE].is_null() && stored[SUPPLEMENTAL_GROUPS].is_null() {
        return;
    }
    if !body["metadata"].is_object() {
        body["metadata"] = json!({});
    }
    if !body["metadata"]["annotations"].is_object() {
        body["metadata"]["annotations"] = json!({});
    }
    for k in [UID_RANGE, SUPPLEMENTAL_GROUPS] {
        match stored.get(k) {
            Some(v) => body["metadata"]["annotations"][k] = v.clone(),
            None => {
                if let Some(a) = body["metadata"]["annotations"].as_object_mut() {
                    a.remove(k);
                }
            }
        }
    }
}

/// The stored namespace `name` with ranges, writing them if it had none
/// (a namespace stored before #321 whose boot backfill has not reached it).
pub async fn namespace_with_ranges(storage: &ResourceStorage, mut ns: Value) -> Result<Value, ApiError> {
    for _ in 0..8 {
        if namespace_ranges(&ns).is_some() {
            return Ok(ns);
        }
        let name = ns["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let key = ResourceStorage::cluster_key("namespaces", &name);
        let rev = ns["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
        let mut updated = ns.clone();
        let block = ensure_ranges(storage, &mut updated).await?;
        match storage.update(&key, updated, rev).await {
            Ok(written) => return Ok(written),
            Err(e) if e.reason == "Conflict" => {
                // Not written: the block goes back (#377).
                create_failed(storage, block, &e).await;
                ns = storage.get(&key).await?
            }
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::internal("namespace UID range: the namespace kept changing, try again"))
}

/// Give every stored namespace without ranges its block, once at boot.
/// Idempotent under HA: each write is a CAS, and a namespace that gained
/// ranges meanwhile is left alone (the block taken for it stays unused).
pub async fn backfill(storage: &ResourceStorage) {
    let prefix = ResourceStorage::cluster_prefix("namespaces");
    let mut token: Option<String> = None;
    let mut fixed = 0usize;
    loop {
        let (items, next, _) = match storage.list(&prefix, 500, token.as_deref()).await {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!("namespace UID range backfill: list failed: {}", e.message);
                return;
            }
        };
        for ns in items {
            if namespace_ranges(&ns).is_some() || ns["metadata"]["deletionTimestamp"].is_string() {
                continue;
            }
            let name = ns["metadata"]["name"].as_str().unwrap_or_default().to_string();
            match namespace_with_ranges(storage, ns).await {
                Ok(_) => fixed += 1,
                Err(e) => tracing::warn!("namespace UID range backfill: {name}: {}", e.message),
            }
        }
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    if fixed > 0 {
        tracing::info!("namespace UID range backfill: {fixed} namespace(s) given a UID and supplemental-group range");
    }
}

/// A platform namespace: SCCs do not govern its pods unless its
/// pod-security label asks for `restricted` or `baseline`.
pub fn is_platform(name: &str) -> bool {
    name.starts_with("kube-") || name.starts_with("openshift") || name.starts_with("storm-") || name.ends_with("-system")
}

/// Whether SCC admission governs pods in this namespace.
pub fn governs(ns: &Value) -> bool {
    let name = ns["metadata"]["name"].as_str().unwrap_or_default();
    match ns["metadata"]["labels"][ENFORCE].as_str() {
        Some("privileged") => false,
        Some("restricted") | Some("baseline") => true,
        _ => !is_platform(name),
    }
}

/// Run SCC admission over a pod being created in `ns` (already read, by
/// NamespaceLifecycle). Mutates the pod; an `Err` refuses it.
pub async fn admit_pod(storage: &ResourceStorage, ns: &Value, pod: &mut Value) -> Result<(), ApiError> {
    if !governs(ns) {
        return Ok(());
    }
    // Writes the apiserver makes for itself carry no request.
    let Some(request) = crate::admission::current() else {
        return Ok(());
    };
    if node_mirror_pod(&request.user, pod) {
        return Ok(());
    }
    let ns = namespace_with_ranges(storage, ns.clone()).await?;
    let ns_name = ns["metadata"]["name"].as_str().unwrap_or_default().to_string();
    let pod_name = pod["metadata"]["name"].as_str().or(pod["metadata"]["generateName"].as_str()).unwrap_or("").to_string();
    let refused = |why: String| ApiError::forbidden(&format!("pods \"{pod_name}\" is forbidden: {why}"));
    let required = pod["metadata"]["annotations"][scc_policy::REQUIRED_SCC].as_str().unwrap_or("").to_string();
    let constraints = constraints(storage, &required).await.map_err(&refused)?;
    let mut providers = Vec::new();
    let mut errs = Vec::new();
    for scc in &constraints {
        match Provider::new(&ns, scc) {
            Ok(p) => providers.push(p),
            Err(e) => errs.push(e),
        }
    }
    if !errs.is_empty() {
        return Err(refused(scc_policy::aggregate(&errs)));
    }
    if providers.is_empty() {
        return Err(refused("no SecurityContextConstraintsProvider available to validate pod request".into()));
    }
    let sa = pod["spec"]["serviceAccountName"].as_str().unwrap_or("default").to_string();
    let sa_user = scc_policy::service_account_user(&ns_name, &sa);
    let rbac = request.rbac.as_deref();
    let mut reasons = Vec::new();
    for p in &providers {
        // The ServiceAccount first, then the requester (`allowedForType`).
        let subject = if scc_policy::applies_to(&p.scc, &sa_user, &ns_name, rbac).await {
            "serviceaccount"
        } else if scc_policy::applies_to(&p.scc, &request.user, &ns_name, rbac).await {
            "user"
        } else {
            reasons.push(format!("provider {:?}: Forbidden: not usable by user or serviceaccount", p.name));
            continue;
        };
        let mut candidate = pod.clone();
        let errs = p.assign(&mut candidate);
        if !errs.is_empty() {
            reasons.extend(errs);
            continue;
        }
        *pod = candidate;
        annotate(pod, &p.name, subject);
        return Ok(());
    }
    Err(refused(format!(
        "unable to validate against any security context constraint: {}",
        scc_policy::aggregate(&reasons)
    )))
}

/// The SCCs a pod is tried against (`listSortedSCCs`): the one it requires,
/// or every SCC — refused while one rustkube ships is missing, as OpenShift
/// refuses while its standard set is incomplete.
async fn constraints(storage: &ResourceStorage, required: &str) -> Result<Vec<Value>, String> {
    let all = scc_policy::list(storage, false).await.map_err(|e| e.message)?;
    if !required.is_empty() {
        return match all.into_iter().find(|s| s["metadata"]["name"].as_str() == Some(required)) {
            Some(s) => Ok(vec![s]),
            None => Err(format!(
                "failed to retrieve the required SCC {required:?}: securitycontextconstraints.security.openshift.io {required:?} not found"
            )),
        };
    }
    let missing: Vec<&str> = scc_policy::BUILTIN
        .into_iter()
        .filter(|n| !all.iter().any(|s| s["metadata"]["name"].as_str() == Some(*n)))
        .collect();
    if !missing.is_empty() {
        return Err(format!("securitycontextconstraints.security.openshift.io cache is missing {}", missing.join(", ")));
    }
    Ok(all)
}

/// A static pod's mirror, created by the node that runs it: the one pod the
/// `kubernetes.io/config.mirror` annotation exempts. The annotation is the
/// client's to write, so on its own it exempted any pod from SCC — a user
/// who set it ran privileged, as root, on the host's namespaces (review
/// finding f5ae1c953cf4). The requester must be the node, `system:node:<n>`
/// in `system:nodes`, and the pod bound to `<n>`, as upstream's
/// NodeRestriction holds a node to its own mirror pods.
pub fn node_mirror_pod(user: &UserInfo, pod: &Value) -> bool {
    if !pod["metadata"]["annotations"][MIRROR].is_string() {
        return false;
    }
    let Some(node) = user.username.strip_prefix("system:node:").filter(|n| !n.is_empty()) else {
        return false;
    };
    user.groups.iter().any(|g| g == "system:nodes") && pod["spec"]["nodeName"].as_str() == Some(node)
}

fn annotate(pod: &mut Value, scc: &str, subject: &str) {
    if !pod["metadata"].is_object() {
        pod["metadata"] = json!({});
    }
    if !pod["metadata"]["annotations"].is_object() {
        pod["metadata"]["annotations"] = json!({});
    }
    pod["metadata"]["annotations"][SCC_ANNOTATION] = json!(scc);
    pod["metadata"]["annotations"][scc_policy::SUBJECT_TYPE] = json!(subject);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_store::MemStore;
    use std::sync::Arc;

    const R: IdRange = IdRange { min: 1_000_680_000, max: 1_000_689_999 };


    #[test]
    fn ranges_parse_as_openshift_writes_them() {
        assert_eq!(parse_range("1000680000/10000"), Some(R));
        assert_eq!(parse_range("1000680000-1000689999"), Some(R));
        assert_eq!(parse_range("x/1"), None);
        assert_eq!(parse_range("5/0"), None);
        assert_eq!(parse_ranges("1/2,10/5").unwrap().len(), 2);
        assert_eq!(parse_ranges("1/2,bad"), None);
    }

    #[test]
    fn platform_namespaces_and_labels() {
        let ns = |name: &str, label: Option<&str>| {
            let mut n = json!({"metadata": {"name": name}});
            if let Some(l) = label {
                n["metadata"]["labels"] = json!({ENFORCE: l});
            }
            n
        };
        assert!(!governs(&ns("kube-system", None)));
        assert!(!governs(&ns("storm-users", None)));
        assert!(!governs(&ns("stormblock-system", None)));
        assert!(!governs(&ns("openshift-monitoring", None)));
        assert!(governs(&ns("storefront", None)));
        assert!(governs(&ns("default", None)));
        assert!(!governs(&ns("storefront", Some("privileged"))));
        assert!(governs(&ns("kube-system", Some("restricted"))));
    }

    #[tokio::test]
    async fn blocks_are_never_reused_and_resume_after_the_highest() {
        let storage = ResourceStorage::new(Arc::new(MemStore::default()));
        // A namespace holding block 7, and no allocation record yet.
        let mut held = json!({"metadata": {"name": "old"}});
        set_ranges(&mut held, 7);
        storage.create(&ResourceStorage::cluster_key("namespaces", "old"), held).await.unwrap();
        assert_eq!(allocate_block(&storage).await.unwrap(), 8);
        assert_eq!(allocate_block(&storage).await.unwrap(), 9);

        let mut ns = json!({"metadata": {"name": "a"}});
        assert_eq!(ensure_ranges(&storage, &mut ns).await.unwrap(), Some(10));
        assert_eq!(ns["metadata"]["annotations"][UID_RANGE], "1000100000/10000");
        assert_eq!(ns["metadata"]["annotations"][SUPPLEMENTAL_GROUPS], "1000100000/10000");
        // Already set: kept.
        assert_eq!(ensure_ranges(&storage, &mut ns).await.unwrap(), None);
    }

    /// #377 (finding 5a63ec20536a): a create of an existing namespace name
    /// is refused before a block is taken — repeated `oc new-project` of a
    /// taken name used to spend a block per 409 until the pool ran out.
    #[tokio::test]
    async fn an_existing_name_takes_no_block() {
        let storage = ResourceStorage::new(Arc::new(MemStore::default()));
        let mut ns = json!({"metadata": {"name": "taken"}});
        let block = ranges_for_new_namespace(&storage, &mut ns).await.unwrap();
        storage.create(&ResourceStorage::cluster_key("namespaces", "taken"), ns).await.unwrap();
        assert_eq!(block, Some(0));
        for _ in 0..5 {
            let mut again = json!({"metadata": {"name": "taken"}});
            let err = ranges_for_new_namespace(&storage, &mut again).await.unwrap_err();
            assert!(err.is_already_exists(), "{}", err.message);
            assert!(again["metadata"]["annotations"].is_null(), "no ranges given");
        }
        let mut fresh = json!({"metadata": {"name": "fresh"}});
        assert_eq!(ranges_for_new_namespace(&storage, &mut fresh).await.unwrap(), Some(1));
    }

    /// A block taken for a create that then failed with a 4xx (a webhook's
    /// refusal, the name taken in a race) is given back and handed out
    /// again; a 5xx, which may have written the namespace, keeps it.
    #[tokio::test]
    async fn a_failed_create_gives_its_block_back() {
        let storage = ResourceStorage::new(Arc::new(MemStore::default()));
        let refused = ApiError::forbidden("denied by webhook");
        // The last one handed out: `next` steps back.
        let a = allocate_block(&storage).await.unwrap();
        create_failed(&storage, Some(a), &refused).await;
        assert_eq!(allocate_block(&storage).await.unwrap(), a);
        // Not the last one (another allocation since): onto `released`, and
        // handed out before a new one.
        let b = allocate_block(&storage).await.unwrap();
        let c = allocate_block(&storage).await.unwrap();
        create_failed(&storage, Some(b), &ApiError::already_exists("namespaces", "x")).await;
        assert_eq!(allocate_block(&storage).await.unwrap(), b);
        assert_eq!(allocate_block(&storage).await.unwrap(), c + 1);
        // Given back twice, or a 5xx: nothing changes.
        create_failed(&storage, Some(c), &ApiError::internal("datastore timeout")).await;
        assert_eq!(allocate_block(&storage).await.unwrap(), c + 2);
        // Released blocks below `next` fold back into it.
        let (d, e) = (allocate_block(&storage).await.unwrap(), allocate_block(&storage).await.unwrap());
        create_failed(&storage, Some(d), &refused).await;
        create_failed(&storage, Some(d), &refused).await;
        create_failed(&storage, Some(e), &refused).await;
        let rec = storage.get(ALLOCATION_KEY).await.unwrap();
        assert_eq!(rec["next"], d);
        assert_eq!(rec["released"], json!([]));
        // Many refused creates in a row spend nothing.
        for _ in 0..100 {
            let blk = allocate_block(&storage).await.unwrap();
            create_failed(&storage, Some(blk), &refused).await;
        }
        assert_eq!(allocate_block(&storage).await.unwrap(), d);
    }

    #[test]
    fn an_update_without_ranges_keeps_the_stored_ones() {
        let mut stored = json!({"metadata": {"name": "a"}});
        set_ranges(&mut stored, 3);
        let mut body = json!({"metadata": {"name": "a", "labels": {"x": "y"}}});
        keep_ranges(&stored, &mut body);
        assert_eq!(namespace_ranges(&body), namespace_ranges(&stored));
        // Valid ones of its own are the writer's choice.
        let mut body = json!({"metadata": {"name": "a", "annotations": {UID_RANGE: "5000/10", SUPPLEMENTAL_GROUPS: "5000/10"}}});
        keep_ranges(&stored, &mut body);
        assert_eq!(body["metadata"]["annotations"][UID_RANGE], "5000/10");
    }

    fn user(name: &str, groups: &[&str]) -> UserInfo {
        UserInfo { username: name.into(), groups: groups.iter().map(|g| g.to_string()).collect() }
    }

    fn mirror(node: &str) -> Value {
        json!({"metadata": {"name": "web", "annotations": {MIRROR: "abc"}},
               "spec": {"nodeName": node, "hostNetwork": true,
                        "containers": [{"name": "c", "securityContext": {"privileged": true, "runAsUser": 0}}]}})
    }

    #[test]
    fn only_its_node_exempts_a_mirror_pod() {
        let n1 = user("system:node:n1", &["system:nodes", "system:authenticated"]);
        assert!(node_mirror_pod(&n1, &mirror("n1")));
        // Review finding f5ae1c953cf4: the annotation alone exempted it.
        assert!(!node_mirror_pod(&n1, &mirror("n2")), "another node's pod");
        assert!(!node_mirror_pod(&n1, &mirror("")), "unbound");
        assert!(!node_mirror_pod(&user("alice", &["system:authenticated"]), &mirror("n1")));
        assert!(!node_mirror_pod(&user("system:node:n1", &["system:authenticated"]), &mirror("n1")), "not in system:nodes");
        assert!(!node_mirror_pod(&user("system:node:", &["system:nodes"]), &mirror("")));
        let mut plain = mirror("n1");
        plain["metadata"]["annotations"] = json!({});
        assert!(!node_mirror_pod(&n1, &plain), "no annotation");
    }

    #[tokio::test]
    async fn a_users_mirror_annotation_does_not_bypass_admission() {
        let storage = ResourceStorage::new(Arc::new(MemStore::default()));
        crate::scc_policy::bootstrap(&storage).await;
        let mut ns = json!({"metadata": {"name": "app"}});
        set_ranges(&mut ns, 7);
        // No RBAC engine: the bootstrapped grants (restricted-v2 for alice).
        let attrs = |u: UserInfo| {
            crate::admission::RequestAttrs::of("/api/v1/namespaces/app/pods", &axum::http::Method::POST, None, u)
                .unwrap()
        };
        let alice = user("alice", &["system:authenticated"]);
        let mut pod = mirror("n1");
        let r = crate::admission::with_request(attrs(alice), admit_pod(&storage, &ns, &mut pod)).await;
        let e = r.expect_err("a privileged pod a user annotated as a mirror is refused");
        assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN, "{}", e.message);

        let n1 = user("system:node:n1", &["system:nodes", "system:authenticated"]);
        let mut pod = mirror("n1");
        let before = pod.clone();
        crate::admission::with_request(attrs(n1), admit_pod(&storage, &ns, &mut pod)).await.unwrap();
        assert_eq!(pod, before, "the node's own mirror pod is left as its manifest says");
    }

    #[tokio::test]
    async fn a_namespace_without_ranges_gets_them_written() {
        let storage = ResourceStorage::new(Arc::new(MemStore::default()));
        let key = ResourceStorage::cluster_key("namespaces", "legacy");
        storage.create(&key, json!({"metadata": {"name": "legacy"}})).await.unwrap();
        backfill(&storage).await;
        let ns = storage.get(&key).await.unwrap();
        assert_eq!(ns["metadata"]["annotations"][UID_RANGE], "1000000000/10000");
    }

    fn attrs(u: UserInfo) -> crate::admission::RequestAttrs {
        crate::admission::RequestAttrs::of("/api/v1/namespaces/app/pods", &axum::http::Method::POST, None, u).unwrap()
    }

    async fn cluster() -> (ResourceStorage, Value) {
        let storage = ResourceStorage::new(Arc::new(MemStore::default()));
        crate::scc_policy::bootstrap(&storage).await;
        let mut ns = json!({"metadata": {"name": "app"}});
        set_ranges(&mut ns, 68);
        (storage, ns)
    }

    async fn admit(storage: &ResourceStorage, ns: &Value, u: UserInfo, pod: &mut Value) -> Result<(), ApiError> {
        crate::admission::with_request(attrs(u), admit_pod(storage, ns, pod)).await
    }

    /// A user's pod with no securityContext — the storefront's case — gets
    /// restricted-v2's UID, fsGroup, seccomp and capabilities (#321, #373).
    #[tokio::test]
    async fn an_openshift_manifest_gets_restricted_v2() {
        let (storage, ns) = cluster().await;
        let mut p = json!({"metadata": {"name": "web"}, "spec": {"containers": [{"name": "app", "image": "x"}]}});
        admit(&storage, &ns, user("alice", &["system:authenticated"]), &mut p).await.unwrap();
        let c = &p["spec"]["containers"][0]["securityContext"];
        assert_eq!(c["runAsUser"], 1_000_680_000u64);
        assert_eq!(c["runAsNonRoot"], true);
        assert_eq!(c["allowPrivilegeEscalation"], false);
        assert_eq!(c["capabilities"]["drop"], json!(["ALL"]));
        assert_eq!(p["spec"]["securityContext"]["fsGroup"], 1_000_680_000u64);
        assert_eq!(p["spec"]["securityContext"]["seccompProfile"]["type"], "RuntimeDefault");
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "restricted-v2");
        assert_eq!(p["metadata"]["annotations"][crate::scc_policy::SUBJECT_TYPE], "user");
        // And it now passes PodSecurity restricted.
        crate::builtin_admission::pod_security("restricted", &p).unwrap();
    }

    #[tokio::test]
    async fn a_refusal_lists_every_provider_as_openshift_does() {
        let (storage, ns) = cluster().await;
        let mut p = json!({"metadata": {"name": "web"}, "spec": {"containers": [{"name": "app", "securityContext": {"runAsUser": 0}}]}});
        let e = admit(&storage, &ns, user("alice", &["system:authenticated"]), &mut p).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(
            e.message,
            "pods \"web\" is forbidden: unable to validate against any security context constraint: [provider \"anyuid\": \
             Forbidden: not usable by user or serviceaccount, provider restricted-v2: .containers[0].runAsUser: Invalid \
             value: 0: must be in the ranges: [1000680000, 1000689999], provider \"privileged\": Forbidden: not usable \
             by user or serviceaccount]"
        );
        assert!(p["metadata"]["annotations"].is_null(), "the refused pod is left as it came");
    }

    #[tokio::test]
    async fn an_administrator_gets_anyuid_and_then_privileged() {
        let (storage, ns) = cluster().await;
        let admin = user("system:admin", &["system:masters", "system:authenticated"]);
        let mut p = json!({"metadata": {"name": "web"}, "spec": {"containers": [{"name": "a", "securityContext": {"runAsUser": 0}}]}});
        admit(&storage, &ns, admin.clone(), &mut p).await.unwrap();
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "anyuid");
        assert_eq!(p["spec"]["containers"][0]["securityContext"]["runAsUser"], 0);
        let mut p = json!({"metadata": {"name": "web"}, "spec": {"containers": [{"name": "a", "securityContext": {"privileged": true}}]}});
        admit(&storage, &ns, admin, &mut p).await.unwrap();
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "privileged");
    }

    /// An administrator's own SCC is taken by admission in OpenShift's
    /// order: listing the ServiceAccount, at a priority above anyuid.
    #[tokio::test]
    async fn a_custom_scc_admits_by_priority_and_names_its_subject() {
        let (storage, ns) = cluster().await;
        let mut custom = crate::scc_policy::builtins().into_iter().find(|s| s["metadata"]["name"] == "restricted-v2").unwrap();
        custom["metadata"] = json!({"name": "fixed-uid"});
        custom["priority"] = json!(20);
        custom["runAsUser"] = json!({"type": "MustRunAs", "uid": 4242});
        custom["users"] = json!(["system:serviceaccount:app:builder"]);
        crate::scc_policy::admit(&mut custom).unwrap();
        storage.create(&ResourceStorage::cluster_key("securitycontextconstraints", "fixed-uid"), custom).await.unwrap();
        let alice = user("alice", &["system:authenticated"]);
        let mut p = json!({"metadata": {"name": "b"}, "spec": {"serviceAccountName": "builder", "containers": [{"name": "a"}]}});
        admit(&storage, &ns, alice.clone(), &mut p).await.unwrap();
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "fixed-uid");
        assert_eq!(p["metadata"]["annotations"][crate::scc_policy::SUBJECT_TYPE], "serviceaccount");
        assert_eq!(p["spec"]["containers"][0]["securityContext"]["runAsUser"], 4242);
        // Another ServiceAccount's pod is not admitted by it.
        let mut p = json!({"metadata": {"name": "w"}, "spec": {"containers": [{"name": "a"}]}});
        admit(&storage, &ns, alice.clone(), &mut p).await.unwrap();
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "restricted-v2");
        // Required: that SCC alone.
        let mut p = json!({"metadata": {"name": "r", "annotations": {crate::scc_policy::REQUIRED_SCC: "privileged"}},
                           "spec": {"containers": [{"name": "a"}]}});
        let e = admit(&storage, &ns, alice, &mut p).await.unwrap_err().message;
        assert!(e.ends_with("unable to validate against any security context constraint: provider \"privileged\": Forbidden: not usable by user or serviceaccount"), "{e}");
    }

    #[tokio::test]
    async fn a_missing_builtin_refuses_pods() {
        let (storage, ns) = cluster().await;
        storage.delete(&ResourceStorage::cluster_key("securitycontextconstraints", "anyuid"), None).await.unwrap();
        let mut p = json!({"metadata": {"name": "web"}, "spec": {"containers": [{"name": "a"}]}});
        let e = admit(&storage, &ns, user("alice", &["system:authenticated"]), &mut p).await.unwrap_err();
        assert_eq!(e.message, "pods \"web\" is forbidden: securitycontextconstraints.security.openshift.io cache is missing anyuid");
    }
}
