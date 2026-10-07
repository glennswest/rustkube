//! LimitRanger admission (#131): a namespace's LimitRanges applied to the
//! Pods and PersistentVolumeClaims created in it, as upstream's plugin does.
//!
//! Before it, the API's own defaulting: a container's `limits` with no
//! matching `requests` set the request to the limit ([`requests_from_limits`]).
//! Then, for each LimitRange item:
//! - `type: Container`: `default` fills a missing limit, `defaultRequest` a
//!   missing request; then each container is checked against `min` (request
//!   and limit at least it), `max` (limit and request at most it) and
//!   `maxLimitRequestRatio`;
//! - `type: Pod`: the containers' summed requests/limits against `min`/`max`;
//! - `type: PersistentVolumeClaim`: `spec.resources.requests.storage`
//!   against `min`/`max`.
//!
//! A violation is a 403 naming each, in upstream's words. A Pod that had
//! defaults set is annotated `kubernetes.io/limit-ranger` with what was set.

use serde_json::{json, Value};

const ANNOTATION: &str = "kubernetes.io/limit-ranger";

/// A quantity as a number to compare: CPU in millicores, anything else in
/// its base unit (bytes for memory and storage).
fn amount(resource: &str, q: &Value) -> Option<f64> {
    let s = match q {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some(if resource == "cpu" {
        apimachinery::quantity::parse_cpu_millis(&s) as f64
    } else {
        apimachinery::quantity::parse_bytes(&s) as f64
    })
}

fn q(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Every container and init container of a Pod, in turn.
fn for_containers(pod: &mut Value, mut f: impl FnMut(&mut Value)) {
    for key in ["containers", "initContainers"] {
        // get_mut, not indexing: a mutable index inserts what it misses.
        if let Some(list) = pod.get_mut("spec").and_then(|s| s.get_mut(key)).and_then(Value::as_array_mut) {
            list.iter_mut().for_each(&mut f);
        }
    }
}

/// The API's defaulting: a limit without a request sets the request.
pub fn requests_from_limits(pod: &mut Value) {
    for_containers(pod, |c| {
        let Some(limits) = c.get("resources").and_then(|r| r.get("limits")).and_then(Value::as_object).cloned() else {
            return;
        };
        for (k, v) in limits {
            if c["resources"]["requests"].get(&k).is_none() {
                if !c["resources"]["requests"].is_object() {
                    c["resources"]["requests"] = json!({});
                }
                c["resources"]["requests"][&k] = v;
            }
        }
    });
}

fn items<'a>(ranges: &'a [Value], kind: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    ranges
        .iter()
        .flat_map(|r| r["spec"]["limits"].as_array().into_iter().flatten())
        .filter(move |i| i["type"] == kind)
}

/// Fill a Pod's container defaults from `ranges`; returns the annotation
/// text, or None when nothing was set.
pub fn default_pod(pod: &mut Value, ranges: &[Value]) -> Option<String> {
    let mut set: Vec<String> = Vec::new();
    let items: Vec<Value> = items(ranges, "Container").cloned().collect();
    if items.is_empty() {
        return None;
    }
    for_containers(pod, |c| {
        let name = c["name"].as_str().unwrap_or("").to_string();
        let (mut req_set, mut lim_set) = (Vec::new(), Vec::new());
        for item in &items {
            for (field, list, which) in [("default", "limits", &mut lim_set), ("defaultRequest", "requests", &mut req_set)] {
                for (k, v) in item[field].as_object().into_iter().flatten() {
                    if c["resources"][list].get(k).is_none() {
                        if !c["resources"].is_object() {
                            c["resources"] = json!({});
                        }
                        if !c["resources"][list].is_object() {
                            c["resources"][list] = json!({});
                        }
                        c["resources"][list][k] = v.clone();
                        which.push(k.clone());
                    }
                }
            }
        }
        if !req_set.is_empty() {
            set.push(format!("{} request for container {name}", req_set.join(", ")));
        }
        if !lim_set.is_empty() {
            set.push(format!("{} limit for container {name}", lim_set.join(", ")));
        }
    });
    (!set.is_empty()).then(|| format!("LimitRanger plugin set: {}", set.join("; ")))
}

fn check(
    errs: &mut Vec<String>,
    scope: &str,
    item: &Value,
    requests: &serde_json::Map<String, Value>,
    limits: &serde_json::Map<String, Value>,
) {
    for (res, min) in item["min"].as_object().into_iter().flatten() {
        let m = amount(res, min).unwrap_or(0.0);
        match requests.get(res) {
            Some(r) if amount(res, r).unwrap_or(0.0) < m => errs.push(format!(
                "minimum {res} usage per {scope} is {}, but request is {}", q(min), q(r))),
            None => errs.push(format!("minimum {res} usage per {scope} is {}.  No request is specified", q(min))),
            _ => {}
        }
        if let Some(l) = limits.get(res).filter(|l| amount(res, l).unwrap_or(0.0) < m) {
            errs.push(format!("minimum {res} usage per {scope} is {}, but limit is {}", q(min), q(l)));
        }
    }
    for (res, max) in item["max"].as_object().into_iter().flatten() {
        let m = amount(res, max).unwrap_or(f64::MAX);
        match limits.get(res) {
            Some(l) if amount(res, l).unwrap_or(0.0) > m => errs.push(format!(
                "maximum {res} usage per {scope} is {}, but limit is {}", q(max), q(l))),
            None => errs.push(format!("maximum {res} usage per {scope} is {}.  No limit is specified", q(max))),
            _ => {}
        }
        if let Some(r) = requests.get(res).filter(|r| amount(res, r).unwrap_or(0.0) > m) {
            errs.push(format!("maximum {res} usage per {scope} is {}, but request is {}", q(max), q(r)));
        }
    }
    for (res, ratio) in item["maxLimitRequestRatio"].as_object().into_iter().flatten() {
        let want = q(ratio).parse::<f64>().unwrap_or(f64::MAX);
        let (Some(l), Some(r)) = (limits.get(res), requests.get(res)) else {
            if limits.get(res).is_none() {
                errs.push(format!("{res} max limit to request ratio per {scope} is {}, but no limit is specified", q(ratio)));
            }
            continue;
        };
        let (l, r) = (amount(res, l).unwrap_or(0.0), amount(res, r).unwrap_or(0.0));
        if r > 0.0 && l / r > want {
            errs.push(format!("{res} max limit to request ratio per {scope} is {}, but provided ratio is {:.6}", q(ratio), l / r));
        }
    }
}

/// The violations of a (defaulted) Pod against `ranges`.
pub fn validate_pod(pod: &Value, ranges: &[Value]) -> Vec<String> {
    let mut errs = Vec::new();
    let containers: Vec<&Value> = ["containers", "initContainers"]
        .iter()
        .flat_map(|k| pod["spec"][*k].as_array().into_iter().flatten())
        .collect();
    let empty = serde_json::Map::new();
    for item in items(ranges, "Container") {
        for c in &containers {
            let r = c["resources"]["requests"].as_object().unwrap_or(&empty);
            let l = c["resources"]["limits"].as_object().unwrap_or(&empty);
            check(&mut errs, "Container", item, r, l);
        }
    }
    let sum = |list: &str| {
        let mut out: std::collections::BTreeMap<String, f64> = Default::default();
        for c in pod["spec"]["containers"].as_array().into_iter().flatten() {
            for (k, v) in c["resources"][list].as_object().into_iter().flatten() {
                *out.entry(k.clone()).or_default() += amount(k, v).unwrap_or(0.0);
            }
        }
        out.into_iter()
            .map(|(k, v)| {
                let s = if k == "cpu" { format!("{}m", v as u64) } else { format!("{}", v as u64) };
                (k, json!(s))
            })
            .collect::<serde_json::Map<String, Value>>()
    };
    for item in items(ranges, "Pod") {
        check(&mut errs, "Pod", item, &sum("requests"), &sum("limits"));
    }
    errs
}

/// The violations of a PersistentVolumeClaim against `ranges`: its storage
/// request within each `type: PersistentVolumeClaim` item's min/max.
pub fn validate_pvc(pvc: &Value, ranges: &[Value]) -> Vec<String> {
    let mut errs = Vec::new();
    let req = &pvc["spec"]["resources"]["requests"]["storage"];
    for item in items(ranges, "PersistentVolumeClaim") {
        let have = amount("storage", req).unwrap_or(0.0);
        if let Some(min) = item["min"].get("storage").filter(|m| have < amount("storage", m).unwrap_or(0.0)) {
            errs.push(format!("minimum storage usage per PersistentVolumeClaim is {}, but request is {}", q(min), q(req)));
        }
        if let Some(max) = item["max"].get("storage").filter(|m| have > amount("storage", m).unwrap_or(f64::MAX)) {
            errs.push(format!("maximum storage usage per PersistentVolumeClaim is {}, but request is {}", q(max), q(req)));
        }
    }
    errs
}

pub(crate) fn annotate(pod: &mut Value, text: String) {
    if !pod["metadata"]["annotations"].is_object() {
        pod["metadata"]["annotations"] = json!({});
    }
    pod["metadata"]["annotations"][ANNOTATION] = json!(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(cpu: &str, mem: &str, eph: &str) -> Value {
        let mut m = serde_json::Map::new();
        for (k, v) in [("cpu", cpu), ("memory", mem), ("ephemeral-storage", eph)] {
            if !v.is_empty() {
                m.insert(k.into(), json!(v));
            }
        }
        Value::Object(m)
    }
    fn range() -> Value {
        // The conformance spec's LimitRange.
        json!({"spec": {"limits": [{"type": "Container", "min": rl("50m", "100Mi", "100Gi"), "max": rl("500m", "500Mi", "500Gi"),
            "default": rl("500m", "500Mi", "500Gi"), "defaultRequest": rl("100m", "200Mi", "200Gi")}]}})
    }
    fn pod(req: Value, lim: Value) -> Value {
        json!({"metadata": {"name": "p"}, "spec": {"containers": [{"name": "pause", "image": "x",
            "resources": {"requests": req, "limits": lim}}]}})
    }
    fn admit(mut p: Value) -> (Value, Vec<String>) {
        requests_from_limits(&mut p);
        if let Some(a) = default_pod(&mut p, &[range()]) {
            annotate(&mut p, a);
        }
        let errs = validate_pod(&p, &[range()]);
        (p, errs)
    }

    #[test]
    fn the_conformance_spec_defaults_and_bounds() {
        let (p, errs) = admit(pod(json!({}), json!({})));
        assert!(errs.is_empty(), "{errs:?}");
        let r = &p["spec"]["containers"][0]["resources"];
        assert_eq!(r["requests"], rl("100m", "200Mi", "200Gi"));
        assert_eq!(r["limits"], rl("500m", "500Mi", "500Gi"));
        assert!(p["metadata"]["annotations"][ANNOTATION].as_str().unwrap().starts_with("LimitRanger plugin set: "));
        // Partial: requests memory/eph, a cpu limit — the cpu request is the limit.
        let (p, errs) = admit(pod(rl("", "150Mi", "150Gi"), rl("300m", "", "")));
        assert!(errs.is_empty(), "{errs:?}");
        let r = &p["spec"]["containers"][0]["resources"];
        assert_eq!(r["requests"], rl("300m", "150Mi", "150Gi"));
        assert_eq!(r["limits"], rl("300m", "500Mi", "500Gi"));
        // Below min, above max: refused.
        let (_, errs) = admit(pod(rl("10m", "50Mi", "50Gi"), json!({})));
        assert!(errs.iter().any(|e| e == "minimum cpu usage per Container is 50m, but request is 10m"), "{errs:?}");
        let (_, errs) = admit(pod(rl("600m", "600Mi", "600Gi"), json!({})));
        assert!(errs.iter().any(|e| e.starts_with("maximum cpu usage per Container is 500m")), "{errs:?}");
    }

    #[test]
    fn pod_sums_ratios_and_claims() {
        let r = json!({"spec": {"limits": [
            {"type": "Pod", "max": {"cpu": "1"}},
            {"type": "Container", "maxLimitRequestRatio": {"memory": "2"}},
            {"type": "PersistentVolumeClaim", "min": {"storage": "1Gi"}, "max": {"storage": "10Gi"}}]}});
        let two = json!({"spec": {"containers": [
            {"name": "a", "resources": {"requests": {"cpu": "600m", "memory": "100Mi"}, "limits": {"cpu": "600m", "memory": "300Mi"}}},
            {"name": "b", "resources": {"requests": {"cpu": "600m", "memory": "100Mi"}, "limits": {"cpu": "600m", "memory": "100Mi"}}}]}});
        let errs = validate_pod(&two, std::slice::from_ref(&r));
        assert!(errs.iter().any(|e| e == "maximum cpu usage per Pod is 1, but limit is 1200m"), "{errs:?}");
        assert!(errs.iter().any(|e| e.starts_with("memory max limit to request ratio per Container is 2, but provided ratio is 3")), "{errs:?}");
        let claim = |s: &str| json!({"spec": {"resources": {"requests": {"storage": s}}}});
        assert!(validate_pvc(&claim("5Gi"), std::slice::from_ref(&r)).is_empty());
        assert_eq!(validate_pvc(&claim("500Mi"), std::slice::from_ref(&r)),
                   vec!["minimum storage usage per PersistentVolumeClaim is 1Gi, but request is 500Mi"]);
        assert_eq!(validate_pvc(&claim("20Gi"), std::slice::from_ref(&r)).len(), 1);
    }

    #[test]
    fn a_limit_without_a_request_sets_the_request() {
        let mut p = json!({"spec": {"containers": [{"name": "a", "resources": {"limits": {"cpu": "1"}}}],
                                     "initContainers": [{"name": "i", "resources": {"limits": {"memory": "1Gi"}, "requests": {}}}]}});
        requests_from_limits(&mut p);
        assert_eq!(p["spec"]["containers"][0]["resources"]["requests"]["cpu"], "1");
        assert_eq!(p["spec"]["initContainers"][0]["resources"]["requests"]["memory"], "1Gi");
    }
}
