//! `short` (< 2 min): the control plane is up on this node and does its main
//! job. What the standard names for rustkube — create and read an object —
//! and the three processes behind it, each by what it does rather than by
//! asking it:
//!
//! - the apiserver answers over TLS, verified with the cluster CA, and takes
//!   the run's ServiceAccount token;
//! - a write lands in the datastore (fastetcd): read back with the store's
//!   revision as its `resourceVersion`, and seen by a watch;
//! - the controller-manager did its per-namespace work on the run's own
//!   namespace (its `default` ServiceAccount and `kube-root-ca.crt`);
//! - a ReplicaSet gets its pod from the controller-manager and the pod a
//!   node from the scheduler. Running it is the kubelet's job (rustkube-node's
//!   tests), not this one's.

use crate::env::Env;
use crate::kube::{brief, rv, Kube};
use crate::report::{fail, pass, Outcome, Report};
use crate::workload;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

pub async fn run(env: &Env, kube: &Kube, rep: &mut Report) -> Result<()> {
    rep.check("apiserver-ready", apiserver_ready(kube)).await?;
    rep.check("object-roundtrip", object_roundtrip(kube)).await?;
    rep.check("watch-sees-writes", watch_sees_writes(kube)).await?;
    rep.check("namespace-provisioned", namespace_provisioned(env, kube)).await?;
    rep.check("replicaset-pod-bound", replicaset_pod_bound(env, kube)).await?;
    Ok(())
}

async fn apiserver_ready(kube: &Kube) -> Result<Outcome> {
    let r = kube.req("GET", "/readyz", "", None, Some("text/plain")).await?;
    if r.status != 200 || r.body.as_str().map(str::trim) != Some("ok") {
        return fail(format!("/readyz: {} {}", r.status, brief(&r.body)));
    }
    let v = kube.get("/version").await?.unwrap_or(Value::Null);
    let Some(ver) = v["gitVersion"].as_str() else {
        return fail(format!("/version has no gitVersion: {v}"));
    };
    // The token is taken: a namespaced read is a 200, not a 401 or 403
    // (preflight already did one; this says so in the result).
    pass(format!("ready, {ver}, TLS verified with the cluster CA, token accepted"))
}

async fn object_roundtrip(kube: &Kube) -> Result<Outcome> {
    let cms = kube.path("", "v1", "configmaps");
    let path = format!("{cms}/short-roundtrip");
    kube.delete(&path).await?;
    let mut body = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": kube.meta("short-roundtrip"), "data": {"k": "v1"}});
    let made = kube.create(&cms, body.clone()).await?;
    let (Some(rv1), Some(uid)) = (rv(&made), made["metadata"]["uid"].as_str().filter(|u| !u.is_empty())) else {
        return fail(format!("created without a numeric resourceVersion or a uid: {}", made["metadata"]));
    };
    let got = kube.get(&path).await?.unwrap_or(Value::Null);
    if got["data"]["k"] != "v1" || got["metadata"]["uid"] != uid {
        return fail(format!("read back something else: {got}"));
    }
    body["data"]["k"] = json!("v2");
    body["metadata"]["resourceVersion"] = json!(rv1.to_string());
    let r = kube.put(&path, &body).await?;
    if !r.ok() {
        return fail(format!("update: {} {}", r.status, brief(&r.body)));
    }
    let rv2 = rv(&r.body).unwrap_or(0);
    if rv2 <= rv1 || r.body["data"]["k"] != "v2" {
        return fail(format!("update: resourceVersion {rv1} -> {rv2}, data {}", r.body["data"]));
    }
    // The same update again is one version behind now: a 409, not a write.
    let stale = kube.put(&path, &body).await?;
    if stale.status != 409 {
        return fail(format!("a stale update was {} (want 409)", stale.status));
    }
    kube.delete(&path).await?;
    let (_, gone) = kube.wait_for(&path, Duration::from_secs(10), |v| v.is_null()).await?;
    if !gone {
        return fail("deleted, and still there after 10 s");
    }
    pass(format!("create, read, update (rv {rv1} -> {rv2}), stale update 409, delete"))
}

async fn watch_sees_writes(kube: &Kube) -> Result<Outcome> {
    let cms = kube.path("", "v1", "configmaps");
    let list = kube.get(&cms).await?.unwrap_or(Value::Null);
    let from = list["metadata"]["resourceVersion"].as_str().unwrap_or("0").to_string();
    let name = "short-watch";
    let mut w = kube
        .watch(&format!("{cms}?watch=1&resourceVersion={from}&fieldSelector=metadata.name%3D{name}&timeoutSeconds=60"))
        .await?;
    kube.create(&cms, json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": kube.meta(name)})).await?;
    let is = |t: &'static str| move |e: &Value| e["type"] == t && e["object"]["metadata"]["name"] == name;
    if w.until(Duration::from_secs(10), is("ADDED")).await?.is_none() {
        kube.delete(&format!("{cms}/{name}")).await?;
        return fail("no ADDED event within 10 s of the create");
    }
    kube.delete(&format!("{cms}/{name}")).await?;
    if w.until(Duration::from_secs(10), is("DELETED")).await?.is_none() {
        return fail("no DELETED event within 10 s of the delete");
    }
    pass("ADDED and DELETED seen")
}

async fn namespace_provisioned(env: &Env, kube: &Kube) -> Result<Outcome> {
    let within = Duration::from_secs(30).min(env.left());
    let sa = format!("{}/default", kube.path("", "v1", "serviceaccounts"));
    let (_, has_sa) = kube.wait_for(&sa, within, |v| !v.is_null()).await?;
    let ca = format!("{}/kube-root-ca.crt", kube.path("", "v1", "configmaps"));
    let (cm, has_ca) = kube.wait_for(&ca, within, |v| v["data"]["ca.crt"].as_str().is_some_and(|s| s.contains("BEGIN CERTIFICATE"))).await?;
    match (has_sa, has_ca) {
        (true, true) => pass("default ServiceAccount and kube-root-ca.crt are there"),
        (false, _) => fail(format!("no default ServiceAccount in {} after {within:?}", kube.ns)),
        (_, false) => fail(format!("no kube-root-ca.crt with a certificate after {within:?}: {}", cm["data"])),
    }
}

async fn replicaset_pod_bound(env: &Env, kube: &Kube) -> Result<Outcome> {
    let image = workload::own_image(kube).await?;
    let rss = kube.path("apps", "v1", "replicasets");
    let name = "short-rs";
    kube.delete(&format!("{rss}/{name}")).await?;
    let rs = kube
        .create(
            &rss,
            json!({"apiVersion": "apps/v1", "kind": "ReplicaSet", "metadata": kube.meta(name),
                   "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}},
                            "template": workload::template(&kube.run, &image, name)}}),
        )
        .await?;
    let uid = rs["metadata"]["uid"].as_str().unwrap_or("").to_string();
    let within = Duration::from_secs(60).min(env.left());
    let start = std::time::Instant::now();
    let outcome = loop {
        let pods = workload::pods_of(kube, name).await?;
        let owned: Vec<&Value> = pods
            .iter()
            .filter(|p| p["metadata"]["ownerReferences"].as_array().is_some_and(|o| o.iter().any(|r| r["uid"] == uid.as_str())))
            .collect();
        if let Some(node) = owned.iter().find_map(|p| p["spec"]["nodeName"].as_str().filter(|n| !n.is_empty())) {
            break pass(format!("pod created by the controller-manager and bound to {node} by the scheduler in {:?}", start.elapsed()));
        }
        if start.elapsed() >= within {
            break if owned.is_empty() {
                // Say why (#96: the first live run on pvetest2 failed here
                // with nothing to go on): read the ReplicaSet and its events
                // before the delete below removes them.
                let now = kube.get(&format!("{rss}/{name}")).await?;
                let events = kube
                    .get(&format!(
                        "{}?fieldSelector=involvedObject.uid%3D{uid}",
                        kube.path("", "v1", "events")
                    ))
                    .await?
                    .and_then(|l| l["items"].as_array().cloned())
                    .unwrap_or_default();
                fail(no_pod_detail(now.as_ref(), &events, within))
            } else {
                fail(format!("its pod was not bound to a node in {within:?}"))
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    kube.delete(&format!("{rss}/{name}")).await?;
    outcome
}

/// What a ReplicaSet that made no pod shows: whether the controller-manager
/// saw it at all (`status.observedGeneration`), its `ReplicaFailure`
/// condition, and its `FailedCreate`-style Warning events — the reason a
/// create was refused (admission, quota, a missing ServiceAccount).
fn no_pod_detail(rs: Option<&Value>, events: &[Value], within: Duration) -> String {
    let mut d = format!("the ReplicaSet made no pod in {within:?}");
    let Some(rs) = rs else {
        d.push_str("; it was gone before it could be read");
        return d;
    };
    let observed = rs["status"]["observedGeneration"].as_i64();
    match observed {
        None => d.push_str("; the controller-manager never observed it (no status.observedGeneration)"),
        Some(g) => d.push_str(&format!("; observed at generation {g}")),
    }
    if let Some(c) = rs["status"]["conditions"]
        .as_array()
        .and_then(|cs| cs.iter().find(|c| c["type"] == "ReplicaFailure" && c["status"] == "True"))
    {
        d.push_str(&format!(
            "; ReplicaFailure {}: {}",
            c["reason"].as_str().unwrap_or("?"),
            c["message"].as_str().unwrap_or("")
        ));
    }
    let warnings: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "Warning")
        .map(|e| format!("{}: {}", e["reason"].as_str().unwrap_or("?"), e["message"].as_str().unwrap_or("")))
        .collect();
    if !warnings.is_empty() {
        d.push_str(&format!("; events: {}", warnings.join(" | ")));
    } else if observed.is_some() {
        d.push_str("; no Warning event");
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_secs(60);

    #[test]
    fn never_observed() {
        let rs = json!({"metadata": {"name": "short-rs"}, "status": {}});
        let d = no_pod_detail(Some(&rs), &[], W);
        assert!(d.contains("never observed it"), "{d}");
        assert!(!d.contains("no Warning event"), "{d}");
    }

    #[test]
    fn refused_create_names_the_reason() {
        let rs = json!({"status": {"observedGeneration": 1, "conditions": [
            {"type": "ReplicaFailure", "status": "True", "reason": "FailedCreate", "message": "pods \"x\" is forbidden: exceeded quota"}]}});
        let ev = vec![
            json!({"type": "Normal", "reason": "Ignored", "message": "n"}),
            json!({"type": "Warning", "reason": "FailedCreate", "message": "Error creating: forbidden"}),
        ];
        let d = no_pod_detail(Some(&rs), &ev, W);
        assert!(d.contains("observed at generation 1"), "{d}");
        assert!(d.contains("ReplicaFailure FailedCreate: pods \"x\" is forbidden: exceeded quota"), "{d}");
        assert!(d.contains("events: FailedCreate: Error creating: forbidden"), "{d}");
        assert!(!d.contains("Ignored"), "{d}");
    }

    #[test]
    fn observed_but_silent() {
        let rs = json!({"status": {"observedGeneration": 1, "replicas": 0}});
        let d = no_pod_detail(Some(&rs), &[], W);
        assert!(d.ends_with("observed at generation 1; no Warning event"), "{d}");
    }

    #[test]
    fn gone() {
        assert!(no_pod_detail(None, &[], W).contains("gone before it could be read"));
    }
}
