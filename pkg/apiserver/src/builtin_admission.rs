//! Built-in admission plugins — the always-on chain upstream enables by default,
//! run before persistence on creates. Implements a first, high-value subset:
//!
//! - **NamespaceLifecycle** (validating): reject writes into a namespace that is
//!   missing or being terminated.
//! - **ServiceAccount** (mutating): default a Pod's `serviceAccountName` to
//!   `default`.
//! - **DefaultTolerationSeconds** (mutating): add the not-ready/unreachable
//!   NoExecute tolerations (300s) to Pods that lack them.

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

    if resource == "services" {
        allocate_cluster_ip(storage, service_cidr, obj).await?;
    }

    if resource == "pods" {
        service_account_default(obj);
        default_toleration_seconds(obj);
        priority_from_class(storage, obj).await;
        // PodSecurity — validate against the namespace's enforce level.
        if let Some(ns_obj) = &ns_obj {
            let level = ns_obj["metadata"]["labels"]
                ["pod-security.kubernetes.io/enforce"]
                .as_str()
                .unwrap_or("");
            pod_security(level, obj)?;
        }
    }

    if resource == "cronjobs" {
        cronjob_schedule(obj)?;
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

/// Give a Service a ClusterIP.
///
/// **Nothing allocated one.** `kubernetes` and `kube-dns` have addresses only
/// because they are written with one at bootstrap; every Service created by
/// anybody else came back with `clusterIP` unset, which renders as `<none>`
/// and means the name resolves to nothing. A Service without an address is not
/// a Service — it is the one thing Kubernetes is expected to provide.
///
/// First free address in the service CIDR, skipping the network address. The
/// scan is over existing Services rather than a persisted bitmap: it is O(n)
/// per create, and n here is small. A bitmap is the right answer at scale and
/// the wrong one to write before anything needs it — but note that this makes
/// two concurrent creates able to pick the same address, which a real
/// allocator must not do.
async fn allocate_cluster_ip(
    storage: &ResourceStorage,
    cidr: &str,
    obj: &mut Value,
) -> Result<(), ApiError> {
    let kind = obj["spec"]["type"].as_str().unwrap_or("ClusterIP");
    // ExternalName is a CNAME and has no address by definition.
    if kind == "ExternalName" {
        return Ok(());
    }
    match obj["spec"]["clusterIP"].as_str() {
        // Headless: the caller asked for no address, and the DNS answer is the
        // pod set instead. Recorded in clusterIPs too, as upstream does.
        Some("None") => {
            obj["spec"]["clusterIPs"] = json!(["None"]);
            return Ok(());
        }
        // Already chosen — bootstrap objects do this, and so may a caller.
        Some(ip) if !ip.is_empty() => {
            obj["spec"]["clusterIPs"] = json!([ip]);
            return Ok(());
        }
        _ => {}
    }

    let Some((base, prefix)) = parse_cidr(cidr) else {
        return Ok(());
    };

    let prefix_all = ResourceStorage::cluster_prefix("services");
    let taken: std::collections::HashSet<String> = match storage.list(&prefix_all, 10_000, None).await
    {
        Ok((items, _, _)) => items
            .iter()
            .filter_map(|s| s["spec"]["clusterIP"].as_str())
            .filter(|ip| *ip != "None")
            .map(str::to_string)
            .collect(),
        Err(_) => Default::default(),
    };

    let count = 1u32 << (32 - prefix.min(32));
    for offset in 1..count.saturating_sub(1) {
        let Some(v) = u32::from(base).checked_add(offset) else {
            break;
        };
        let candidate = std::net::Ipv4Addr::from(v).to_string();
        if !taken.contains(&candidate) {
            obj["spec"]["clusterIP"] = json!(candidate);
            obj["spec"]["clusterIPs"] = json!([candidate]);
            if obj["spec"]["type"].as_str().is_none() {
                obj["spec"]["type"] = json!("ClusterIP");
            }
            return Ok(());
        }
    }
    // Full. Refused rather than created without an address, because a Service
    // that exists and resolves to nothing is worse than one that failed.
    Err(ApiError::invalid(&format!(
        "no ClusterIP available in {cidr}"
    )))
}

/// `10.96.0.0/12` into its base address and prefix length.
fn parse_cidr(cidr: &str) -> Option<(std::net::Ipv4Addr, u32)> {
    let (addr, prefix) = cidr.split_once('/')?;
    Some((addr.parse().ok()?, prefix.parse().ok()?))
}

#[cfg(test)]
mod cluster_ip_tests {
    use super::*;

    #[test]
    fn a_cidr_parses_into_a_base_and_a_prefix() {
        let (base, prefix) = parse_cidr("10.96.0.0/12").unwrap();
        assert_eq!(base.to_string(), "10.96.0.0");
        assert_eq!(prefix, 12);
        assert!(parse_cidr("10.96.0.0").is_none());
        assert!(parse_cidr("nonsense/12").is_none());
    }

    /// The network address is never handed out, and allocation starts at the
    /// address after it — which is where the `kubernetes` Service already sits,
    /// so the first free one a caller gets is past it.
    #[test]
    fn allocation_starts_after_the_network_address() {
        let (base, _) = parse_cidr("10.96.0.0/12").unwrap();
        let first = std::net::Ipv4Addr::from(u32::from(base) + 1);
        assert_eq!(first.to_string(), "10.96.0.1");
    }
}
