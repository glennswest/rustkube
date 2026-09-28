//! `medium` (< 30 min): the control plane's features and failure paths, end
//! to end, as a client sees them. It carries the checks of the retired
//! `stormcos_qa/tests/rustkube/*.sh` that fit a namespace (stormcos_qa#25),
//! and the regressions fixed since: conditional status writes (#78), watch
//! DELETED for a selector (#100), LIST item resourceVersions (#111),
//! GC propagation (#43, #99).
//!
//! The run's Role is `*` in its own namespace and nothing else, so the
//! cluster-scoped checks those scripts made — CRDs, Nodes, the
//! leader-election Leases in `kube-system` — are reported as one skip.
//!
//! The last group needs a kubelet that runs pods (a Deployment rolled out,
//! its Service's EndpointSlice, a Job, a DaemonSet): on a stormcos test
//! machine it has one.

use crate::env::Env;
use crate::kube::{brief, rv, Kube, APPLY, JSON, JSON_PATCH, MERGE, STRATEGIC};
use crate::report::{fail, pass, skip, Outcome, Report};
use crate::workload;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub async fn run(env: &Env, kube: &Kube, rep: &mut Report) -> Result<()> {
    rep.check("server-side-apply", server_side_apply(kube)).await?;
    rep.check("json-patch-test-null", json_patch_test_null(kube)).await?;
    rep.check("strategic-merge-by-key", strategic_merge_by_key(kube)).await?;
    rep.check("status-write-conditional", status_write_conditional(kube)).await?;
    rep.check("delete-options", delete_options(kube)).await?;
    rep.check("label-field-selectors", selectors(kube)).await?;
    rep.check("list-paging", list_paging(kube)).await?;
    rep.check("generate-name", generate_name(kube)).await?;
    rep.check("watch-initial-events", watch_initial_events(kube)).await?;
    rep.check("watch-selector-leave", watch_selector_leave(kube)).await?;
    rep.check("partial-object-metadata", partial_object_metadata(kube)).await?;
    rep.check("events-translation", events_translation(kube)).await?;
    rep.check("cluster-scoped", async {
        skip("CRD lifecycle, Nodes Ready, leader-election Leases: need cluster-scoped access; the run's Role is namespaced")
    })
    .await?;

    let image = workload::own_image(kube).await?;
    rep.check("gc-orphan", gc_orphan(env, kube, &image)).await?;
    rep.check("gc-foreground", gc_foreground(env, kube, &image)).await?;
    rep.check("deployment-rollout-running", rollout(env, kube, &image)).await?;
    rep.check("service-endpointslice", endpointslice(env, kube)).await?;
    rep.check("job-completes", job_completes(env, kube, &image)).await?;
    rep.check("daemonset-steady", daemonset(env, kube, &image)).await?;
    Ok(())
}

fn cms(kube: &Kube) -> String {
    kube.path("", "v1", "configmaps")
}

async fn configmap(kube: &Kube, name: &str, labels: Value, data: Value) -> Result<Value> {
    kube.delete(&format!("{}/{name}", cms(kube))).await?;
    let mut meta = kube.meta(name);
    if let Value::Object(m) = labels {
        for (k, v) in m {
            meta["labels"][k] = v;
        }
    }
    kube.create(&cms(kube), json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": meta, "data": data})).await
}

/// Within `want`, but never past the suite's deadline.
fn within(env: &Env, want: u64) -> Duration {
    Duration::from_secs(want).min(env.left())
}

async fn server_side_apply(kube: &Kube) -> Result<Outcome> {
    let path = format!("{}/ssa", cms(kube));
    kube.delete(&path).await?;
    let obj = |v: &str| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "ssa", "labels": {"storm.io/test-run": kube.run}}, "data": {"k": v}});
    let r = kube.patch(&format!("{path}?fieldManager=alice"), APPLY, &obj("a")).await?;
    if !r.ok() {
        return fail(format!("apply by alice (upsert): {} {}", r.status, brief(&r.body)));
    }
    let managers: Vec<&str> = r.body["metadata"]["managedFields"].as_array().into_iter().flatten().filter_map(|m| m["manager"].as_str()).collect();
    if !managers.contains(&"alice") {
        return fail(format!("managedFields names no alice: {managers:?}"));
    }
    let r = kube.patch(&format!("{path}?fieldManager=bob"), APPLY, &obj("b")).await?;
    if r.status != 409 {
        return fail(format!("bob applying alice's field: {} (want 409 conflict)", r.status));
    }
    let r = kube.patch(&format!("{path}?fieldManager=bob&force=true"), APPLY, &obj("b")).await?;
    if !r.ok() || r.body["data"]["k"] != "b" {
        return fail(format!("bob with force: {} {}", r.status, brief(&r.body)));
    }
    kube.delete(&path).await?;
    pass("upsert, managedFields, conflict 409, force takes the field")
}

async fn json_patch_test_null(kube: &Kube) -> Result<Outcome> {
    configmap(kube, "jsonpatch", json!({}), json!({"k": "v"})).await?;
    let path = format!("{}/jsonpatch", cms(kube));
    // `test` of null against an absent path holds (client-go leans on it for
    // compare-and-set on optional fields), and the `add` after it applies.
    let r = kube
        .patch(&path, JSON_PATCH, &json!([{"op": "test", "path": "/data/absent", "value": null}, {"op": "add", "path": "/data/absent", "value": "now"}]))
        .await?;
    if !r.ok() || r.body["data"]["absent"] != "now" {
        return fail(format!("test-null then add: {} {}", r.status, brief(&r.body)));
    }
    // A test that does not hold refuses the whole patch.
    let r = kube
        .patch(&path, JSON_PATCH, &json!([{"op": "test", "path": "/data/k", "value": "other"}, {"op": "remove", "path": "/data/k"}]))
        .await?;
    if r.ok() {
        return fail("a failing test op still applied the patch");
    }
    kube.delete(&path).await?;
    pass(format!("test-null holds; a failing test is {}", r.status))
}

async fn strategic_merge_by_key(kube: &Kube) -> Result<Outcome> {
    // A Deployment at zero replicas: containers merge by name.
    let deps = kube.path("apps", "v1", "deployments");
    let path = format!("{deps}/smp");
    kube.delete(&path).await?;
    let c = |n: &str, img: &str| json!({"name": n, "image": img, "command": ["/test"], "args": ["idle"]});
    let mut tpl = workload::template(&kube.run, "unused:1", "smp");
    tpl["spec"]["containers"] = json!([c("a", "a:1"), c("b", "b:1")]);
    kube.create(&deps, json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": kube.meta("smp"),
        "spec": {"replicas": 0, "selector": {"matchLabels": {"app": "smp"}}, "template": tpl}}))
        .await?;
    let r = kube.patch(&path, STRATEGIC, &json!({"spec": {"template": {"spec": {"containers": [{"name": "b", "image": "b:2"}]}}}})).await?;
    let cs = r.body["spec"]["template"]["spec"]["containers"].clone();
    kube.delete(&path).await?;
    if !r.ok() {
        return fail(format!("patch: {} {}", r.status, brief(&r.body)));
    }
    let img = |n: &str| cs.as_array().and_then(|a| a.iter().find(|x| x["name"] == n)).map(|x| x["image"].clone());
    if img("a") != Some(json!("a:1")) || img("b") != Some(json!("b:2")) {
        return fail(format!("containers after the patch: {cs}"));
    }
    if cs.as_array().and_then(|a| a.iter().find(|x| x["name"] == "b")).map(|x| x["command"][0].clone()) != Some(json!("/test")) {
        return fail(format!("the patched container lost its other fields: {cs}"));
    }
    pass("list merged by name: a kept, b updated, b's other fields kept")
}

async fn status_write_conditional(kube: &Kube) -> Result<Outcome> {
    let svcs = kube.path("", "v1", "services");
    let path = format!("{svcs}/status-rv");
    kube.delete(&path).await?;
    let made = kube
        .create(&svcs, json!({"apiVersion": "v1", "kind": "Service", "metadata": kube.meta("status-rv"),
            "spec": {"ports": [{"port": 80, "protocol": "TCP"}], "selector": {"app": "none"}}}))
        .await?;
    let mut body = made.clone();
    body["status"] = json!({"loadBalancer": {"ingress": [{"hostname": "first.example"}]}});
    let r = kube.put(&format!("{path}/status"), &body).await?;
    if !r.ok() {
        kube.delete(&path).await?;
        return fail(format!("status PUT at the current resourceVersion: {} {}", r.status, brief(&r.body)));
    }
    body["status"] = json!({"loadBalancer": {"ingress": [{"hostname": "stale.example"}]}});
    let stale = kube.put(&format!("{path}/status"), &body).await?;
    let now = kube.get(&path).await?.unwrap_or(Value::Null);
    kube.delete(&path).await?;
    if stale.status != 409 {
        return fail(format!("a stale status PUT was {} (want 409)", stale.status));
    }
    if now["status"]["loadBalancer"]["ingress"][0]["hostname"] != "first.example" {
        return fail(format!("the stale status was written: {}", now["status"]));
    }
    pass("current resourceVersion lands; stale is 409 and changes nothing")
}

async fn delete_options(kube: &Kube) -> Result<Outcome> {
    let made = configmap(kube, "delopts", json!({}), json!({})).await?;
    let path = format!("{}/delopts", cms(kube));
    let wrong = json!({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": "00000000-0000-0000-0000-000000000000"}});
    let r = kube.req("DELETE", &path, JSON, Some(&wrong), None).await?;
    if r.status != 409 {
        return fail(format!("delete with a wrong uid precondition: {} (want 409)", r.status));
    }
    let r = kube.req("DELETE", &format!("{path}?dryRun=All"), JSON, None, None).await?;
    if !r.ok() || kube.get(&path).await?.is_none() {
        return fail(format!("dryRun delete: {} and the object is {}", r.status, "gone"));
    }
    let right = json!({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": made["metadata"]["uid"]}});
    let r = kube.req("DELETE", &path, JSON, Some(&right), None).await?;
    if !r.ok() {
        return fail(format!("delete with the right uid: {} {}", r.status, brief(&r.body)));
    }
    pass("wrong uid 409, dryRun keeps the object, right uid deletes")
}

async fn selectors(kube: &Kube) -> Result<Outcome> {
    for (n, tier) in [("sel-a", "web"), ("sel-b", "web"), ("sel-c", "db")] {
        configmap(kube, n, json!({"tier": tier}), json!({})).await?;
    }
    let names = |v: Option<Value>| -> Vec<String> {
        let mut n: Vec<String> = v.unwrap_or(Value::Null)["items"].as_array().into_iter().flatten().filter_map(|i| i["metadata"]["name"].as_str().map(str::to_string)).collect();
        n.sort();
        n
    };
    let web = names(kube.get(&format!("{}?labelSelector=tier%3Dweb", cms(kube))).await?);
    let notweb = names(kube.get(&format!("{}?labelSelector=tier%2Ctier%21%3Dweb", cms(kube))).await?);
    let byname = names(kube.get(&format!("{}?fieldSelector=metadata.name%3Dsel-b", cms(kube))).await?);
    for n in ["sel-a", "sel-b", "sel-c"] {
        kube.delete(&format!("{}/{n}", cms(kube))).await?;
    }
    if web != ["sel-a", "sel-b"] || notweb != ["sel-c"] || byname != ["sel-b"] {
        return fail(format!("tier=web {web:?}, tier,tier!=web {notweb:?}, metadata.name=sel-b {byname:?}"));
    }
    pass("equality, existence + inequality, and a field selector")
}

async fn list_paging(kube: &Kube) -> Result<Outcome> {
    for i in 0..7 {
        configmap(kube, &format!("page-{i}"), json!({"paging": "yes"}), json!({})).await?;
    }
    let mut seen = Vec::new();
    let mut cont = String::new();
    let mut pages = 0;
    let mut problem = None;
    loop {
        let mut q = format!("{}?labelSelector=paging%3Dyes&limit=3", cms(kube));
        if !cont.is_empty() {
            q.push_str(&format!("&continue={}", urlencode(&cont)));
        }
        let page = kube.get(&q).await?.unwrap_or(Value::Null);
        pages += 1;
        for i in page["items"].as_array().into_iter().flatten() {
            if rv(i).is_none() {
                problem.get_or_insert_with(|| format!("item {} has no resourceVersion (#111)", i["metadata"]["name"]));
            }
            seen.push(i["metadata"]["name"].as_str().unwrap_or("").to_string());
        }
        cont = page["metadata"]["continue"].as_str().unwrap_or("").to_string();
        if cont.is_empty() || pages > 10 {
            break;
        }
    }
    for i in 0..7 {
        kube.delete(&format!("{}/page-{i}", cms(kube))).await?;
    }
    seen.sort();
    seen.dedup();
    if let Some(p) = problem {
        return fail(p);
    }
    if seen.len() != 7 || pages != 3 {
        return fail(format!("{} distinct items in {pages} pages of 3 (want 7 in 3)", seen.len()));
    }
    pass("7 items in 3 pages, each with its resourceVersion")
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn generate_name(kube: &Kube) -> Result<Outcome> {
    let mut names = Vec::new();
    for _ in 0..2 {
        let v = kube
            .create(&cms(kube), json!({"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"generateName": "gen-", "labels": {"storm.io/test-run": kube.run}}}))
            .await?;
        names.push(v["metadata"]["name"].as_str().unwrap_or("").to_string());
    }
    for n in &names {
        kube.delete(&format!("{}/{n}", cms(kube))).await?;
    }
    if names[0] == names[1] || !names.iter().all(|n| n.starts_with("gen-") && n.len() > 4) {
        return fail(format!("generated {names:?}"));
    }
    pass(format!("two distinct names: {names:?}"))
}

async fn watch_initial_events(kube: &Kube) -> Result<Outcome> {
    for n in ["wl-a", "wl-b"] {
        configmap(kube, n, json!({"watchlist": "yes"}), json!({})).await?;
    }
    let mut w = kube
        .watch(&format!(
            "{}?watch=1&labelSelector=watchlist%3Dyes&sendInitialEvents=true&allowWatchBookmarks=true&resourceVersionMatch=NotOlderThan&timeoutSeconds=30",
            cms(kube)
        ))
        .await?;
    let mut added = Vec::new();
    let mut bookmark = false;
    while let Some(ev) = w.next(Duration::from_secs(15)).await? {
        match ev["type"].as_str() {
            Some("ADDED") => added.push(ev["object"]["metadata"]["name"].as_str().unwrap_or("").to_string()),
            Some("BOOKMARK") if ev["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"] == "true" => {
                bookmark = true;
                break;
            }
            _ => {}
        }
    }
    for n in ["wl-a", "wl-b"] {
        kube.delete(&format!("{}/{n}", cms(kube))).await?;
    }
    added.sort();
    if added != ["wl-a", "wl-b"] || !bookmark {
        return fail(format!("initial ADDED {added:?}, initial-events-end bookmark: {bookmark}"));
    }
    pass("both objects, then the initial-events-end bookmark")
}

async fn watch_selector_leave(kube: &Kube) -> Result<Outcome> {
    let list = kube.get(&cms(kube)).await?.unwrap_or(Value::Null);
    let from = list["metadata"]["resourceVersion"].as_str().unwrap_or("0").to_string();
    let mut w = kube
        .watch(&format!("{}?watch=1&resourceVersion={from}&labelSelector=phase%3Din&timeoutSeconds=30", cms(kube)))
        .await?;
    configmap(kube, "leaver", json!({"phase": "in"}), json!({})).await?;
    let path = format!("{}/leaver", cms(kube));
    let named = |t: &'static str| move |e: &Value| e["type"] == t && e["object"]["metadata"]["name"] == "leaver";
    let added = w.until(Duration::from_secs(10), named("ADDED")).await?.is_some();
    kube.patch(&path, MERGE, &json!({"metadata": {"labels": {"phase": "out"}}})).await?;
    let left = w.until(Duration::from_secs(10), named("DELETED")).await?.is_some();
    kube.delete(&path).await?;
    match (added, left) {
        (true, true) => pass("ADDED on create, DELETED when it stopped matching"),
        (false, _) => fail("no ADDED for an object created matching the selector"),
        (_, false) => fail("no DELETED when the object stopped matching (#100)"),
    }
}

async fn partial_object_metadata(kube: &Kube) -> Result<Outcome> {
    configmap(kube, "pom", json!({"pom": "yes"}), json!({"secret-ish": "not in the projection"})).await?;
    let r = kube
        .req("GET", &format!("{}?labelSelector=pom%3Dyes", cms(kube)), JSON, None, Some("application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1"))
        .await?;
    kube.delete(&format!("{}/pom", cms(kube))).await?;
    let item = &r.body["items"][0];
    if r.body["kind"] != "PartialObjectMetadataList" || item["metadata"]["name"] != "pom" || !item["data"].is_null() {
        return fail(format!("{}: kind {}, item {}", r.status, r.body["kind"], item));
    }
    pass("PartialObjectMetadataList with metadata only")
}

async fn events_translation(kube: &Kube) -> Result<Outcome> {
    let evs = kube.path("", "v1", "events");
    kube.delete(&format!("{evs}/translate")).await?;
    kube.create(&evs, json!({"apiVersion": "v1", "kind": "Event", "metadata": kube.meta("translate"),
        "involvedObject": {"kind": "ConfigMap", "name": "anything", "namespace": kube.ns, "apiVersion": "v1"},
        "reason": "Tested", "message": "hello from core", "type": "Normal", "source": {"component": "rustkube-test"}}))
        .await?;
    let v = kube.get(&format!("{}/translate", kube.path("events.k8s.io", "v1", "events"))).await?.unwrap_or(Value::Null);
    kube.delete(&format!("{evs}/translate")).await?;
    if v["note"] != "hello from core" || v["regarding"]["name"] != "anything" || v["reason"] != "Tested" {
        return fail(format!("events.k8s.io/v1 view: note {}, regarding {}, reason {}", v["note"], v["regarding"], v["reason"]));
    }
    pass("a core/v1 Event reads as events.k8s.io/v1 (message → note, involvedObject → regarding)")
}

async fn deployment(kube: &Kube, name: &str, image: &str, replicas: u32) -> Result<Value> {
    let deps = kube.path("apps", "v1", "deployments");
    kube.delete(&format!("{deps}/{name}")).await?;
    kube.create(&deps, json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": kube.meta(name),
        "spec": {"replicas": replicas, "selector": {"matchLabels": {"app": name}}, "template": workload::template(&kube.run, image, name)}}))
        .await
}

/// ReplicaSets whose controller owner has `uid`.
async fn owned_rs(kube: &Kube, uid: &str) -> Result<Vec<Value>> {
    let l = kube.get(&kube.path("apps", "v1", "replicasets")).await?.unwrap_or(Value::Null);
    Ok(l["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["metadata"]["ownerReferences"].as_array().is_some_and(|o| o.iter().any(|x| x["uid"] == uid)))
        .cloned()
        .collect())
}

async fn wait_owned_rs(env: &Env, kube: &Kube, uid: &str) -> Result<Option<Value>> {
    let end = Instant::now() + within(env, 60);
    loop {
        if let Some(r) = owned_rs(kube, uid).await?.into_iter().next() {
            return Ok(Some(r));
        }
        if Instant::now() >= end {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn gc_orphan(env: &Env, kube: &Kube, image: &str) -> Result<Outcome> {
    let d = deployment(kube, "gc-orphan", image, 1).await?;
    let uid = d["metadata"]["uid"].as_str().unwrap_or("").to_string();
    let Some(rs) = wait_owned_rs(env, kube, &uid).await? else {
        return fail("the Deployment made no ReplicaSet in 60 s");
    };
    let rs_name = rs["metadata"]["name"].as_str().unwrap_or("").to_string();
    let rs_path = format!("{}/{rs_name}", kube.path("apps", "v1", "replicasets"));
    let opts = json!({"apiVersion": "v1", "kind": "DeleteOptions", "propagationPolicy": "Orphan"});
    kube.req("DELETE", &format!("{}/gc-orphan", kube.path("apps", "v1", "deployments")), JSON, Some(&opts), None).await?;
    let (v, freed) = kube
        .wait_for(&rs_path, within(env, 30), |v| {
            !v.is_null() && !v["metadata"]["ownerReferences"].as_array().is_some_and(|o| o.iter().any(|x| x["uid"] == uid.as_str()))
        })
        .await?;
    // Still there ten seconds on: orphaned, not collected late.
    tokio::time::sleep(Duration::from_secs(10).min(env.left())).await;
    let still = kube.get(&rs_path).await?.is_some();
    kube.delete(&rs_path).await?;
    match (freed, still) {
        (true, true) => pass(format!("ReplicaSet {rs_name} kept, its ownerReference removed")),
        (false, _) if v.is_null() => fail(format!("orphan delete removed ReplicaSet {rs_name}")),
        (false, _) => fail(format!("ReplicaSet {rs_name} still owned after 30 s: {}", v["metadata"]["ownerReferences"])),
        (true, false) => fail(format!("ReplicaSet {rs_name} was orphaned, then collected anyway")),
    }
}

async fn gc_foreground(env: &Env, kube: &Kube, image: &str) -> Result<Outcome> {
    let d = deployment(kube, "gc-fg", image, 1).await?;
    let uid = d["metadata"]["uid"].as_str().unwrap_or("").to_string();
    let Some(rs) = wait_owned_rs(env, kube, &uid).await? else {
        return fail("the Deployment made no ReplicaSet in 60 s");
    };
    let rs_path = format!("{}/{}", kube.path("apps", "v1", "replicasets"), rs["metadata"]["name"].as_str().unwrap_or(""));
    let dep_path = format!("{}/gc-fg", kube.path("apps", "v1", "deployments"));
    let opts = json!({"apiVersion": "v1", "kind": "DeleteOptions", "propagationPolicy": "Foreground"});
    kube.req("DELETE", &dep_path, JSON, Some(&opts), None).await?;
    let (_, rs_gone) = kube.wait_for(&rs_path, within(env, 90), |v| v.is_null()).await?;
    let (_, dep_gone) = kube.wait_for(&dep_path, within(env, 30), |v| v.is_null()).await?;
    let pods_left = workload::pods_of(kube, "gc-fg").await?.len();
    match (rs_gone, dep_gone, pods_left) {
        (true, true, 0) => pass("Deployment, ReplicaSet and pods all gone"),
        _ => fail(format!("after a foreground delete: ReplicaSet gone {rs_gone}, Deployment gone {dep_gone}, {pods_left} pods left")),
    }
}

async fn rollout(env: &Env, kube: &Kube, image: &str) -> Result<Outcome> {
    deployment(kube, "web", image, 2).await?;
    let path = format!("{}/web", kube.path("apps", "v1", "deployments"));
    let (v, ready) = kube.wait_for(&path, within(env, 180), |v| v["status"]["readyReplicas"] == 2).await?;
    if !ready {
        let pods = workload::pods_of(kube, "web").await?;
        let phases: Vec<&str> = pods.iter().map(|p| p["status"]["phase"].as_str().unwrap_or("?")).collect();
        return fail(format!("readyReplicas {} after 180 s; pods {phases:?} (a kubelet runs them)", v["status"]["readyReplicas"]));
    }
    // A template change rolls a new ReplicaSet out and the old one down.
    let gen = v["metadata"]["generation"].as_i64().unwrap_or(0);
    kube.patch(&path, STRATEGIC, &json!({"spec": {"template": {"metadata": {"annotations": {"rustkube-test/rev": "2"}}}}})).await?;
    let (v, done) = kube
        .wait_for(&path, within(env, 180), |v| {
            v["status"]["observedGeneration"].as_i64().unwrap_or(0) > gen
                && v["status"]["updatedReplicas"] == 2
                && v["status"]["readyReplicas"] == 2
                && v["status"]["replicas"] == 2
        })
        .await?;
    if !done {
        return fail(format!("rollout not complete after 180 s: {}", v["status"]));
    }
    pass("2 replicas Ready, then rolled to a new template")
}

async fn endpointslice(env: &Env, kube: &Kube) -> Result<Outcome> {
    // Fronts the Deployment `web` the rollout left running.
    let svcs = kube.path("", "v1", "services");
    kube.delete(&format!("{svcs}/web")).await?;
    kube.create(&svcs, json!({"apiVersion": "v1", "kind": "Service", "metadata": kube.meta("web"),
        "spec": {"selector": {"app": "web"}, "ports": [{"port": 80, "targetPort": 8080, "protocol": "TCP"}]}}))
        .await?;
    let q = format!("{}?labelSelector=kubernetes.io%2Fservice-name%3Dweb", kube.path("discovery.k8s.io", "v1", "endpointslices"));
    let ready = |l: &Value| -> usize {
        l["items"].as_array().into_iter().flatten().flat_map(|s| s["endpoints"].as_array().cloned().unwrap_or_default()).filter(|e| e["conditions"]["ready"] == true).count()
    };
    let (l, ok) = kube.wait_for(&q, within(env, 60), |l| ready(l) == 2).await?;
    kube.delete(&format!("{svcs}/web")).await?;
    kube.delete(&format!("{}/web", kube.path("apps", "v1", "deployments"))).await?;
    if !ok {
        return fail(format!("{} ready endpoints after 60 s (want the 2 pods of web)", ready(&l)));
    }
    pass("the Service's EndpointSlice lists both ready pods")
}

async fn job_completes(env: &Env, kube: &Kube, image: &str) -> Result<Outcome> {
    let jobs = kube.path("batch", "v1", "jobs");
    kube.delete(&format!("{jobs}/done?propagationPolicy=Background")).await?;
    let mut tpl = workload::template(&kube.run, image, "done");
    tpl["spec"]["restartPolicy"] = json!("Never");
    tpl["spec"]["containers"][0]["args"] = json!(["idle", "0"]);
    kube.create(&jobs, json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": kube.meta("done"),
        "spec": {"backoffLimit": 0, "template": tpl}}))
        .await?;
    let (v, ok) = kube
        .wait_for(&format!("{jobs}/done"), within(env, 120), |v| {
            v["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| (c["type"] == "Complete" || c["type"] == "Failed") && c["status"] == "True"))
        })
        .await?;
    kube.delete(&format!("{jobs}/done?propagationPolicy=Background")).await?;
    let complete = v["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Complete" && c["status"] == "True"));
    if !ok || !complete || v["status"]["succeeded"] != 1 {
        return fail(format!("Job status after 120 s: {}", v["status"]));
    }
    pass("succeeded 1, Complete")
}

async fn daemonset(env: &Env, kube: &Kube, image: &str) -> Result<Outcome> {
    let dss = kube.path("apps", "v1", "daemonsets");
    let path = format!("{dss}/every-node");
    kube.delete(&path).await?;
    kube.create(&dss, json!({"apiVersion": "apps/v1", "kind": "DaemonSet", "metadata": kube.meta("every-node"),
        "spec": {"selector": {"matchLabels": {"app": "every-node"}}, "template": workload::template(&kube.run, image, "every-node")}}))
        .await?;
    let (v, ok) = kube
        .wait_for(&path, within(env, 120), |v| {
            let d = v["status"]["desiredNumberScheduled"].as_i64().unwrap_or(0);
            d > 0 && v["status"]["currentNumberScheduled"].as_i64() == Some(d) && v["status"]["numberReady"].as_i64() == Some(d)
        })
        .await?;
    if !ok {
        kube.delete(&path).await?;
        return fail(format!("DaemonSet status after 120 s: {}", v["status"]));
    }
    let desired = v["status"]["desiredNumberScheduled"].as_i64().unwrap_or(0) as usize;
    // One pod per node, and the same pods twenty seconds on: no churn.
    let uids = |ps: &[Value]| -> Vec<String> {
        let mut u: Vec<String> = ps.iter().filter(|p| p["metadata"]["deletionTimestamp"].is_null()).filter_map(|p| p["metadata"]["uid"].as_str().map(str::to_string)).collect();
        u.sort();
        u
    };
    let first = workload::pods_of(kube, "every-node").await?;
    let nodes: std::collections::BTreeSet<&str> = first.iter().filter_map(|p| p["spec"]["nodeName"].as_str()).collect();
    tokio::time::sleep(Duration::from_secs(20).min(env.left())).await;
    let later = workload::pods_of(kube, "every-node").await?;
    kube.delete(&path).await?;
    if uids(&first).len() != desired || nodes.len() != desired {
        return fail(format!("{} pods on {} nodes for {desired} desired", uids(&first).len(), nodes.len()));
    }
    if uids(&first) != uids(&later) {
        return fail("the DaemonSet's pods changed within 20 s of being Ready (churn)");
    }
    pass(format!("{desired} pods, one per node, Ready and steady for 20 s"))
}
