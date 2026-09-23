//! Built-in admission plugins — the always-on chain upstream enables by default,
//! run before persistence on creates. Implements a first, high-value subset:
//!
//! - **NamespaceLifecycle** (validating): reject writes into a namespace that is
//!   missing or being terminated.
//! - **ServiceAccount** (mutating): default a Pod's `serviceAccountName` to
//!   `default`.
//! - **DefaultTolerationSeconds** (mutating): add the not-ready/unreachable
//!   NoExecute tolerations (300s) to Pods that lack them.
//! - **Namespace defaults** (mutating): `status.phase: Active` and the
//!   `kubernetes` finalizer, as upstream's namespace strategy sets on create.

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

    if resource == "services" {
        default_service_ports(obj);
        crate::service_ip::allocate(storage, service_cidr, obj).await?;
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

    if resource == "persistentvolumeclaims" {
        access_modes(obj)?;
    }
    Ok(())
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

#[cfg(test)]
mod service_defaults_tests {
    use super::*;

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
