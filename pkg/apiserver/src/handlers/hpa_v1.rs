//! `autoscaling/v1` HorizontalPodAutoscaler (#123): a view of the stored
//! `autoscaling/v2` object, converted both ways as upstream converts it.
//!
//! - `spec.targetCPUUtilizationPercentage` ⇄ v2's `Resource` cpu metric with
//!   a `Utilization` target;
//! - every other v2 metric, the `behavior`, the status' other current metrics
//!   and its conditions ride in upstream's annotations
//!   (`autoscaling.alpha.kubernetes.io/metrics`, `…/behavior`,
//!   `…/current-metrics`, `…/conditions`), so a v1 client that reads and
//!   writes an object back loses nothing;
//! - `status.currentCPUUtilizationPercentage` ⇄ the cpu current metric.
//!
//! Reads (get, list, watch — line by line) are the v2 object converted;
//! writes are converted to v2 and go through the v2 handlers; a PATCH is
//! applied to the v1 view of the stored object.

use super::resource::{self, apply_patch_body, guaranteed_update};
use super::AppState;
use crate::error::ApiError;
use crate::storage::ResourceStorage;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::{json, Map, Value};

const RES: &str = "horizontalpodautoscalers";
const ANN_METRICS: &str = "autoscaling.alpha.kubernetes.io/metrics";
const ANN_BEHAVIOR: &str = "autoscaling.alpha.kubernetes.io/behavior";
const ANN_CURRENT: &str = "autoscaling.alpha.kubernetes.io/current-metrics";
const ANN_CONDITIONS: &str = "autoscaling.alpha.kubernetes.io/conditions";

fn is_cpu_utilization(m: &Value) -> bool {
    m["type"] == "Resource" && m["resource"]["name"] == "cpu" && m["resource"]["target"]["type"] == "Utilization"
}

fn set_ann(obj: &mut Value, key: &str, v: &Value) {
    if !obj["metadata"]["annotations"].is_object() {
        obj["metadata"]["annotations"] = json!({});
    }
    obj["metadata"]["annotations"][key] = json!(serde_json::to_string(v).unwrap_or_default());
}

fn take_ann(obj: &mut Value, key: &str) -> Option<Value> {
    // get_mut, not indexing: a mutable index inserts the key it misses.
    let anns = obj.get_mut("metadata")?.get_mut("annotations")?.as_object_mut()?;
    let raw = anns.remove(key)?;
    let out = raw.as_str().and_then(|s| serde_json::from_str(s).ok());
    if anns.is_empty() {
        obj["metadata"].as_object_mut().map(|m| m.remove("annotations"));
    }
    out
}

/// The v1 view of a stored v2 HorizontalPodAutoscaler.
pub fn to_v1(v2: &Value) -> Value {
    let mut o = v2.clone();
    o["apiVersion"] = json!("autoscaling/v1");
    o["kind"] = json!("HorizontalPodAutoscaler");
    let spec = &v2["spec"];
    let mut s = Map::new();
    for k in ["scaleTargetRef", "minReplicas", "maxReplicas"] {
        if !spec[k].is_null() {
            s.insert(k.into(), spec[k].clone());
        }
    }
    let metrics = spec["metrics"].as_array().cloned().unwrap_or_default();
    let mut rest = Vec::new();
    for m in metrics {
        if is_cpu_utilization(&m) && !s.contains_key("targetCPUUtilizationPercentage") {
            s.insert("targetCPUUtilizationPercentage".into(), m["resource"]["target"]["averageUtilization"].clone());
        } else {
            rest.push(m);
        }
    }
    if !rest.is_empty() {
        set_ann(&mut o, ANN_METRICS, &json!(rest));
    }
    if !spec["behavior"].is_null() {
        set_ann(&mut o, ANN_BEHAVIOR, &spec["behavior"]);
    }
    o["spec"] = Value::Object(s);

    let st = &v2["status"];
    if st.is_object() {
        let mut n = Map::new();
        for k in ["observedGeneration", "lastScaleTime", "currentReplicas", "desiredReplicas"] {
            if !st[k].is_null() {
                n.insert(k.into(), st[k].clone());
            }
        }
        let mut rest = Vec::new();
        for m in st["currentMetrics"].as_array().cloned().unwrap_or_default() {
            let cpu = m["type"] == "Resource" && m["resource"]["name"] == "cpu" && !m["resource"]["current"]["averageUtilization"].is_null();
            if cpu && !n.contains_key("currentCPUUtilizationPercentage") {
                n.insert("currentCPUUtilizationPercentage".into(), m["resource"]["current"]["averageUtilization"].clone());
            } else {
                rest.push(m);
            }
        }
        if !rest.is_empty() {
            set_ann(&mut o, ANN_CURRENT, &json!(rest));
        }
        if st["conditions"].as_array().is_some_and(|c| !c.is_empty()) {
            set_ann(&mut o, ANN_CONDITIONS, &st["conditions"]);
        }
        o["status"] = Value::Object(n);
    }
    o
}

/// The v2 object a v1 HorizontalPodAutoscaler stands for.
pub fn to_v2(v1: &Value) -> Value {
    let mut o = v1.clone();
    o["apiVersion"] = json!("autoscaling/v2");
    o["kind"] = json!("HorizontalPodAutoscaler");
    let extra = take_ann(&mut o, ANN_METRICS);
    let behavior = take_ann(&mut o, ANN_BEHAVIOR);
    let current = take_ann(&mut o, ANN_CURRENT);
    let conditions = take_ann(&mut o, ANN_CONDITIONS);
    let spec = &v1["spec"];
    let mut s = Map::new();
    for k in ["scaleTargetRef", "minReplicas", "maxReplicas"] {
        if !spec[k].is_null() {
            s.insert(k.into(), spec[k].clone());
        }
    }
    let mut metrics = Vec::new();
    if let Some(t) = spec["targetCPUUtilizationPercentage"].as_i64() {
        metrics.push(json!({"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": t}}}));
    }
    metrics.extend(extra.and_then(|v| v.as_array().cloned()).unwrap_or_default());
    if !metrics.is_empty() {
        s.insert("metrics".into(), json!(metrics));
    }
    if let Some(b) = behavior {
        s.insert("behavior".into(), b);
    }
    o["spec"] = Value::Object(s);
    let st = &v1["status"];
    if st.is_object() {
        let mut n = Map::new();
        for k in ["observedGeneration", "lastScaleTime", "currentReplicas", "desiredReplicas"] {
            if !st[k].is_null() {
                n.insert(k.into(), st[k].clone());
            }
        }
        let mut cur = Vec::new();
        if let Some(u) = st["currentCPUUtilizationPercentage"].as_i64() {
            cur.push(json!({"type": "Resource", "resource": {"name": "cpu", "current": {"averageUtilization": u}}}));
        }
        cur.extend(current.and_then(|v| v.as_array().cloned()).unwrap_or_default());
        if !cur.is_empty() {
            n.insert("currentMetrics".into(), json!(cur));
        }
        if let Some(c) = conditions {
            n.insert("conditions".into(), c);
        }
        o["status"] = Value::Object(n);
    }
    o
}

/// A v2 response converted: an object, a list, a deleted list; a Status as it is.
fn convert_value(v: Value) -> Value {
    match v["kind"].as_str() {
        Some("HorizontalPodAutoscaler") => to_v1(&v),
        Some("HorizontalPodAutoscalerList") => {
            let mut l = v;
            l["apiVersion"] = json!("autoscaling/v1");
            if let Some(items) = l["items"].as_array_mut() {
                for i in items.iter_mut() {
                    *i = to_v1(i);
                }
            }
            l
        }
        _ => v,
    }
}

/// Convert a v2 handler's response: JSON whole, a watch stream line by line.
async fn convert_response(resp: Response) -> Response {
    let (mut parts, body) = resp.into_parts();
    let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !ct.starts_with("application/json") {
        return Response::from_parts(parts, body);
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    if ct.contains("stream=watch") {
        // One JSON event per line; a line may span chunks.
        let stream = futures::stream::unfold((body.into_data_stream(), Vec::<u8>::new()), |(mut s, mut buf)| async move {
            loop {
                if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let out = match serde_json::from_slice::<Value>(&line) {
                        Ok(mut ev) => {
                            ev["object"] = convert_value(ev["object"].take());
                            let mut b = serde_json::to_vec(&ev).unwrap_or_default();
                            b.push(b'\n');
                            b
                        }
                        Err(_) => line,
                    };
                    return Some((Ok::<_, std::io::Error>(bytes::Bytes::from(out)), (s, buf)));
                }
                match s.next().await {
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    _ if !buf.is_empty() => return Some((Ok(bytes::Bytes::from(std::mem::take(&mut buf))), (s, buf))),
                    _ => return None,
                }
            }
        });
        return Response::from_parts(parts, Body::from_stream(stream));
    }
    let bytes = axum::body::to_bytes(body, 64 << 20).await.unwrap_or_default();
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => Response::from_parts(parts, Body::from(serde_json::to_vec(&convert_value(v)).unwrap_or_default())),
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

fn key(ns: &str, name: &str) -> String {
    ResourceStorage::namespaced_key(RES, ns, name)
}

pub async fn list(State(st): State<AppState>, Path(ns): Path<String>, headers: HeaderMap, q: RawQuery) -> Response {
    let r = resource::list_namespaced_resources(State(st), Path((ns, RES.into())), headers, q).await.into_response();
    convert_response(r).await
}

pub async fn list_all(State(st): State<AppState>, headers: HeaderMap, q: RawQuery) -> Response {
    let r = resource::list_all_namespaces_resources(State(st), Path(RES.into()), headers, q).await.into_response();
    convert_response(r).await
}

pub async fn create(State(st): State<AppState>, Path(ns): Path<String>, Json(body): Json<Value>) -> Response {
    let r = resource::create_namespaced_resource(State(st), Path((ns, RES.into())), Json(to_v2(&body))).await.into_response();
    convert_response(r).await
}

pub async fn get(State(st): State<AppState>, Path((ns, name)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    Ok(Json(to_v1(&st.storage.get(&key(&ns, &name)).await?)))
}

pub async fn update(State(st): State<AppState>, Path((ns, name)): Path<(String, String)>, Json(body): Json<Value>) -> Response {
    let r = resource::update_namespaced_resource(State(st), Path((ns, RES.into(), name)), Json(to_v2(&body))).await.into_response();
    convert_response(r).await
}

/// PATCH applies to the v1 view: `targetCPUUtilizationPercentage` patches as a v1 client means it.
pub async fn patch(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let obj = guaranteed_update(&st, &key(&ns, &name), None, |stored| {
        let mut v1 = to_v1(&stored);
        apply_patch_body(&mut v1, &ct, &body)?;
        let mut v2 = to_v2(&v1);
        v2["status"] = stored["status"].clone(); // the main resource does not write status
        Ok(v2)
    })
    .await?;
    Ok(Json(to_v1(&obj)))
}

pub async fn delete(State(st): State<AppState>, Path((ns, name)): Path<(String, String)>, q: RawQuery, body: axum::body::Bytes) -> Response {
    let r = resource::delete_namespaced_resource(State(st), Path((ns, RES.into(), name)), q, body).await.into_response();
    convert_response(r).await
}

pub async fn delete_collection(State(st): State<AppState>, Path(ns): Path<String>, q: RawQuery, body: axum::body::Bytes) -> Response {
    let r = resource::delete_namespaced_collection(State(st), Path((ns, RES.into())), q, body).await.into_response();
    convert_response(r).await
}

pub async fn get_status(st: State<AppState>, p: Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    get(st, p).await
}

pub async fn update_status(State(st): State<AppState>, Path((ns, name)): Path<(String, String)>, Json(body): Json<Value>) -> Response {
    let r = resource::update_namespaced_status(State(st), Path((ns, RES.into(), name)), Json(to_v2(&body))).await.into_response();
    convert_response(r).await
}

pub async fn patch_status(
    State(st): State<AppState>,
    Path((ns, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let obj = guaranteed_update(&st, &key(&ns, &name), None, |stored| {
        let mut v1 = to_v1(&stored);
        apply_patch_body(&mut v1, &ct, &body)?;
        let mut v2 = stored.clone();
        v2["status"] = to_v2(&v1)["status"].clone(); // status only
        Ok(v2)
    })
    .await?;
    Ok(Json(to_v1(&obj)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2() -> Value {
        json!({"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler",
            "metadata": {"name": "h", "namespace": "d", "annotations": {"keep": "me"}},
            "spec": {"scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"}, "minReplicas": 1, "maxReplicas": 5,
                "metrics": [
                    {"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 60}}},
                    {"type": "Resource", "resource": {"name": "memory", "target": {"type": "AverageValue", "averageValue": "100Mi"}}}],
                "behavior": {"scaleDown": {"stabilizationWindowSeconds": 60}}},
            "status": {"currentReplicas": 2, "desiredReplicas": 3,
                "currentMetrics": [{"type": "Resource", "resource": {"name": "cpu", "current": {"averageUtilization": 90}}}],
                "conditions": [{"type": "AbleToScale", "status": "True"}]}})
    }

    #[test]
    fn v2_reads_as_v1_and_round_trips() {
        let v1 = to_v1(&v2());
        assert_eq!(v1["apiVersion"], "autoscaling/v1");
        assert_eq!(v1["spec"]["targetCPUUtilizationPercentage"], 60);
        assert!(v1["spec"].get("metrics").is_none());
        assert_eq!(v1["status"]["currentCPUUtilizationPercentage"], 90);
        let anns = &v1["metadata"]["annotations"];
        assert_eq!(anns["keep"], "me");
        assert!(anns[ANN_METRICS].as_str().unwrap().contains("memory"));
        assert!(anns[ANN_BEHAVIOR].as_str().unwrap().contains("stabilizationWindowSeconds"));
        assert!(anns[ANN_CONDITIONS].as_str().unwrap().contains("AbleToScale"));
        // Back to v2: nothing lost, the annotations taken off again.
        let back = to_v2(&v1);
        assert_eq!(back["spec"], v2()["spec"]);
        assert_eq!(back["status"], v2()["status"]);
        assert_eq!(back["metadata"]["annotations"], json!({"keep": "me"}));
    }

    #[test]
    fn a_plain_v1_object_converts() {
        let v1 = json!({"apiVersion": "autoscaling/v1", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "h"},
            "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "web"}, "maxReplicas": 3, "targetCPUUtilizationPercentage": 50}});
        let v2 = to_v2(&v1);
        assert_eq!(v2["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"], 50);
        assert!(v2["metadata"].get("annotations").is_none());
        let none = to_v2(&json!({"spec": {"maxReplicas": 3}}));
        assert!(none["spec"].get("metrics").is_none(), "no target: the v2 default (cpu 80 %) applies");
        assert_eq!(convert_value(json!({"kind": "HorizontalPodAutoscalerList", "items": [v2]}))["items"][0]["apiVersion"], "autoscaling/v1");
        assert_eq!(convert_value(json!({"kind": "Status", "code": 404}))["code"], 404);
    }
}
