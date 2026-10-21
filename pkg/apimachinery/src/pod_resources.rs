//! A Pod's total requests or limits from its containers, as upstream's
//! `PodRequests`/`PodLimits` (and LimitRanger's pod aggregate) add them up
//! since sidecars (#285; sidecar containers GA in 1.33).
//!
//! The regular containers run together, so they add. A sidecar — an init
//! container with `restartPolicy: Always` — keeps running beside them, so it
//! adds too. An ordinary init container runs alone, but beside every sidecar
//! started before it. The pod needs the larger of the running total and its
//! busiest init step, per resource. Overhead and pod-level `resources` are
//! the caller's: they are not the same rule everywhere.

use serde_json::Value;
use std::collections::BTreeMap;

/// `kind` is `requests` or `limits`; `amount` reads one quantity (a resource
/// name, its value).
pub fn containers_total<T>(pod: &Value, kind: &str, amount: impl Fn(&str, &Value) -> T) -> BTreeMap<String, T>
where
    T: Copy + Default + PartialOrd + std::ops::Add<Output = T>,
{
    let each = |c: &Value| -> BTreeMap<String, T> {
        c["resources"][kind]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), amount(k, v)))
            .collect()
    };
    let add = |into: &mut BTreeMap<String, T>, from: &BTreeMap<String, T>| {
        for (k, v) in from {
            let e = into.entry(k.clone()).or_default();
            *e = *e + *v;
        }
    };
    let max = |into: &mut BTreeMap<String, T>, from: &BTreeMap<String, T>| {
        for (k, v) in from {
            let e = into.entry(k.clone()).or_default();
            if *v > *e {
                *e = *v;
            }
        }
    };
    let mut total = BTreeMap::new();
    for c in pod["spec"]["containers"].as_array().into_iter().flatten() {
        add(&mut total, &each(c));
    }
    let mut sidecars = BTreeMap::new();
    let mut busiest_init = BTreeMap::new();
    for c in pod["spec"]["initContainers"].as_array().into_iter().flatten() {
        let mine = each(c);
        let step = if c["restartPolicy"].as_str() == Some("Always") {
            add(&mut total, &mine);
            add(&mut sidecars, &mine);
            sidecars.clone()
        } else {
            let mut step = mine;
            add(&mut step, &sidecars);
            step
        };
        max(&mut busiest_init, &step);
    }
    max(&mut total, &busiest_init);
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn n(_: &str, v: &Value) -> i64 {
        v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    #[test]
    fn sidecars_add_and_init_steps_run_beside_earlier_sidecars() {
        let r = |cpu: &str| json!({"requests": {"cpu": cpu}});
        let pod = json!({"spec": {
            "initContainers": [
                {"name": "setup", "resources": r("300")},
                {"name": "proxy", "restartPolicy": "Always", "resources": r("200")},
                {"name": "migrate", "resources": r("500")}
            ],
            "containers": [{"name": "app", "resources": r("400")}]
        }});
        // running: 400 + 200 = 600; busiest init step: 500 beside 200 = 700
        assert_eq!(containers_total(&pod, "requests", n)["cpu"], 700);
        let only_sidecar = json!({"spec": {
            "initContainers": [{"name": "proxy", "restartPolicy": "Always", "resources": r("200")}],
            "containers": [{"name": "app", "resources": r("400")}]
        }});
        assert_eq!(containers_total(&only_sidecar, "requests", n)["cpu"], 600, "summed, not maxed");
        let plain = json!({"spec": {
            "initContainers": [{"name": "i", "resources": r("900")}],
            "containers": [{"name": "app", "resources": r("400")}, {"name": "b", "resources": r("100")}]
        }});
        assert_eq!(containers_total(&plain, "requests", n)["cpu"], 900, "an init container larger than the sum");
        assert!(containers_total(&plain, "limits", n).is_empty());
    }
}
