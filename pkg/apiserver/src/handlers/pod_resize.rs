//! In-place pod resize, the `pods/resize` subresource (#136), as upstream
//! serves it since 1.35 (GA).
//!
//! GET returns the Pod. PUT and PATCH (merge, strategic, JSON patch, apply,
//! on the Pod) change only what a resize may: each container's
//! `resources` and `resizePolicy`, matched by name, and an init
//! container's when it is a sidecar (`restartPolicy: Always`). Everything
//! else in the body is ignored, as upstream's resize strategy copies the
//! stored Pod and takes only those fields. Then upstream's
//! `ValidatePodResize`:
//!
//! - a static (mirror) or Windows Pod is not resized;
//! - the QoS class may not change;
//! - a Pod with a running container whose status carries no `resources`
//!   is on a kubelet that does not resize — refused;
//! - requests and limits may change but not be removed, only cpu and
//!   memory may change, a non-sidecar init container's resources not at all;
//! - a request above its limit is invalid.
//!
//! Every refusal is a 422 `Invalid`, as upstream returns its field errors.
//!
//! The write is the Pod's guaranteed update (conditional on a PUT body's
//! `resourceVersion`), admitted like any Pod update. Status (`resize`
//! conditions `PodResizePending`/`PodResizeInProgress`,
//! `containerStatuses[].resources`) is the kubelet's to report; resizing the
//! running container is rustkube-node's.

use super::resource::{apply_patch_body, guaranteed_update};
use super::AppState;
use crate::error::ApiError;
use crate::storage::ResourceStorage;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};

const LISTS: [&str; 2] = ["containers", "initContainers"];

fn is_sidecar(c: &Value) -> bool {
    c["restartPolicy"].as_str() == Some("Always")
}

/// The stored Pod with only the resize fields taken from `new`.
pub fn merge(old: &Value, new: &Value) -> Value {
    let mut pod = old.clone();
    for list in LISTS {
        let Some(stored) = pod["spec"][list].as_array_mut() else { continue };
        for c in stored.iter_mut() {
            let name = c["name"].clone();
            let Some(n) = new["spec"][list].as_array().and_then(|l| l.iter().find(|x| x["name"] == name)) else {
                continue;
            };
            for field in ["resources", "resizePolicy"] {
                match n.get(field) {
                    Some(v) if !v.is_null() => c[field] = v.clone(),
                    _ => {
                        if let Some(m) = c.as_object_mut() {
                            m.remove(field);
                        }
                    }
                }
            }
        }
    }
    pod
}

fn amount(resource: &str, v: &Value) -> Option<i128> {
    let s = v.as_str().map(str::to_string).or_else(|| v.as_f64().map(|n| n.to_string()))?;
    Some(if resource == "cpu" {
        apimachinery::quantity::parse_cpu_millis(&s) as i128
    } else {
        apimachinery::quantity::parse_bytes(&s) as i128
    })
}

/// Upstream's resize validation of `new` (already [`merge`]d) against `old`.
pub fn validate(old: &Value, new: &Value) -> Result<(), ApiError> {
    let name = old["metadata"]["name"].as_str().unwrap_or("");
    let invalid = |errs: &[String]| ApiError::invalid(&format!("Pod \"{name}\" is invalid: {}", errs.join(", ")));
    if old["metadata"]["annotations"]["kubernetes.io/config.mirror"].is_string() {
        return Err(invalid(&["Forbidden: static pods cannot be resized".into()]));
    }
    if old["spec"]["os"]["name"].as_str() == Some("windows") {
        return Err(invalid(&["Forbidden: windows pods cannot be resized".into()]));
    }
    let mut errs: Vec<String> = Vec::new();
    let (before, after) = (crate::builtin_admission::qos_of(old), crate::builtin_admission::qos_of(new));
    if before != after {
        errs.push(format!("spec: Invalid value: \"{after}\": Pod QOS Class may not change as a result of resizing"));
    }
    let running_without_resources = old["status"]["containerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["state"]["running"].is_object())
        .is_some_and(|c| c["resources"].is_null());
    if running_without_resources {
        errs.push("spec: Forbidden: Pod running on node without support for resize".into());
    }
    for list in LISTS {
        let olds = old["spec"][list].as_array().cloned().unwrap_or_default();
        for (ix, oc) in olds.iter().enumerate() {
            let Some(nc) = new["spec"][list].as_array().and_then(|l| l.get(ix)) else { continue };
            let path = format!("spec.{list}[{ix}].resources");
            let (or, nr) = (&oc["resources"], &nc["resources"]);
            if list == "initContainers" && !is_sidecar(oc) {
                if or != nr {
                    errs.push(format!("{path}: Forbidden: resources for non-sidecar init containers are immutable"));
                }
                continue;
            }
            for kind in ["requests", "limits"] {
                let had: Vec<&String> = or[kind].as_object().map(|m| m.keys().collect()).unwrap_or_default();
                if had.iter().any(|k| nr[kind].get(k.as_str()).is_none()) {
                    let what = if kind == "requests" { "resource requests" } else { "resource limits" };
                    errs.push(format!("{path}.{kind}: Forbidden: {what} cannot be removed"));
                }
            }
            let others = |r: &Value| {
                let mut r = r.clone();
                for kind in ["requests", "limits"] {
                    if let Some(m) = r[kind].as_object_mut() {
                        m.remove("cpu");
                        m.remove("memory");
                    }
                    if r[kind].as_object().is_some_and(|m| m.is_empty()) {
                        r.as_object_mut().unwrap().remove(kind);
                    }
                }
                r
            };
            if others(or) != others(nr) {
                errs.push("spec: Forbidden: only cpu and memory resources are mutable".into());
            }
            for res in ["cpu", "memory"] {
                if let (Some(r), Some(l)) = (amount(res, &nr["requests"][res]), amount(res, &nr["limits"][res])) {
                    if r > l {
                        errs.push(format!(
                            "{path}.requests: Invalid value: \"{}\": must be less than or equal to {res} limit of {}",
                            q(&nr["requests"][res]),
                            q(&nr["limits"][res])
                        ));
                    }
                }
            }
        }
    }
    errs.dedup();
    if errs.is_empty() {
        return Ok(());
    }
    Err(invalid(&errs))
}

fn q(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn key(namespace: &str, name: &str) -> String {
    ResourceStorage::namespaced_key("pods", namespace, name)
}

fn resized(old: Value, new: &Value) -> Result<Value, ApiError> {
    let merged = merge(&old, new);
    validate(&old, &merged)?;
    Ok(merged)
}

/// GET /api/v1/namespaces/{ns}/pods/{name}/resize
pub async fn get(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.storage.get(&key(&namespace, &name)).await?))
}

/// PUT …/resize — a Pod; only its resize fields are taken.
pub async fn put(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if body["metadata"]["name"].as_str().is_some_and(|n| n != name) {
        return Err(ApiError::bad_request(&format!(
            "the name of the object ({}) does not match the name on the URL ({name})",
            body["metadata"]["name"].as_str().unwrap_or("")
        )));
    }
    let precondition = body["metadata"]["resourceVersion"].as_str().filter(|v| !v.is_empty()).map(str::to_string);
    let obj = guaranteed_update(&state, &key(&namespace, &name), precondition, |old| resized(old, &body)).await?;
    Ok(Json(obj))
}

/// PATCH …/resize — applied to the Pod, then only its resize fields taken.
pub async fn patch(
    State(state): State<AppState>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let ct = headers.get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let obj = guaranteed_update(&state, &key(&namespace, &name), None, |old| {
        let mut patched = old.clone();
        apply_patch_body(&mut patched, &ct, &body)?;
        resized(old, &patched)
    })
    .await?;
    Ok(Json(obj))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(cpu_req: &str, cpu_lim: &str, mem: &str) -> Value {
        json!({
            "metadata": {"name": "p"},
            "spec": {
                "nodeName": "n1",
                "initContainers": [
                    {"name": "init", "resources": {"requests": {"cpu": "10m"}}},
                    {"name": "side", "restartPolicy": "Always", "resources": {"requests": {"cpu": "10m"}, "limits": {"cpu": "20m"}}}
                ],
                "containers": [{"name": "c", "image": "i",
                    "resources": {"requests": {"cpu": cpu_req, "memory": mem}, "limits": {"cpu": cpu_lim, "memory": mem}},
                    "resizePolicy": [{"resourceName": "cpu", "restartPolicy": "NotRequired"}]}]
            },
            "status": {"qosClass": "Burstable"}
        })
    }

    #[test]
    fn only_resources_and_resize_policy_are_taken() {
        let old = pod("100m", "200m", "100Mi");
        let mut body = pod("150m", "300m", "100Mi");
        body["spec"]["containers"][0]["image"] = json!("other");
        body["spec"]["nodeName"] = json!("n2");
        body["spec"]["containers"][0]["resizePolicy"][0]["restartPolicy"] = json!("RestartContainer");
        body["spec"]["initContainers"][1]["resources"]["limits"]["cpu"] = json!("30m");
        let m = merge(&old, &body);
        assert_eq!(m["spec"]["containers"][0]["resources"]["requests"]["cpu"], "150m");
        assert_eq!(m["spec"]["containers"][0]["resizePolicy"][0]["restartPolicy"], "RestartContainer");
        assert_eq!(m["spec"]["containers"][0]["image"], "i", "image untouched");
        assert_eq!(m["spec"]["nodeName"], "n1");
        assert_eq!(m["spec"]["initContainers"][1]["resources"]["limits"]["cpu"], "30m", "a sidecar resizes");
        assert!(validate(&old, &m).is_ok());
    }

    #[test]
    fn upstream_refusals() {
        let old = pod("100m", "200m", "100Mi");
        let err = |new: Value| validate(&old, &merge(&old, &new)).unwrap_err().message;
        // Burstable → Guaranteed (without the init containers, which keep
        // the Pod Burstable whatever the main container does).
        let mut lone = old.clone();
        lone["spec"].as_object_mut().unwrap().remove("initContainers");
        let to_guaranteed = merge(&lone, &pod("200m", "200m", "100Mi"));
        assert!(validate(&lone, &to_guaranteed).unwrap_err().message
            .contains("spec: Invalid value: \"Guaranteed\": Pod QOS Class may not change as a result of resizing"));
        let mut b = pod("100m", "200m", "100Mi");
        b["spec"]["containers"][0]["resources"]["requests"].as_object_mut().unwrap().remove("memory");
        assert!(err(b).contains("spec.containers[0].resources.requests: Forbidden: resource requests cannot be removed"));
        let mut b = pod("100m", "200m", "100Mi");
        b["spec"]["containers"][0]["resources"]["limits"]["ephemeral-storage"] = json!("1Gi");
        assert!(err(b).contains("only cpu and memory resources are mutable"));
        let mut b = pod("100m", "200m", "100Mi");
        b["spec"]["initContainers"][0]["resources"]["requests"]["cpu"] = json!("20m");
        assert!(err(b).contains("resources for non-sidecar init containers are immutable"));
        assert!(err(pod("300m", "200m", "100Mi")).contains("must be less than or equal to cpu limit of 200m"));
        let mut running = old.clone();
        running["status"]["containerStatuses"] = json!([{"name": "c", "state": {"running": {}}}]);
        let e = validate(&running, &merge(&running, &pod("150m", "300m", "100Mi"))).unwrap_err();
        assert!(e.message.contains("Pod running on node without support for resize"));
        running["status"]["containerStatuses"][0]["resources"] = json!({"requests": {"cpu": "100m"}});
        assert!(validate(&running, &merge(&running, &pod("150m", "300m", "100Mi"))).is_ok());
        let mut mirror = old.clone();
        mirror["metadata"]["annotations"] = json!({"kubernetes.io/config.mirror": "x"});
        assert!(validate(&mirror, &old).unwrap_err().message.contains("static pods cannot be resized"));
    }
}
