//! Built-in admission plugins — the always-on chain upstream enables by default,
//! run before persistence on creates. Implements a first, high-value subset:
//!
//! - **NamespaceLifecycle** (validating): reject writes into a namespace that is
//!   missing or being terminated.
//! - **ServiceAccount** (mutating): default a Pod's `serviceAccountName` to
//!   `default`, and mount its API credentials — the projected
//!   `kube-api-access-*` volume (token, `ca.crt`, namespace) at
//!   `/var/run/secrets/kubernetes.io/serviceaccount` in every container —
//!   unless the pod or its ServiceAccount opts out.
//! - **DefaultTolerationSeconds** (mutating): add the not-ready/unreachable
//!   NoExecute tolerations (300s) to Pods that lack them.
//! - **Namespace defaults** (mutating): `status.phase: Active` and the
//!   `kubernetes` finalizer, as upstream's namespace strategy sets on create.
//! - **Service defaults** (mutating): port `protocol` and `targetPort`, and a
//!   ClusterIP allocated from `--service-cidr`.
//! - **Priority** (mutating): a Pod's `spec.priority` from its PriorityClass.
//! - **PodSecurity** (validating): a subset of baseline/restricted, keyed on
//!   the namespace's `pod-security.kubernetes.io/enforce` label.
//! - **CronJob schedule** and **PVC access modes** (validating): a
//!   `ReadWriteOncePod` claim may not name any other mode.
//! - **Validation** of what upstream's strategies refuse and nothing else here
//!   checked: ConfigMap and Secret data keys, pod sysctl names. And a pod's
//!   `status.qosClass`, which upstream sets on create.
//!
//! That is all the admission there is: webhooks are not called (#82).

use crate::error::ApiError;
use crate::storage::ResourceStorage;
use serde_json::{json, Value};

/// Run built-in admission for a create. Mutates `obj` in place; an `Err`
/// rejects the request.
pub async fn admit_create(
    storage: &ResourceStorage,
    resource: &str,
    namespace: Option<&str>,
    obj: &mut Value,
    service_cidr: &str,
) -> Result<(), ApiError> {
    // NamespaceLifecycle — namespaced resources (other than Namespaces) require
    // an existing, non-terminating namespace.
    let ns_obj = if let Some(ns) = namespace {
        if resource != "namespaces" {
            Some(namespace_lifecycle(storage, ns).await?)
        } else {
            None
        }
    } else {
        None
    };

    if resource == "namespaces" {
        namespace_defaults(obj);
    }

    // Served (#119) but not evaluated yet (#234): say so, rather than let
    // the policy's author believe it protects anything.
    if matches!(resource, "validatingadmissionpolicies" | "mutatingadmissionpolicies"
        | "validatingadmissionpolicybindings" | "mutatingadmissionpolicybindings")
    {
        crate::admission::warn(format!(
            "{resource} are stored but not evaluated by this apiserver yet (rustkube#234): this policy is not enforced"
        ));
    }

    if resource == "services" {
        default_service_ports(obj);
        // ClusterIP and node ports (#132); a failed create gives them back
        // (`node_port::release_all` in the create paths).
        crate::node_port::plan(storage, service_cidr, None, obj).await?;
    }

    if resource == "pods" {
        service_account_default(obj);
        if let Some(ns) = namespace {
            service_account_token_volume(storage, ns, obj).await;
        }
        default_toleration_seconds(obj);
        priority_from_class(storage, obj).await;
        pod_sysctls(obj)?;
        // The API's defaulting, then LimitRanger (#131) — before the QoS
        // class, which reads the resources they set.
        crate::limitranger::requests_from_limits(obj);
        if let Some(ns) = namespace {
            let ranges = limit_ranges(storage, ns).await;
            if !ranges.is_empty() {
                if let Some(text) = crate::limitranger::default_pod(obj, &ranges) {
                    crate::limitranger::annotate(obj, text);
                }
                limit_refused("pods", obj, crate::limitranger::validate_pod(obj, &ranges))?;
            }
        }
        qos_class(obj);
        initial_phase(obj);
        // PodSecurity — validate against the namespace's enforce level.
        if let Some(ns_obj) = &ns_obj {
            let level = ns_obj["metadata"]["labels"]
                ["pod-security.kubernetes.io/enforce"]
                .as_str()
                .unwrap_or("");
            pod_security(level, obj)?;
        }
    }

    if resource == "persistentvolumeclaims" {
        if let Some(ns) = namespace {
            let ranges = limit_ranges(storage, ns).await;
            limit_refused("persistentvolumeclaims", obj, crate::limitranger::validate_pvc(obj, &ranges))?;
        }
    }

    if resource == "cronjobs" {
        cronjob_schedule(obj)?;
    }

    if resource == "configmaps" || resource == "secrets" {
        data_keys(resource, obj)?;
    }

    if resource == "secrets" {
        fold_string_data(obj);
    }

    if resource == "persistentvolumeclaims" {
        access_modes(obj)?;
    }

    if resource == "persistentvolumeclaims" || resource == "persistentvolumes" {
        initial_phase(obj);
    }
    Ok(())
}

/// Admission for an update of a PersistentVolumeClaim (#63): upstream's
/// validation of the claim, then its `PersistentVolumeClaimResize` plugin.
///
/// Growing a claim is the one change a bound claim's spec takes, and it is
/// how volume expansion starts: the driver's external-resizer sees
/// `spec.resources.requests.storage` above `status.capacity` and does the
/// rest, writing the claim's resize status itself. What the apiserver owes is
/// that only a legal request reaches it:
///
/// - a claim's spec is immutable after creation except
///   `resources.requests`, `volumeAttributesClassName`, a `volumeName` set
///   where there was none (binding) and a `storageClassName` set where there
///   was none (a default class assigned after the fact);
/// - `resources.requests` changes only on a Bound claim, and only `storage`;
/// - it may shrink only to what the claim has (`status.capacity`) — the
///   recovery from an expansion the driver could not make — never below;
/// - growing needs a StorageClass with `allowVolumeExpansion: true`
///   (403 otherwise, as upstream's admission answers).
///
/// Before this any change to a claim's spec was stored, and a larger request
/// waited forever for a resize nobody would make.
/// The namespace's LimitRanges (#131).
async fn limit_ranges(storage: &ResourceStorage, ns: &str) -> Vec<Value> {
    storage
        .list(&ResourceStorage::namespace_prefix("limitranges", ns), 500, None)
        .await
        .map(|(items, _, _)| items)
        .unwrap_or_default()
}

/// Upstream's refusal: 403, `pods "x" is forbidden: [what, what]`.
fn limit_refused(resource: &str, obj: &Value, errs: Vec<String>) -> Result<(), ApiError> {
    if errs.is_empty() {
        return Ok(());
    }
    let name = obj["metadata"]["name"].as_str().or(obj["metadata"]["generateName"].as_str()).unwrap_or("");
    Err(ApiError::forbidden(&format!("{resource} \"{name}\" is forbidden: [{}]", errs.join(", "))))
}

pub async fn pvc_update(storage: &ResourceStorage, old: &Value, new: &Value) -> Result<(), ApiError> {
    let grew = pvc_update_valid(old, new)?;
    if grew {
        let class = old["spec"]["storageClassName"].as_str().unwrap_or("");
        let expandable = !class.is_empty()
            && storage
                .get(&ResourceStorage::cluster_key("storageclasses", class))
                .await
                .ok()
                .is_some_and(|sc| sc["allowVolumeExpansion"].as_bool() == Some(true));
        if !expandable {
            return Err(ApiError::forbidden(
                "only dynamically provisioned pvc can be resized and the storageclass that provisions the pvc must support resize",
            ));
        }
    }
    Ok(())
}

/// A Pod's scheduling gates can only be removed after create (#87), as
/// upstream validates: a gate that holds a Pod back is a promise to whoever
/// put it there, and adding one to a Pod already queued or bound would mean
/// nothing to a scheduler that has moved on.
pub fn pod_gates_update(old: &Value, new: &Value) -> Result<(), ApiError> {
    let names = |p: &Value| -> Vec<String> {
        p["spec"]["schedulingGates"]
            .as_array()
            .map(|g| g.iter().filter_map(|g| g["name"].as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let before = names(old);
    if let Some(added) = names(new).into_iter().find(|g| !before.contains(g)) {
        return Err(ApiError::invalid(&format!(
            "spec.schedulingGates: Forbidden: only deletion is allowed, but found new scheduling gate '{added}'"
        )));
    }
    Ok(())
}

/// The validation half of [`pvc_update`]; `Ok(true)` when the storage
/// request grew.
fn pvc_update_valid(old: &Value, new: &Value) -> Result<bool, ApiError> {
    use apimachinery::quantity::parse_bytes;
    let immutable = |spec: &Value| -> Value {
        let mut s = spec.clone();
        if let Some(m) = s.as_object_mut() {
            m.remove("resources");
            m.remove("volumeAttributesClassName");
            // Unset is upstream's default.
            m.entry("volumeMode").or_insert(json!("Filesystem"));
            if old["spec"]["volumeName"].as_str().unwrap_or("").is_empty() {
                m.remove("volumeName");
            }
            if old["spec"]["storageClassName"].is_null() {
                m.remove("storageClassName");
            }
            m.retain(|_, v| !v.is_null());
        }
        s
    };
    if immutable(&old["spec"]) != immutable(&new["spec"]) {
        return Err(ApiError::invalid(
            "spec: Forbidden: spec is immutable after creation except resources.requests and volumeAttributesClassName for bound claims",
        ));
    }
    let (or, nr) = (&old["spec"]["resources"], &new["spec"]["resources"]);
    if or == nr {
        return Ok(false);
    }
    if old["status"]["phase"].as_str() != Some("Bound") {
        return Err(ApiError::invalid(
            "spec: Forbidden: spec is immutable after creation except resources.requests and volumeAttributesClassName for bound claims",
        ));
    }
    let without_storage = |r: &Value| -> Value {
        let mut r = r.clone();
        if let Some(q) = r["requests"].as_object_mut() {
            q.remove("storage");
        }
        r
    };
    if without_storage(or) != without_storage(nr) {
        return Err(ApiError::invalid(
            "spec.resources: Forbidden: only resources.requests.storage may be changed",
        ));
    }
    let bytes = |v: &Value| v.as_str().map(parse_bytes).unwrap_or(0);
    let (was, now) = (bytes(&or["requests"]["storage"]), bytes(&nr["requests"]["storage"]));
    let has = bytes(&old["status"]["capacity"]["storage"]);
    if now < was && now < has {
        return Err(ApiError::invalid(&format!(
            "spec.resources.requests.storage: Forbidden: field can not be less than status.capacity ({})",
            old["status"]["capacity"]["storage"].as_str().unwrap_or("?")
        )));
    }
    Ok(now > was)
}

/// What a namespace is given on create: `status.phase: Active` and the
/// `kubernetes` finalizer in `spec.finalizers` (#75).
///
/// Upstream's namespace strategy does both in `PrepareForCreate`: the status a
/// client sends is replaced, not merged, and the finalizer is added alongside
/// any the client named. Only bootstrap set them here, so every namespace made
/// through the API was neither Active nor Terminating — `kubectl wait` and
/// anything else gating on the phase had a value outside the enum to reason
/// about. Deletion adds the finalizer too, but a namespace should carry it from
/// the moment it is stored, as it does upstream.
///
/// Returns whether anything changed, for the boot-time backfill.
pub fn namespace_defaults(obj: &mut Value) -> bool {
    let before = (obj["status"].clone(), obj["spec"]["finalizers"].clone());
    // A namespace that is already terminating keeps its phase: this also runs
    // over stored objects, and one mid-deletion must not be revived.
    let terminating = obj["metadata"]["deletionTimestamp"].is_string();
    if !terminating {
        obj["status"] = json!({"phase": "Active"});
    }
    if !obj["spec"].is_object() {
        obj["spec"] = json!({});
    }
    let mut finalizers = obj["spec"]["finalizers"].as_array().cloned().unwrap_or_default();
    if !terminating && !finalizers.iter().any(|f| f.as_str() == Some("kubernetes")) {
        finalizers.push(Value::String("kubernetes".into()));
    }
    obj["spec"]["finalizers"] = Value::Array(finalizers);
    (obj["status"].clone(), obj["spec"]["finalizers"].clone()) != before
}

/// `ReadWriteOncePod` may not be combined with any other access mode.
///
/// Upstream forbids it, and the reason is that the two halves contradict each
/// other: RWOP promises exactly one pod, every other mode permits more than
/// one. A claim asking for both is asking for exclusivity and sharing at once,
/// and whichever the binder honoured would be the wrong answer half the time —
/// so it is refused at the door rather than resolved by precedence.
fn access_modes(obj: &Value) -> Result<(), ApiError> {
    let modes: Vec<&str> = obj["spec"]["accessModes"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if modes.contains(&"ReadWriteOncePod") && modes.len() > 1 {
        return Err(ApiError {
            status: axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            reason: "Invalid".into(),
            message: format!(
                "spec.accessModes: ReadWriteOncePod may not be combined with other modes (got [{}])",
                modes.join(", ")
            ),
            continue_token: None,
        });
    }
    Ok(())
}

/// Reject a CronJob whose schedule can never fire.
///
/// Without this the failure is silent and looks like patience: the object is
/// accepted, `get cronjobs` shows it, `lastScheduleTime` stays empty, and
/// nothing anywhere says the schedule is unsatisfiable. `0 0 30 2 *` waits
/// for the 30th of February forever. Rejecting at admission turns a month-long
/// mystery into a message at `kubectl apply`.
fn cronjob_schedule(obj: &Value) -> Result<(), ApiError> {
    let Some(schedule) = obj["spec"]["schedule"].as_str() else {
        return Err(ApiError::invalid("spec.schedule is required"));
    };
    if let Err(e) = apimachinery::cron::validate(schedule) {
        return Err(ApiError::invalid(&format!(
            "spec.schedule {schedule:?} will never fire: {e}"
        )));
    }
    Ok(())
}

async fn namespace_lifecycle(
    storage: &ResourceStorage,
    ns: &str,
) -> Result<serde_json::Value, ApiError> {
    let key = ResourceStorage::cluster_key("namespaces", ns);
    match storage.get(&key).await {
        Ok(nsobj) => {
            if nsobj["status"]["phase"].as_str() == Some("Terminating") {
                return Err(ApiError::forbidden(&format!(
                    "unable to create new content in namespace {ns} because it is being terminated"
                )));
            }
            Ok(nsobj)
        }
        Err(_) => Err(ApiError::forbidden(&format!("namespace {ns} not found"))),
    }
}

/// PodSecurity admission — a subset of the baseline/restricted checks, keyed on
/// the namespace's `pod-security.kubernetes.io/enforce` level.
fn pod_security(level: &str, obj: &Value) -> Result<(), ApiError> {
    if level.is_empty() || level == "privileged" {
        return Ok(());
    }
    let spec = &obj["spec"];

    // baseline + restricted: no host namespaces, no hostPath volumes.
    for key in ["hostNetwork", "hostPID", "hostIPC"] {
        if spec[key].as_bool() == Some(true) {
            return Err(ApiError::forbidden(&format!(
                "pod security \"{level}\": {key} is not allowed"
            )));
        }
    }
    if let Some(vols) = spec["volumes"].as_array() {
        if vols.iter().any(|v| !v["hostPath"].is_null()) {
            return Err(ApiError::forbidden(&format!(
                "pod security \"{level}\": hostPath volumes are not allowed"
            )));
        }
    }

    let empty = vec![];
    let containers = spec["containers"].as_array().unwrap_or(&empty);
    let init = spec["initContainers"].as_array().unwrap_or(&empty);
    for c in containers.iter().chain(init.iter()) {
        let sc = &c["securityContext"];
        if sc["privileged"].as_bool() == Some(true) {
            return Err(ApiError::forbidden(&format!(
                "pod security \"{level}\": privileged containers are not allowed"
            )));
        }
        if level == "restricted" {
            if sc["allowPrivilegeEscalation"].as_bool() != Some(false) {
                return Err(ApiError::forbidden(
                    "pod security \"restricted\": allowPrivilegeEscalation must be false",
                ));
            }
            let drops_all = sc["capabilities"]["drop"]
                .as_array()
                .map(|d| d.iter().any(|x| x.as_str() == Some("ALL")))
                .unwrap_or(false);
            if !drops_all {
                return Err(ApiError::forbidden(
                    "pod security \"restricted\": containers must drop ALL capabilities",
                ));
            }
        }
    }

    if level == "restricted" {
        let pod_nonroot = spec["securityContext"]["runAsNonRoot"].as_bool() == Some(true);
        for c in containers.iter().chain(init.iter()) {
            let c_nonroot = c["securityContext"]["runAsNonRoot"].as_bool() == Some(true);
            if !pod_nonroot && !c_nonroot {
                return Err(ApiError::forbidden(
                    "pod security \"restricted\": runAsNonRoot must be true",
                ));
            }
        }
    }
    Ok(())
}

fn service_account_default(obj: &mut Value) {
    if let Some(spec) = obj.get_mut("spec").and_then(|s| s.as_object_mut()) {
        let unset = spec
            .get("serviceAccountName")
            .and_then(|v| v.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(true);
        if unset {
            spec.insert("serviceAccountName".into(), json!("default"));
        }
        // Mirror to the deprecated `serviceAccount` field for compatibility.
        let name = spec
            .get("serviceAccountName")
            .cloned()
            .unwrap_or_else(|| json!("default"));
        spec.insert("serviceAccount".into(), name);
    }
}

/// A ConfigMap or Secret data key, as upstream validates it: 1–253
/// characters of `[-._a-zA-Z0-9]`, not `.` or `..`, and not in both `data`
/// and `binaryData`. An empty key was stored (#67: the ConfigMap and Secret
/// empty-key conformance specs).
/// A Secret's `stringData` folded into `data`, as upstream's apiserver does
/// on every write (#101): each entry base64-encoded into `data`, overwriting
/// a same-named key, and `stringData` dropped — it is write-only and never
/// stored or returned. Readers (`kubectl get -o jsonpath='{.data.x}'`,
/// client-go's typed `Secret.Data`, the kubelet mounting it) only look at
/// `data`. True when it changed the object.
pub fn fold_string_data(obj: &mut Value) -> bool {
    use base64::Engine;
    let Some(string_data) = obj.as_object_mut().and_then(|o| o.remove("stringData")) else {
        return false;
    };
    let Value::Object(entries) = string_data else {
        return true; // a null or malformed stringData is dropped, as upstream
    };
    if !obj["data"].is_object() {
        obj["data"] = serde_json::json!({});
    }
    for (k, v) in entries {
        let text = match &v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        obj["data"][k] = Value::String(base64::engine::general_purpose::STANDARD.encode(text));
    }
    true
}

fn data_keys(resource: &str, obj: &Value) -> Result<(), ApiError> {
    let fields: &[&str] = if resource == "configmaps" { &["data", "binaryData"] } else { &["data", "stringData"] };
    let valid = |k: &str| {
        !k.is_empty()
            && k.len() <= 253
            && k != "."
            && k != ".."
            && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    };
    for f in fields {
        for k in obj[*f].as_object().into_iter().flat_map(|m| m.keys()) {
            if !valid(k) {
                return Err(ApiError::invalid(&format!(
                    "{f}[{k:?}]: Invalid value: {k:?}: a valid config key must consist of alphanumeric \
                     characters, '-', '_' or '.'"
                )));
            }
        }
    }
    if resource == "configmaps" {
        if let (Some(d), Some(b)) = (obj["data"].as_object(), obj["binaryData"].as_object()) {
            if let Some(k) = d.keys().find(|k| b.contains_key(*k)) {
                return Err(ApiError::invalid(&format!("data[{k:?}]: duplicate of key present in binaryData")));
            }
        }
    }
    Ok(())
}

/// Pod sysctl names: dot- or slash-separated segments of `[a-z0-9_-]`, each
/// starting and ending alphanumeric, at most 253 characters, none twice.
/// Invalid names were accepted (#67: the Sysctls conformance spec).
fn pod_sysctls(obj: &Value) -> Result<(), ApiError> {
    let Some(sysctls) = obj["spec"]["securityContext"]["sysctls"].as_array() else {
        return Ok(());
    };
    let segment_ok = |s: &str| {
        let b = s.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && (b[b.len() - 1].is_ascii_lowercase() || b[b.len() - 1].is_ascii_digit())
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
    };
    // Every problem in one error, as upstream reports them: a client fixing
    // one name at a time would otherwise need a round trip per name.
    let mut seen = std::collections::HashSet::new();
    let mut errors = Vec::new();
    for (i, s) in sysctls.iter().enumerate() {
        let name = s["name"].as_str().unwrap_or("");
        if name.is_empty() || name.len() > 253 || !name.split(['.', '/']).all(segment_ok) {
            errors.push(format!(
                "spec.securityContext.sysctls[{i}].name: Invalid value: {name:?}: must be a valid sysctl name"
            ));
        } else if !seen.insert(name) {
            errors.push(format!("spec.securityContext.sysctls[{i}].name: Duplicate value: {name:?}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::invalid(&errors.join(", ")))
    }
}

/// A pod's QoS class, set on create as upstream does: Guaranteed when every
/// container has cpu and memory limits and its requests (defaulting to the
/// limits) equal them; BestEffort when no container requests or limits
/// either; Burstable otherwise. It was never set (#67).
fn qos_class(obj: &mut Value) {
    let class = qos_of(obj);
    if !obj["status"].is_object() {
        obj["status"] = json!({});
    }
    obj["status"]["qosClass"] = json!(class);
}

/// The QoS class [`qos_class`] would set, without setting it (in-place
/// resize must not change it, #136).
pub(crate) fn qos_of(obj: &Value) -> &'static str {
    let spec = &obj["spec"];
    let containers: Vec<&Value> = ["containers", "initContainers"]
        .iter()
        .flat_map(|k| spec[*k].as_array().into_iter().flatten())
        .collect();
    let mut any = false;
    let mut guaranteed = !containers.is_empty();
    for c in &containers {
        let (req, lim) = (&c["resources"]["requests"], &c["resources"]["limits"]);
        for r in ["cpu", "memory"] {
            // A quantity may arrive as a string or a bare number.
            let text = |v: &Value| v.as_str().map(str::to_string).or_else(|| v.as_f64().map(|n| n.to_string()));
            let amount = |s: &str| {
                if r == "cpu" {
                    apimachinery::quantity::parse_cpu_millis(s)
                } else {
                    apimachinery::quantity::parse_bytes(s)
                }
            };
            let (rq, lm) = (text(&req[r]), text(&lim[r]));
            if rq.is_some() || lm.is_some() {
                any = true;
            }
            match (rq, lm) {
                (_, None) => guaranteed = false,
                (Some(q), Some(l)) if amount(&q) != amount(&l) => guaranteed = false,
                _ => {}
            }
        }
    }
    if guaranteed { "Guaranteed" } else if any { "Burstable" } else { "BestEffort" }
}

/// `status.phase: Pending` on create for a Pod, PersistentVolumeClaim or
/// PersistentVolume that names none, as upstream's strategies set it. They
/// were stored with no phase at all, which is outside the enum every client
/// switches on (#67; #102 for PVCs).
fn initial_phase(obj: &mut Value) {
    if !obj["status"].is_object() {
        obj["status"] = json!({});
    }
    if obj["status"]["phase"].as_str().map_or(true, str::is_empty) {
        obj["status"]["phase"] = json!("Pending");
    }
}

/// Where a container finds its ServiceAccount credentials.
const SA_MOUNT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// ServiceAccount admission's token mount, as upstream: a projected volume of
/// a bound token (3607 s), the namespace's `kube-root-ca.crt` and the pod's
/// namespace, mounted read-only at `SA_MOUNT_PATH` in every container and
/// init container that does not already mount something there.
///
/// Skipped when `spec.automountServiceAccountToken` is false, or it is unset
/// and the ServiceAccount's is false — the opt-out the conformance suite
/// checks (#67) — and when a volume of that name is already there. The
/// kubelet (rustkube-node) materializes all three sources.
async fn service_account_token_volume(storage: &ResourceStorage, namespace: &str, obj: &mut Value) {
    let automount = match obj["spec"]["automountServiceAccountToken"].as_bool() {
        Some(b) => b,
        None => {
            let sa = obj["spec"]["serviceAccountName"].as_str().unwrap_or("default").to_string();
            let key = ResourceStorage::namespaced_key("serviceaccounts", namespace, &sa);
            storage
                .get(&key)
                .await
                .ok()
                .and_then(|sa| sa["automountServiceAccountToken"].as_bool())
                .unwrap_or(true)
        }
    };
    if !automount {
        return;
    }
    let Some(spec) = obj.get_mut("spec").and_then(|s| s.as_object_mut()) else {
        return;
    };
    let mounts_path = |c: &Value| {
        c["volumeMounts"]
            .as_array()
            .is_some_and(|ms| ms.iter().any(|m| m["mountPath"].as_str() == Some(SA_MOUNT_PATH)))
    };
    let volumes = spec.entry("volumes").or_insert_with(|| json!([]));
    let Some(volumes) = volumes.as_array_mut() else { return };
    if volumes
        .iter()
        .any(|v| v["name"].as_str().is_some_and(|n| n.starts_with("kube-api-access-")))
    {
        return;
    }
    const ALPHABET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    let suffix: String = uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .take(5)
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    let name = format!("kube-api-access-{suffix}");
    volumes.push(json!({
        "name": name,
        "projected": {
            "defaultMode": 420,
            "sources": [
                { "serviceAccountToken": { "expirationSeconds": 3607, "path": "token" } },
                { "configMap": { "name": "kube-root-ca.crt",
                                 "items": [ { "key": "ca.crt", "path": "ca.crt" } ] } },
                { "downwardAPI": { "items": [ { "path": "namespace",
                    "fieldRef": { "apiVersion": "v1", "fieldPath": "metadata.namespace" } } ] } }
            ]
        }
    }));
    for key in ["containers", "initContainers"] {
        if let Some(cs) = spec.get_mut(key).and_then(Value::as_array_mut) {
            for c in cs.iter_mut() {
                if mounts_path(c) || !c.is_object() {
                    continue;
                }
                let ms = c.as_object_mut().unwrap().entry("volumeMounts").or_insert_with(|| json!([]));
                if let Some(ms) = ms.as_array_mut() {
                    ms.push(json!({ "name": name, "mountPath": SA_MOUNT_PATH, "readOnly": true }));
                }
            }
        }
    }
}

fn default_toleration_seconds(obj: &mut Value) {
    let Some(spec) = obj.get_mut("spec").and_then(|s| s.as_object_mut()) else {
        return;
    };
    let tols = spec.entry("tolerations").or_insert_with(|| json!([]));
    let Some(arr) = tols.as_array_mut() else { return };
    for key in [
        "node.kubernetes.io/not-ready",
        "node.kubernetes.io/unreachable",
    ] {
        let present = arr.iter().any(|t| t["key"].as_str() == Some(key));
        if !present {
            arr.push(json!({
                "key": key,
                "operator": "Exists",
                "effect": "NoExecute",
                "tolerationSeconds": 300
            }));
        }
    }
}

/// Priority admission — resolve a Pod's `spec.priorityClassName` to
/// `spec.priority` from the named PriorityClass (scheduling.k8s.io/v1), so the
/// scheduler's PrioritySort can order it. Leaves priority unset if the class is
/// missing (best-effort, matching how the scheduler defaults priority to 0).
async fn priority_from_class(storage: &ResourceStorage, obj: &mut Value) {
    if !obj["spec"]["priority"].is_null() {
        return; // already set
    }
    let Some(class) = obj["spec"]["priorityClassName"].as_str() else {
        return;
    };
    if class.is_empty() {
        return;
    }
    let key = ResourceStorage::cluster_key("priorityclasses", class);
    if let Ok(pc) = storage.get(&key).await {
        if let Some(val) = pc["value"].as_i64() {
            if let Some(spec) = obj.get_mut("spec").and_then(|s| s.as_object_mut()) {
                spec.insert("priority".into(), json!(val));
            }
        }
    }
}

/// Default `spec.ports[].protocol` to TCP, and `targetPort` to `port`.
///
/// **A port with no protocol matches no endpoint.** Kubernetes defaults the
/// protocol, so nearly every Service in the world omits it; this apiserver did
/// not, and Cilium listed the frontend as `10.96.0.2:8080/NONE` with no
/// backend while the pod behind it answered on its own address perfectly well.
/// Nothing logged an error — the Service simply never worked, which is the
/// worst way for a default to be missing.
fn default_service_ports(obj: &mut Value) {
    default_service_spec(obj);
    let Some(ports) = obj["spec"]["ports"].as_array_mut() else {
        return;
    };
    for p in ports {
        if p["protocol"].as_str().is_none() {
            p["protocol"] = json!("TCP");
        }
        // Upstream defaults targetPort to port when it is absent.
        if p["targetPort"].is_null() {
            if let Some(port) = p["port"].as_i64() {
                p["targetPort"] = json!(port);
            }
        }
    }
}

/// The Service spec fields upstream defaults on create, when they are absent
/// **or empty** — a client-go body decodes from protobuf with every string at
/// `""`, so a Service created by client-go arrived with `type: ""` and was
/// stored that way (#67).
fn default_service_spec(obj: &mut Value) {
    if !obj["spec"].is_object() {
        obj["spec"] = json!({});
    }
    let spec = &mut obj["spec"];
    let unset = |v: &Value| v.as_str().map_or(true, str::is_empty);
    if unset(&spec["type"]) {
        spec["type"] = json!("ClusterIP");
    }
    let external_name = spec["type"] == "ExternalName";
    if unset(&spec["sessionAffinity"]) {
        spec["sessionAffinity"] = json!("None");
    }
    if !external_name {
        if unset(&spec["internalTrafficPolicy"]) {
            spec["internalTrafficPolicy"] = json!("Cluster");
        }
        if unset(&spec["ipFamilyPolicy"]) {
            spec["ipFamilyPolicy"] = json!("SingleStack");
        }
        if spec["ipFamilies"].as_array().map_or(true, Vec::is_empty) {
            spec["ipFamilies"] = json!(["IPv4"]);
        }
    }
}

#[cfg(test)]
mod service_defaults_tests {
    use super::*;

    #[test]
    fn an_empty_service_type_is_cluster_ip() {
        let mut svc = json!({"spec": {"type": "", "sessionAffinity": "", "ports": [{"port": 80}]}});
        default_service_ports(&mut svc);
        assert_eq!(svc["spec"]["type"], "ClusterIP");
        assert_eq!(svc["spec"]["sessionAffinity"], "None");
        assert_eq!(svc["spec"]["internalTrafficPolicy"], "Cluster");
        assert_eq!(svc["spec"]["ipFamilies"], json!(["IPv4"]));
        assert_eq!(svc["spec"]["ports"][0]["protocol"], "TCP");

        let mut ext = json!({"spec": {"type": "ExternalName", "externalName": "x.example"}});
        default_service_ports(&mut ext);
        assert_eq!(ext["spec"]["type"], "ExternalName");
        assert!(ext["spec"]["ipFamilies"].is_null());
    }

    #[test]
    fn read_write_once_pod_may_not_be_combined() {
        // Exclusivity and sharing at once is not something a binder can
        // honour, so it is refused rather than resolved by precedence.
        let both = json!({"spec": {"accessModes": ["ReadWriteOncePod", "ReadWriteOnce"]}});
        let e = access_modes(&both).unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(e.message.contains("ReadWriteOncePod"), "{}", e.message);

        // Alone is the whole point of the mode.
        assert!(access_modes(&json!({"spec": {"accessModes": ["ReadWriteOncePod"]}})).is_ok());
        // Everything else combines as it always could.
        assert!(access_modes(
            &json!({"spec": {"accessModes": ["ReadWriteOnce", "ReadOnlyMany"]}})
        )
        .is_ok());
        // A claim naming no modes is somebody else's error, not this one's.
        assert!(access_modes(&json!({"spec": {}})).is_ok());
    }

    /// A Service written the way nearly every Service is written — no
    /// protocol — must come out with one, or it matches no endpoint.
    #[test]
    fn a_port_with_no_protocol_gets_tcp() {
        let mut svc = json!({"spec": {"ports": [{"port": 8080}]}});
        default_service_ports(&mut svc);
        assert_eq!(svc["spec"]["ports"][0]["protocol"], "TCP");
        assert_eq!(svc["spec"]["ports"][0]["targetPort"], 8080);

        let mut udp = json!({"spec": {"ports": [{"port": 53, "protocol": "UDP"}]}});
        default_service_ports(&mut udp);
        assert_eq!(udp["spec"]["ports"][0]["protocol"], "UDP");

        let mut tp = json!({"spec": {"ports": [{"port": 80, "targetPort": 8080}]}});
        default_service_ports(&mut tp);
        assert_eq!(tp["spec"]["ports"][0]["targetPort"], 8080);
    }
}

#[cfg(test)]
mod namespace_tests {
    use super::namespace_defaults;
    use serde_json::json;

    #[test]
    fn a_namespace_created_through_the_api_is_active_with_the_kubernetes_finalizer() {
        // #75: `kubectl create ns anything` stored neither field.
        let mut ns = json!({"metadata": {"name": "anything"}});
        assert!(namespace_defaults(&mut ns));
        assert_eq!(ns["status"], json!({"phase": "Active"}));
        assert_eq!(ns["spec"]["finalizers"], json!(["kubernetes"]));
    }

    #[test]
    fn client_status_is_replaced_and_client_finalizers_are_kept() {
        let mut ns = json!({
            "metadata": {"name": "x"},
            "spec": {"finalizers": ["example.com/hold"]},
            "status": {"phase": "Terminating"}
        });
        namespace_defaults(&mut ns);
        assert_eq!(ns["status"], json!({"phase": "Active"}));
        assert_eq!(ns["spec"]["finalizers"], json!(["example.com/hold", "kubernetes"]));
    }

    #[test]
    fn a_defaulted_namespace_is_unchanged_the_second_time() {
        let mut ns = json!({"metadata": {"name": "x"}});
        namespace_defaults(&mut ns);
        assert!(!namespace_defaults(&mut ns), "the backfill must be idempotent");
    }

    #[test]
    fn a_terminating_namespace_is_not_revived() {
        // The backfill runs over stored objects; one whose finalizers the
        // controller has cleared must not get them back, or it never goes.
        let mut ns = json!({
            "metadata": {"name": "x", "deletionTimestamp": "2026-09-23T00:00:00Z"},
            "spec": {"finalizers": []},
            "status": {"phase": "Terminating"}
        });
        assert!(!namespace_defaults(&mut ns));
        assert_eq!(ns["status"]["phase"], "Terminating");
        assert_eq!(ns["spec"]["finalizers"], json!([]));
    }
}

#[cfg(test)]
mod sa_token_tests {
    use super::*;
    use crate::test_store::MemStore;
    use std::sync::Arc;

    fn pod(automount: Option<bool>) -> Value {
        let mut p = json!({"metadata": {"name": "p", "namespace": "n"},
                           "spec": {"serviceAccountName": "default",
                                    "containers": [{"name": "a"}, {"name": "b"}]}});
        if let Some(a) = automount {
            p["spec"]["automountServiceAccountToken"] = json!(a);
        }
        p
    }

    fn mounted(p: &Value) -> usize {
        p["spec"]["containers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["volumeMounts"].as_array().is_some_and(|ms| {
                ms.iter().any(|m| m["mountPath"] == SA_MOUNT_PATH)
            }))
            .count()
    }

    /// Mounted by default; not when the pod or its ServiceAccount opts out,
    /// the pod's choice winning (#67).
    #[tokio::test]
    async fn the_token_volume_follows_automount() {
        let s = ResourceStorage::new(Arc::new(MemStore::default()));
        let mut p = pod(None);
        service_account_token_volume(&s, "n", &mut p).await;
        assert_eq!(mounted(&p), 2);
        let vol = &p["spec"]["volumes"][0];
        assert!(vol["name"].as_str().unwrap().starts_with("kube-api-access-"));
        assert_eq!(vol["projected"]["sources"][0]["serviceAccountToken"]["path"], "token");
        // Idempotent: a second pass adds nothing.
        service_account_token_volume(&s, "n", &mut p).await;
        assert_eq!(p["spec"]["volumes"].as_array().unwrap().len(), 1);

        let mut p = pod(Some(false));
        service_account_token_volume(&s, "n", &mut p).await;
        assert_eq!(mounted(&p), 0);

        s.create(&ResourceStorage::namespaced_key("serviceaccounts", "n", "default"),
                 json!({"metadata": {"name": "default", "namespace": "n"},
                        "automountServiceAccountToken": false}))
            .await
            .unwrap();
        let mut p = pod(None);
        service_account_token_volume(&s, "n", &mut p).await;
        assert_eq!(mounted(&p), 0, "the ServiceAccount opted out");
        let mut p = pod(Some(true));
        service_account_token_volume(&s, "n", &mut p).await;
        assert_eq!(mounted(&p), 2, "the pod's choice wins");
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn string_data_is_folded_into_data() {
        use base64::Engine;
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let mut s = serde_json::json!({"data": {"keep": b64("k"), "both": b64("old")},
                                       "stringData": {"both": "new", "userdata": "#cloud-config\n"}});
        assert!(fold_string_data(&mut s));
        assert!(s.get("stringData").is_none(), "never stored");
        assert_eq!(s["data"], serde_json::json!({"keep": b64("k"), "both": b64("new"), "userdata": b64("#cloud-config\n")}),
                   "stringData wins over a same-named data key");
        let mut only = serde_json::json!({"stringData": {"a": "x"}});
        assert!(fold_string_data(&mut only));
        assert_eq!(only["data"]["a"], b64("x"));
        let mut none = serde_json::json!({"data": {"a": b64("x")}});
        assert!(!fold_string_data(&mut none));
    }

    #[test]
    fn data_keys_are_validated() {
        assert!(data_keys("configmaps", &json!({"data": {"a.b-c_1": "x"}})).is_ok());
        assert!(data_keys("configmaps", &json!({"data": {"": "x"}})).is_err());
        assert!(data_keys("secrets", &json!({"data": {"": "eA=="}})).is_err());
        assert!(data_keys("secrets", &json!({"stringData": {"a/b": "x"}})).is_err());
        assert!(data_keys("configmaps", &json!({"data": {"..": "x"}})).is_err());
        assert!(data_keys("configmaps", &json!({"data": {"k": "x"}, "binaryData": {"k": "eA=="}})).is_err());
    }

    #[test]
    fn sysctl_names_are_validated() {
        let pod = |names: &[&str]| json!({"spec": {"securityContext": {"sysctls":
            names.iter().map(|n| json!({"name": n, "value": "1"})).collect::<Vec<_>>()}}});
        assert!(pod_sysctls(&pod(&["kernel.shm_rmid_forced", "net.ipv4.conf/eth0.forwarding"])).is_ok());
        for bad in ["foo-", "bar..", "", "Kernel.x", "a..b"] {
            assert!(pod_sysctls(&pod(&[bad])).is_err(), "{bad:?}");
        }
        assert!(pod_sysctls(&pod(&["kernel.msgmax", "kernel.msgmax"])).is_err());
        // Every invalid name is reported, the valid ones are not (the
        // conformance spec checks both).
        let msg = pod_sysctls(&pod(&["foo-", "kernel.shmmax", "safe-and-unsafe", "bar.."]))
            .unwrap_err()
            .message;
        assert!(msg.contains(r#"Invalid value: "foo-""#) && msg.contains(r#"Invalid value: "bar..""#), "{msg}");
        assert!(!msg.contains("safe-and-unsafe") && !msg.contains("kernel.shmmax"), "{msg}");
    }

    #[test]
    fn qos_class_is_set() {
        let pod = |res: Value| {
            let mut p = json!({"spec": {"containers": [{"name": "c", "resources": res}]}});
            qos_class(&mut p);
            p["status"]["qosClass"].as_str().unwrap().to_string()
        };
        assert_eq!(pod(json!({})), "BestEffort");
        assert_eq!(pod(json!({"requests": {"cpu": "100m"}})), "Burstable");
        assert_eq!(pod(json!({"limits": {"cpu": "1", "memory": "1Gi"}})), "Guaranteed");
        assert_eq!(pod(json!({"requests": {"cpu": "1000m", "memory": "1Gi"},
                              "limits": {"cpu": "1", "memory": "1Gi"}})), "Guaranteed");
        assert_eq!(pod(json!({"requests": {"cpu": "500m", "memory": "1Gi"},
                              "limits": {"cpu": "1", "memory": "1Gi"}})), "Burstable");
    }
}

#[cfg(test)]
mod pvc_update_tests {
    use super::*;

    fn claim(req: &str, phase: &str) -> Value {
        json!({"spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": "fast", "volumeName": "pv-1",
                        "resources": {"requests": {"storage": req}}},
               "status": {"phase": phase, "capacity": {"storage": "1Gi"}}})
    }

    #[test]
    fn a_bound_claim_may_grow() {
        assert!(matches!(pvc_update_valid(&claim("1Gi", "Bound"), &claim("2Gi", "Bound")), Ok(true)));
        // No change, or a change elsewhere (metadata), is not growth.
        assert!(matches!(pvc_update_valid(&claim("1Gi", "Bound"), &claim("1Gi", "Bound")), Ok(false)));
    }

    #[test]
    fn it_shrinks_only_back_to_what_it_has() {
        // Grew to 3Gi, the driver failed: back to 2Gi is recovery, fine.
        assert!(matches!(pvc_update_valid(&claim("3Gi", "Bound"), &claim("2Gi", "Bound")), Ok(false)));
        // Below status.capacity (1Gi) is not.
        assert!(pvc_update_valid(&claim("2Gi", "Bound"), &claim("512Mi", "Bound")).is_err());
    }

    #[test]
    fn an_unbound_claim_keeps_its_request() {
        assert!(pvc_update_valid(&claim("1Gi", "Pending"), &claim("2Gi", "Pending")).is_err());
    }

    #[test]
    fn the_rest_of_the_spec_is_immutable_but_for_binding_and_a_late_default_class() {
        let mut other = claim("1Gi", "Bound");
        other["spec"]["accessModes"] = json!(["ReadWriteMany"]);
        assert!(pvc_update_valid(&claim("1Gi", "Bound"), &other).is_err());
        // The binder setting volumeName, a default class arriving late.
        let mut unbound = claim("1Gi", "Pending");
        unbound["spec"]["volumeName"] = json!("");
        unbound["spec"].as_object_mut().unwrap().remove("storageClassName");
        let bound = claim("1Gi", "Pending");
        assert!(pvc_update_valid(&unbound, &bound).is_ok());
        // volumeMode absent reads as Filesystem.
        let mut explicit = claim("1Gi", "Bound");
        explicit["spec"]["volumeMode"] = json!("Filesystem");
        assert!(pvc_update_valid(&claim("1Gi", "Bound"), &explicit).is_ok());
        // Limits are not a resize.
        let mut limits = claim("1Gi", "Bound");
        limits["spec"]["resources"]["limits"] = json!({"storage": "5Gi"});
        assert!(pvc_update_valid(&claim("1Gi", "Bound"), &limits).is_err());
    }
}

#[cfg(test)]
mod scheduling_gate_tests {
    use serde_json::json;

    #[test]
    fn gates_may_only_be_removed() {
        let pod = |gates: &[&str]| json!({"spec": {"schedulingGates": gates.iter().map(|g| json!({"name": g})).collect::<Vec<_>>()}});
        assert!(super::pod_gates_update(&pod(&["a", "b"]), &pod(&["a"])).is_ok(), "removing one");
        assert!(super::pod_gates_update(&pod(&["a"]), &json!({"spec": {}})).is_ok(), "removing all");
        assert!(super::pod_gates_update(&pod(&["a"]), &pod(&["a"])).is_ok(), "unchanged");
        let e = super::pod_gates_update(&pod(&["a"]), &pod(&["a", "b"])).unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(e.message.contains("only deletion is allowed, but found new scheduling gate 'b'"));
        assert!(super::pod_gates_update(&json!({"spec": {}}), &pod(&["x"])).is_err(), "none to one");
    }
}
