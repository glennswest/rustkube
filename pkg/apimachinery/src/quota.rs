//! ResourceQuota accounting shared by the apiserver's admission and the
//! controller-manager's quota controller (#124), so the two count alike.
//!
//! [`usage`] is what one object charges, by resource name, in integer
//! units: milli-CPU for `cpu`, `requests.cpu` and `limits.cpu`, bytes for
//! memory and storage, a count for everything else. [`matches`] is whether a
//! quota's scopes cover an object. Quantities go in and out as strings with
//! [`parse`] and [`format`].
//!
//! As upstream's evaluators:
//! - a Pod that is not Succeeded or Failed charges `pods`, `count/pods`,
//!   `cpu`/`requests.cpu`, `memory`/`requests.memory`,
//!   `requests.ephemeral-storage`, and the `limits.*` — its containers' sum,
//!   or its largest init container when that is larger, plus `spec.overhead`;
//! - a Service charges `services`, `services.nodeports` (ports with a node
//!   port) and `services.loadbalancers`;
//! - a PersistentVolumeClaim charges `persistentvolumeclaims`,
//!   `requests.storage`, and both per StorageClass
//!   (`<class>.storageclass.storage.k8s.io/…`);
//! - every object charges `count/<plural>` (core) or `count/<plural>.<group>`,
//!   and the core kinds with a legacy name (`secrets`, `configmaps`,
//!   `replicationcontrollers`, `resourcequotas`) that name too;
//! - a quota with scopes (`Terminating`, `NotTerminating`, `BestEffort`,
//!   `NotBestEffort`, `PriorityClass` via `scopeSelector`) covers Pods only.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub type Usage = BTreeMap<String, i128>;

fn is_cpu(key: &str) -> bool {
    matches!(key, "cpu" | "requests.cpu" | "limits.cpu")
}

/// A quantity string as this key counts it.
pub fn parse(key: &str, v: &Value) -> i128 {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return 0,
    };
    if is_cpu(key) {
        crate::quantity::parse_cpu_millis(&s) as i128
    } else {
        crate::quantity::parse_bytes(&s) as i128
    }
}

/// A count as a quantity string: `500m`/`2` for CPU, a plain number else.
pub fn format(key: &str, n: i128) -> String {
    if is_cpu(key) && n % 1000 != 0 {
        format!("{n}m")
    } else if is_cpu(key) {
        format!("{}", n / 1000)
    } else {
        n.to_string()
    }
}

fn add(u: &mut Usage, key: &str, n: i128) {
    *u.entry(key.to_string()).or_default() += n;
}

/// The compute a Pod holds: (requests, limits) per resource name.
fn pod_compute(pod: &Value) -> (Usage, Usage) {
    let sum = |list: &Value, kind: &str| -> Usage {
        let mut u = Usage::new();
        for c in list.as_array().into_iter().flatten() {
            for (k, v) in c["resources"][kind].as_object().into_iter().flatten() {
                add(&mut u, k, parse(k, v));
            }
        }
        u
    };
    let mut out = (Usage::new(), Usage::new());
    for (i, kind) in ["requests", "limits"].iter().enumerate() {
        let mut total = sum(&pod["spec"]["containers"], kind);
        for c in pod["spec"]["initContainers"].as_array().into_iter().flatten() {
            for (k, v) in c["resources"][*kind].as_object().into_iter().flatten() {
                let n = parse(k, v);
                let e = total.entry(k.clone()).or_default();
                *e = (*e).max(n);
            }
        }
        for (k, v) in pod["spec"]["overhead"].as_object().into_iter().flatten() {
            add(&mut total, k, parse(k, v));
        }
        if i == 0 { out.0 = total } else { out.1 = total }
    }
    out
}

/// The containers (and init containers) that do not set `kind.res`.
pub fn missing(pod: &Value, key: &str) -> Vec<String> {
    let (kind, res) = match key {
        "cpu" | "memory" | "ephemeral-storage" => ("requests", key),
        k => match k.split_once('.') {
            Some((kind @ ("requests" | "limits"), res)) => (kind, res),
            _ => return Vec::new(),
        },
    };
    ["containers", "initContainers"]
        .iter()
        .flat_map(|l| pod["spec"][*l].as_array().cloned().unwrap_or_default())
        .filter(|c| c["resources"][kind].get(res).is_none())
        .map(|c| c["name"].as_str().unwrap_or("").to_string())
        .collect()
}

pub fn is_terminal(pod: &Value) -> bool {
    matches!(pod["status"]["phase"].as_str(), Some("Succeeded" | "Failed"))
}

/// What `obj`, an object of `resource` in `group` ("" for core), charges.
pub fn usage(resource: &str, group: &str, obj: &Value) -> Usage {
    let mut u = Usage::new();
    if resource == "pods" && is_terminal(obj) {
        return u;
    }
    let count = if group.is_empty() { format!("count/{resource}") } else { format!("count/{resource}.{group}") };
    add(&mut u, &count, 1);
    if group.is_empty() {
        match resource {
            "pods" => {
                add(&mut u, "pods", 1);
                let (req, lim) = pod_compute(obj);
                for (k, v) in &req {
                    add(&mut u, &format!("requests.{k}"), *v);
                    if matches!(k.as_str(), "cpu" | "memory" | "ephemeral-storage") {
                        add(&mut u, k, *v);
                    }
                }
                for (k, v) in &lim {
                    add(&mut u, &format!("limits.{k}"), *v);
                }
            }
            "services" => {
                add(&mut u, "services", 1);
                let t = obj["spec"]["type"].as_str().unwrap_or("ClusterIP");
                if t == "LoadBalancer" {
                    add(&mut u, "services.loadbalancers", 1);
                }
                let np = obj["spec"]["ports"].as_array().into_iter().flatten()
                    .filter(|p| p["nodePort"].as_i64().unwrap_or(0) != 0).count();
                if np > 0 {
                    add(&mut u, "services.nodeports", np as i128);
                }
            }
            "persistentvolumeclaims" => {
                add(&mut u, "persistentvolumeclaims", 1);
                let storage = parse("requests.storage", &obj["spec"]["resources"]["requests"]["storage"]);
                add(&mut u, "requests.storage", storage);
                if let Some(class) = obj["spec"]["storageClassName"].as_str().filter(|c| !c.is_empty()) {
                    add(&mut u, &format!("{class}.storageclass.storage.k8s.io/requests.storage"), storage);
                    add(&mut u, &format!("{class}.storageclass.storage.k8s.io/persistentvolumeclaims"), 1);
                }
            }
            "secrets" | "configmaps" | "replicationcontrollers" | "resourcequotas" => add(&mut u, resource, 1),
            _ => {}
        }
    }
    u
}

fn best_effort(pod: &Value) -> bool {
    let (req, lim) = pod_compute(pod);
    ["cpu", "memory"].iter().all(|r| req.get(*r).copied().unwrap_or(0) == 0 && lim.get(*r).copied().unwrap_or(0) == 0)
        && ["containers", "initContainers"].iter().all(|l| {
            pod["spec"][*l].as_array().into_iter().flatten().all(|c| {
                ["requests", "limits"].iter().all(|k| ["cpu", "memory"].iter().all(|r| c["resources"][*k].get(*r).is_none()))
            })
        })
}

/// Does a quota with these scopes cover `obj`?
pub fn matches(quota: &Value, resource: &str, group: &str, obj: &Value) -> bool {
    let mut scopes: Vec<(String, String, Vec<String>)> = quota["spec"]["scopes"]
        .as_array().into_iter().flatten()
        .filter_map(|s| s.as_str().map(|s| (s.to_string(), "Exists".to_string(), Vec::new())))
        .collect();
    for e in quota["spec"]["scopeSelector"]["matchExpressions"].as_array().into_iter().flatten() {
        let values = e["values"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
        scopes.push((e["scopeName"].as_str().unwrap_or("").into(), e["operator"].as_str().unwrap_or("Exists").into(), values));
    }
    if scopes.is_empty() {
        return true;
    }
    if !(resource == "pods" && group.is_empty()) {
        return false;
    }
    scopes.iter().all(|(name, op, values)| match name.as_str() {
        "Terminating" => obj["spec"]["activeDeadlineSeconds"].as_i64().is_some_and(|d| d >= 0),
        "NotTerminating" => obj["spec"]["activeDeadlineSeconds"].is_null(),
        "BestEffort" => best_effort(obj),
        "NotBestEffort" => !best_effort(obj),
        "PriorityClass" => {
            let pc = obj["spec"]["priorityClassName"].as_str().unwrap_or("");
            match op.as_str() {
                "In" => values.iter().any(|v| v == pc),
                "NotIn" => !values.iter().any(|v| v == pc),
                "Exists" => !pc.is_empty(),
                "DoesNotExist" => pc.is_empty(),
                _ => false,
            }
        }
        _ => false,
    })
}

/// `usage`, kept to the resources a quota's `hard` names.
pub fn tracked(quota: &Value, u: &Usage) -> Usage {
    let hard = quota["spec"]["hard"].as_object();
    u.iter()
        .filter(|(k, _)| hard.is_some_and(|h| h.contains_key(*k)))
        .map(|(k, v)| (k.clone(), *v))
        .collect()
}

/// A usage as a ResourceList, for every key of `hard` (absent ones 0).
pub fn resource_list(hard: &Map<String, Value>, u: &Usage) -> Value {
    let mut m = Map::new();
    for k in hard.keys() {
        m.insert(k.clone(), json!(format(k, u.get(k).copied().unwrap_or(0))));
    }
    Value::Object(m)
}

/// Two ResourceLists with the same amounts, whatever their spelling.
pub fn same(a: &Value, b: &Value) -> bool {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else { return a == b };
    a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| parse(k, v) == parse(k, w)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pod_charges_its_compute_and_its_count() {
        let pod = json!({"spec": {
            "overhead": {"cpu": "100m"},
            "initContainers": [{"name": "i", "resources": {"requests": {"cpu": "2"}}}],
            "containers": [
                {"name": "a", "resources": {"requests": {"cpu": "500m", "memory": "64Mi"}, "limits": {"memory": "128Mi"}}},
                {"name": "b", "resources": {"requests": {"cpu": "250m"}}}]}});
        let u = usage("pods", "", &pod);
        assert_eq!(u["pods"], 1);
        assert_eq!(u["count/pods"], 1);
        assert_eq!(u["requests.cpu"], 2100, "init container larger than the sum, plus overhead");
        assert_eq!(u["cpu"], 2100);
        assert_eq!(u["requests.memory"], 64 << 20);
        assert_eq!(u["limits.memory"], 128 << 20);
        assert_eq!(missing(&pod, "limits.cpu"), vec!["a", "b", "i"]);
        assert_eq!(missing(&pod, "requests.cpu"), Vec::<String>::new());
        let mut done = pod.clone();
        done["status"] = json!({"phase": "Succeeded"});
        assert!(usage("pods", "", &done).is_empty(), "a finished pod charges nothing");
    }

    #[test]
    fn objects_charge_counts_services_and_claims_their_extras() {
        let svc = json!({"spec": {"type": "LoadBalancer", "ports": [{"port": 80, "nodePort": 30080}, {"port": 81, "nodePort": 30081}]}});
        let u = usage("services", "", &svc);
        assert_eq!((u["services"], u["services.loadbalancers"], u["services.nodeports"], u["count/services"]), (1, 1, 2, 1));
        let pvc = json!({"spec": {"storageClassName": "gold", "resources": {"requests": {"storage": "1Gi"}}}});
        let u = usage("persistentvolumeclaims", "", &pvc);
        assert_eq!(u["requests.storage"], 1 << 30);
        assert_eq!(u["gold.storageclass.storage.k8s.io/requests.storage"], 1 << 30);
        assert_eq!(u["gold.storageclass.storage.k8s.io/persistentvolumeclaims"], 1);
        assert_eq!(usage("replicasets", "apps", &json!({}))["count/replicasets.apps"], 1);
        assert_eq!(usage("secrets", "", &json!({}))["secrets"], 1);
        assert_eq!(format("requests.cpu", 1500), "1500m");
        assert_eq!(format("requests.cpu", 2000), "2");
        assert!(same(&json!({"cpu": "1", "memory": "1Gi"}), &json!({"cpu": "1000m", "memory": "1073741824"})));
    }

    #[test]
    fn scopes_cover_pods_only() {
        let be = json!({"spec": {"containers": [{"name": "c"}]}});
        let burst = json!({"spec": {"activeDeadlineSeconds": 60, "priorityClassName": "high",
            "containers": [{"name": "c", "resources": {"requests": {"cpu": "1"}}}]}});
        let q = |scopes: Value| json!({"spec": {"scopes": scopes}});
        assert!(matches(&q(json!(["BestEffort"])), "pods", "", &be));
        assert!(!matches(&q(json!(["BestEffort"])), "pods", "", &burst));
        assert!(matches(&q(json!(["NotBestEffort", "Terminating"])), "pods", "", &burst));
        assert!(!matches(&q(json!(["Terminating"])), "pods", "", &be));
        assert!(matches(&q(json!(["NotTerminating"])), "pods", "", &be));
        assert!(!matches(&q(json!(["BestEffort"])), "services", "", &json!({})));
        assert!(matches(&json!({"spec": {}}), "services", "", &json!({})), "no scopes: everything");
        let pc = json!({"spec": {"scopeSelector": {"matchExpressions": [{"scopeName": "PriorityClass", "operator": "In", "values": ["high"]}]}}});
        assert!(matches(&pc, "pods", "", &burst) && !matches(&pc, "pods", "", &be));
    }
}
