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
//! **Which SCC**, as OpenShift chooses: the providers the requester *or the
//! pod's ServiceAccount* may `use` (RBAC, `security.openshift.io`
//! `securitycontextconstraints`, by name), in OpenShift's order — `anyuid`
//! (priority 10), `restricted-v2`, `privileged` — and the first one the pod
//! satisfies admits it. `restricted-v2` is open to everyone (OpenShift binds
//! it to `system:authenticated`). `cluster-admin` and `system:masters` may
//! use every SCC, so a pod an administrator creates directly is admitted
//! under `anyuid` (or `privileged`) unchanged, exactly as on OpenShift, while
//! a pod a controller creates for a Deployment, StatefulSet or Job gets
//! `restricted-v2` unless its ServiceAccount was granted more. Granting one:
//! a RoleBinding to `system:openshift:scc:anyuid` (or `…:privileged`), the
//! ClusterRoles bootstrapped for it.
//!
//! **restricted-v2** (what it sets when the pod leaves it unset, and refuses
//! otherwise, in OpenShift's words):
//!
//! - each container's `runAsUser`: the first UID of the range
//!   (`MustRunAsRange`); a UID outside it is refused;
//! - each container's `runAsNonRoot: true`;
//! - the pod's `fsGroup`: the first group of the supplemental range; one
//!   outside it is refused (`supplementalGroups` are `RunAsAny`);
//! - `allowPrivilegeEscalation: false`; `capabilities.drop` gains `ALL`; only
//!   `NET_BIND_SERVICE` may be added;
//! - the pod's `seccompProfile: RuntimeDefault`; any other profile is refused;
//! - no privileged containers, host network/PID/IPC, host ports, and only
//!   configMap, csi, downwardAPI, emptyDir, ephemeral, image,
//!   persistentVolumeClaim, projected and secret volumes.
//!
//! The pod is annotated `openshift.io/scc: <provider>`. SELinux levels
//! (`openshift.io/sa.scc.mcs`) are not assigned: stormcos nodes do not run
//! SELinux. The kubelet applying `fsGroup` to a claim is rustkube-node#171.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
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

/// The SCCs this admission knows, in OpenShift's order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    AnyUid,
    RestrictedV2,
    Privileged,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::AnyUid => "anyuid",
            Provider::RestrictedV2 => "restricted-v2",
            Provider::Privileged => "privileged",
        }
    }
    const ORDER: [Provider; 3] = [Provider::AnyUid, Provider::RestrictedV2, Provider::Privileged];
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
    let Some((uid, groups)) = namespace_ranges(&ns) else {
        return Err(ApiError::internal("namespace has no UID range"));
    };
    let ns_name = ns["metadata"]["name"].as_str().unwrap_or_default().to_string();
    let sa = pod["spec"]["serviceAccountName"].as_str().unwrap_or("default").to_string();
    let mut usable = Vec::new();
    for p in Provider::ORDER {
        let ok = p == Provider::RestrictedV2
            || may_use(request.rbac.as_deref(), &request.user, &ns_name, p.name()).await
            || may_use(request.rbac.as_deref(), &service_account_user(&ns_name, &sa), &ns_name, p.name()).await;
        usable.push((p, ok));
    }
    choose(pod, uid, &groups, &usable)
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

/// The pod's ServiceAccount as an RBAC subject.
fn service_account_user(ns: &str, sa: &str) -> UserInfo {
    UserInfo {
        username: format!("system:serviceaccount:{ns}:{sa}"),
        groups: vec![
            "system:serviceaccounts".into(),
            format!("system:serviceaccounts:{ns}"),
            "system:authenticated".into(),
        ],
    }
}

async fn may_use(rbac: Option<&RbacEngine>, user: &UserInfo, ns: &str, scc: &str) -> bool {
    let Some(rbac) = rbac else {
        return user.groups.iter().any(|g| g == "system:masters");
    };
    rbac.authorize(
        user,
        &AuthorizationRequest {
            verb: "use".into(),
            resource: "securitycontextconstraints".into(),
            subresource: None,
            api_group: "security.openshift.io".into(),
            namespace: Some(ns.into()),
            name: Some(scc.into()),
        },
    )
    .await
}

/// Admit `pod` under the first usable provider it satisfies, or refuse it
/// with every provider's reason, as OpenShift's message lists them.
pub fn choose(pod: &mut Value, uid: IdRange, groups: &[IdRange], usable: &[(Provider, bool)]) -> Result<(), ApiError> {
    let mut reasons = Vec::new();
    for &(p, ok) in usable {
        if !ok {
            reasons.push(format!("provider \"{}\": Forbidden: not usable by user or serviceaccount", p.name()));
            continue;
        }
        let mut candidate = pod.clone();
        let errs = match p {
            Provider::Privileged => Vec::new(),
            Provider::AnyUid => anyuid(&candidate),
            Provider::RestrictedV2 => restricted_v2(&mut candidate, uid, groups),
        };
        if errs.is_empty() {
            *pod = candidate;
            annotate(pod, p);
            return Ok(());
        }
        reasons.push(format!("provider {}: {}", p.name(), errs.join(", ")));
    }
    let name = pod["metadata"]["name"].as_str().or(pod["metadata"]["generateName"].as_str()).unwrap_or("");
    Err(ApiError::forbidden(&format!(
        "pods \"{name}\" is forbidden: unable to validate against any security context constraint: [{}]",
        reasons.join(", ")
    )))
}

fn annotate(pod: &mut Value, p: Provider) {
    if !pod["metadata"].is_object() {
        pod["metadata"] = json!({});
    }
    if !pod["metadata"]["annotations"].is_object() {
        pod["metadata"]["annotations"] = json!({});
    }
    pod["metadata"]["annotations"][SCC_ANNOTATION] = json!(p.name());
}

const VOLUMES: &[&str] = &[
    "configMap", "csi", "downwardAPI", "emptyDir", "ephemeral", "image", "persistentVolumeClaim", "projected", "secret",
];

/// What `anyuid` and `restricted-v2` both refuse: host namespaces, host
/// ports, privileged containers, volume types outside the list.
fn host_and_volumes(pod: &Value, errs: &mut Vec<String>) {
    let spec = &pod["spec"];
    for (key, what) in [("hostNetwork", "Host network"), ("hostPID", "Host PID"), ("hostIPC", "Host IPC")] {
        if spec[key].as_bool() == Some(true) {
            errs.push(format!(".spec.securityContext.{key}: Invalid value: true: {what} is not allowed to be used"));
        }
    }
    for (i, v) in spec["volumes"].as_array().into_iter().flatten().enumerate() {
        let kind = v.as_object().and_then(|o| o.keys().find(|k| *k != "name")).cloned().unwrap_or_default();
        if !kind.is_empty() && !VOLUMES.contains(&kind.as_str()) {
            errs.push(format!(".spec.volumes[{i}]: Invalid value: \"{kind}\": {kind} volumes are not allowed to be used"));
        }
    }
    for (path, c) in containers(pod) {
        if c["securityContext"]["privileged"].as_bool() == Some(true) {
            errs.push(format!("{path}.privileged: Invalid value: true: Privileged containers are not allowed"));
        }
        for p in c["ports"].as_array().into_iter().flatten() {
            if let Some(port) = p["hostPort"].as_u64().filter(|p| *p != 0) {
                errs.push(format!("{path}.hostPort: Invalid value: {port}: Host ports are not allowed to be used"));
            }
        }
    }
}

/// Every container (init first) with its path in OpenShift's messages.
fn containers(pod: &Value) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    for field in ["initContainers", "containers"] {
        for (i, c) in pod["spec"][field].as_array().into_iter().flatten().enumerate() {
            out.push((format!(".{field}[{i}]"), c));
        }
    }
    out
}

/// `anyuid`: any UID and group, no capabilities added; validation only.
fn anyuid(pod: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    host_and_volumes(pod, &mut errs);
    for (path, c) in containers(pod) {
        for cap in c["securityContext"]["capabilities"]["add"].as_array().into_iter().flatten() {
            errs.push(format!("{path}.capabilities.add: Invalid value: {cap}: capability may not be added"));
        }
    }
    errs
}

fn seccomp_ok(sc: &Value) -> Result<bool, String> {
    match sc["seccompProfile"]["type"].as_str() {
        None => Ok(false),
        Some("RuntimeDefault") => Ok(true),
        Some(other) => Err(other.to_string()),
    }
}

/// `restricted-v2`: fill in what the pod left unset, refuse what it set
/// against the profile. Mutates `pod` (the caller's copy).
fn restricted_v2(pod: &mut Value, uid: IdRange, groups: &[IdRange]) -> Vec<String> {
    let mut errs = Vec::new();
    host_and_volumes(pod, &mut errs);
    if !pod["spec"].is_object() {
        pod["spec"] = json!({});
    }
    if !pod["spec"]["securityContext"].is_object() {
        pod["spec"]["securityContext"] = json!({});
    }
    let pod_sc = pod["spec"]["securityContext"].clone();

    // fsGroup: MustRunAs, the first group of the supplemental range.
    match pod_sc["fsGroup"].as_i64() {
        None => pod["spec"]["securityContext"]["fsGroup"] = json!(groups[0].min),
        Some(g) if g >= 0 && groups.iter().any(|r| r.contains(g as u64)) => {}
        Some(g) => errs.push(format!(".spec.securityContext.fsGroup: Invalid value: []int64{{{g}}}: {g} is not an allowed group")),
    }
    // seccomp: runtime/default only.
    match seccomp_ok(&pod_sc) {
        Ok(true) => {}
        Ok(false) => pod["spec"]["securityContext"]["seccompProfile"] = json!({"type": "RuntimeDefault"}),
        Err(t) => errs.push(format!(
            ".spec.securityContext.seccompProfile.type: Invalid value: \"{t}\": {t} is not an allowed seccomp profile. Valid values are runtime/default"
        )),
    }
    let pod_uid = pod_sc["runAsUser"].as_i64();
    let pod_nonroot = pod_sc["runAsNonRoot"].as_bool();

    for field in ["initContainers", "containers"] {
        let Some(list) = pod["spec"][field].as_array_mut() else { continue };
        for (i, c) in list.iter_mut().enumerate() {
            let path = format!(".{field}[{i}]");
            if !c["securityContext"].is_object() {
                c["securityContext"] = json!({});
            }
            let sc = &mut c["securityContext"];
            // runAsUser: MustRunAsRange, the first UID of the range.
            match sc["runAsUser"].as_i64().or(pod_uid) {
                None => sc["runAsUser"] = json!(uid.min),
                Some(u) if u >= 0 && uid.contains(u as u64) => {}
                Some(u) => errs.push(format!(
                    "{path}.runAsUser: Invalid value: {u}: must be in the ranges: [{}, {}]",
                    uid.min, uid.max
                )),
            }
            if sc["runAsNonRoot"].as_bool().or(pod_nonroot).is_none() {
                sc["runAsNonRoot"] = json!(true);
            }
            match sc["allowPrivilegeEscalation"].as_bool() {
                None => sc["allowPrivilegeEscalation"] = json!(false),
                Some(false) => {}
                Some(true) => errs.push(format!(
                    "{path}.allowPrivilegeEscalation: Invalid value: true: Allowing privilege escalation for containers is not allowed"
                )),
            }
            if !sc["capabilities"].is_object() {
                sc["capabilities"] = json!({});
            }
            for cap in sc["capabilities"]["add"].as_array().into_iter().flatten() {
                if cap.as_str() != Some("NET_BIND_SERVICE") {
                    errs.push(format!("{path}.capabilities.add: Invalid value: {cap}: capability may not be added"));
                }
            }
            let mut drop = sc["capabilities"]["drop"].as_array().cloned().unwrap_or_default();
            if !drop.iter().any(|d| d.as_str() == Some("ALL")) {
                drop.push(json!("ALL"));
            }
            sc["capabilities"]["drop"] = Value::Array(drop);
            if let Err(t) = seccomp_ok(sc) {
                errs.push(format!(
                    "{path}.seccompProfile.type: Invalid value: \"{t}\": {t} is not an allowed seccomp profile. Valid values are runtime/default"
                ));
            }
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_store::MemStore;
    use std::sync::Arc;

    const R: IdRange = IdRange { min: 1_000_680_000, max: 1_000_689_999 };
    const ALL: [(Provider, bool); 3] =
        [(Provider::AnyUid, false), (Provider::RestrictedV2, true), (Provider::Privileged, false)];

    fn pod(spec: Value) -> Value {
        json!({"metadata": {"name": "web"}, "spec": spec})
    }

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
    fn an_openshift_manifest_gets_restricted_v2() {
        // No runAsUser, no fsGroup, nothing: the storefront's case.
        let mut p = pod(json!({"containers": [{"name": "app", "image": "x"}]}));
        choose(&mut p, R, &[R], &ALL).unwrap();
        let c = &p["spec"]["containers"][0]["securityContext"];
        assert_eq!(c["runAsUser"], 1_000_680_000u64);
        assert_eq!(c["runAsNonRoot"], true);
        assert_eq!(c["allowPrivilegeEscalation"], false);
        assert_eq!(c["capabilities"]["drop"], json!(["ALL"]));
        assert_eq!(p["spec"]["securityContext"]["fsGroup"], 1_000_680_000u64);
        assert_eq!(p["spec"]["securityContext"]["seccompProfile"]["type"], "RuntimeDefault");
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "restricted-v2");
        // And it now passes PodSecurity restricted.
        crate::builtin_admission::pod_security("restricted", &p).unwrap();
    }

    #[test]
    fn a_uid_in_range_is_kept_and_one_outside_is_refused() {
        let mut p = pod(json!({"securityContext": {"runAsUser": 1_000_680_005u64},
                               "containers": [{"name": "app"}]}));
        choose(&mut p, R, &[R], &ALL).unwrap();
        assert!(p["spec"]["containers"][0]["securityContext"]["runAsUser"].is_null());

        let mut p = pod(json!({"containers": [{"name": "app", "securityContext": {"runAsUser": 0}}]}));
        let e = choose(&mut p, R, &[R], &ALL).unwrap_err();
        assert!(e.message.contains("unable to validate against any security context constraint"), "{}", e.message);
        assert!(e.message.contains("provider restricted-v2: .containers[0].runAsUser: Invalid value: 0: must be in the ranges: [1000680000, 1000689999]"), "{}", e.message);
        assert!(e.message.contains("provider \"anyuid\": Forbidden: not usable by user or serviceaccount"));
        // The refused pod is left as it came.
        assert!(p["metadata"]["annotations"].is_null());
    }

    #[test]
    fn fs_group_outside_the_range_is_refused() {
        let mut p = pod(json!({"securityContext": {"fsGroup": 1}, "containers": [{"name": "a"}]}));
        let e = choose(&mut p, R, &[R], &ALL).unwrap_err();
        assert!(e.message.contains(".spec.securityContext.fsGroup: Invalid value: []int64{1}: 1 is not an allowed group"));
    }

    #[test]
    fn privileged_host_and_capabilities_are_refused() {
        let mut p = pod(json!({"hostNetwork": true,
            "volumes": [{"name": "h", "hostPath": {"path": "/"}}, {"name": "d", "emptyDir": {}}],
            "containers": [{"name": "a", "ports": [{"containerPort": 80, "hostPort": 80}],
                            "securityContext": {"privileged": true, "allowPrivilegeEscalation": true,
                                                "capabilities": {"add": ["NET_ADMIN", "NET_BIND_SERVICE"]}}}]}));
        let e = choose(&mut p, R, &[R], &ALL).unwrap_err().message;
        for want in [
            "Host network is not allowed to be used",
            ".spec.volumes[0]: Invalid value: \"hostPath\": hostPath volumes are not allowed to be used",
            ".containers[0].privileged: Invalid value: true: Privileged containers are not allowed",
            ".containers[0].hostPort: Invalid value: 80",
            "Allowing privilege escalation for containers is not allowed",
            "capabilities.add: Invalid value: \"NET_ADMIN\": capability may not be added",
        ] {
            assert!(e.contains(want), "{want} not in {e}");
        }
        assert!(!e.contains("NET_BIND_SERVICE\": capability"));
    }

    #[test]
    fn anyuid_and_privileged_admit_what_restricted_refuses() {
        // An administrator (may use every SCC) pinning UID 0: anyuid, unchanged.
        let every = [(Provider::AnyUid, true), (Provider::RestrictedV2, true), (Provider::Privileged, true)];
        let mut p = pod(json!({"containers": [{"name": "a", "securityContext": {"runAsUser": 0}}]}));
        let before = p["spec"].clone();
        choose(&mut p, R, &[R], &every).unwrap();
        assert_eq!(p["spec"], before);
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "anyuid");
        // A privileged container: past anyuid and restricted-v2, privileged.
        let mut p = pod(json!({"containers": [{"name": "a", "securityContext": {"privileged": true}}]}));
        choose(&mut p, R, &[R], &every).unwrap();
        assert_eq!(p["metadata"]["annotations"][SCC_ANNOTATION], "privileged");
    }

    #[test]
    fn seccomp_other_than_runtime_default_is_refused() {
        let mut p = pod(json!({"securityContext": {"seccompProfile": {"type": "Unconfined"}}, "containers": [{"name": "a"}]}));
        assert!(choose(&mut p, R, &[R], &ALL).unwrap_err().message.contains("Unconfined is not an allowed seccomp profile"));
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
        let mut ns = json!({"metadata": {"name": "app"}});
        set_ranges(&mut ns, 7);
        // No RBAC engine: only restricted-v2 is usable by anyone here.
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
}
