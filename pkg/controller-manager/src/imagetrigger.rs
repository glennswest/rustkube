//! The image trigger controller (#386, sc#162): what openshift-controller-
//! manager's `pkg/image/trigger` does — when an ImageStreamTag moves, every
//! object that asked to follow it is moved too.
//!
//! - **Annotation triggers** (`image.openshift.io/triggers`, which `oc set
//!   triggers deploy/web --from-image=web:latest -c web` writes): on
//!   Deployments, DaemonSets, StatefulSets and CronJobs, a JSON list of
//!   `{from: {kind: ImageStreamTag, name, namespace}, fieldPath, paused}`.
//!   The tag's current pull spec is written into the container image the
//!   `fieldPath` names (`spec.template.spec.containers[?(@.name=="web")].image`,
//!   or `initContainers`, or an index), and the workload's own controller
//!   rolls it out. Jobs are not covered, as upstream's sources do not cover
//!   them: a Job's pod template is immutable.
//! - **DeploymentConfigs**: each automatic `ImageChange` trigger's tag is
//!   written into the containers it names and into its
//!   `lastTriggeredImage` (`UpdateDeploymentConfigImages`); the
//!   `deploymentconfig` controller then rolls out a version, cause
//!   `ImageChange`.
//! - **BuildConfigs**: an `ImageChange` trigger (its own `from`, or the
//!   strategy's when it has none) whose tag now points at an image other
//!   than its last triggered one instantiates a build with
//!   `triggeredByImage` (`buildRequestForImageChange`); `instantiate`
//!   records the image. A BuildConfig whose ConfigChange trigger has not
//!   run yet is left to it: that first build records every trigger's image.
//!
//! **ImageStreams are the registry's** (#385): they are listed and watched
//! through the aggregator at `/apis/image.openshift.io/v1/imagestreams`,
//! and only while the `v1.image.openshift.io` APIService is Available — on
//! a cluster without the registry this controller watches nothing more
//! than APIServices. Streams are followed by their own reflector, keyed by
//! namespace and name: one the registry shows only because something was
//! pushed has no uid, which the indexed informers require.
//!
//! Ported from openshift/openshift-controller-manager `pkg/image/trigger`
//! (`annotations`, `deploymentconfigs`, `buildconfigs`, read 2026-11-25).

use crate::runner::ApiClient;
use apimachinery::imagestream;
use apimachinery::informer::Key;
use apimachinery::reflector::Change;
use apimachinery::workqueue::WorkQueue;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{info, warn};

/// The annotation `oc set triggers` writes on a workload.
pub const TRIGGER_ANNOTATION: &str = "image.openshift.io/triggers";
const IMAGE_API_SERVICE: &str = "v1.image.openshift.io";
const STREAMS_PATH: &str = "/apis/image.openshift.io/v1/imagestreams";
const APISERVICES_PATH: &str = "/apis/apiregistration.k8s.io/v1/apiservices";
/// `buildutil.BuildTriggerCauseImageMsg`.
const IMAGE_CAUSE: &str = "Image change";

/// What an object's triggers are, and so how a moved tag changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The annotation; the pod spec at this path, named by `fieldPath`s
    /// that start with `base`.
    Annotation { spec: &'static [&'static str], base: &'static str },
    DeploymentConfig,
    BuildConfig,
}

/// One collection whose objects carry image triggers.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub api: &'static str,
    pub resource: &'static str,
    pub kind: Kind,
}

const TEMPLATE: Kind = Kind::Annotation { spec: &["spec", "template", "spec"], base: "spec.template.spec." };

/// Upstream's trigger sources.
pub const SOURCES: [Source; 6] = [
    Source { api: "/apis/apps/v1", resource: "deployments", kind: TEMPLATE },
    Source { api: "/apis/apps/v1", resource: "daemonsets", kind: TEMPLATE },
    Source { api: "/apis/apps/v1", resource: "statefulsets", kind: TEMPLATE },
    Source {
        api: "/apis/batch/v1",
        resource: "cronjobs",
        kind: Kind::Annotation {
            spec: &["spec", "jobTemplate", "spec", "template", "spec"],
            base: "spec.jobTemplate.spec.template.spec.",
        },
    },
    Source { api: "/apis/apps.openshift.io/v1", resource: "deploymentconfigs", kind: Kind::DeploymentConfig },
    Source { api: "/apis/build.openshift.io/v1", resource: "buildconfigs", kind: Kind::BuildConfig },
];

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn ns_of(o: &Value) -> &str {
    s(&o["metadata"], "namespace")
}

/// An ImageStreamTag reference an object follows: namespace, stream, tag.
pub type TagRef = (String, String, String);

fn tag_ref(from: &Value, default_ns: &str) -> Option<TagRef> {
    if s(from, "kind") != "ImageStreamTag" || s(from, "name").is_empty() {
        return None;
    }
    let ns = match s(from, "namespace") {
        "" => default_ns,
        n => n,
    };
    let (stream, tag, _) = imagestream::split_tag(s(from, "name"));
    Some((ns.to_string(), stream.to_string(), tag.to_string()))
}

/// The annotation's triggers, or none when it does not parse.
fn annotation_triggers(obj: &Value) -> Vec<Value> {
    obj["metadata"]["annotations"][TRIGGER_ANNOTATION]
        .as_str()
        .and_then(|a| serde_json::from_str::<Vec<Value>>(a).ok())
        .unwrap_or_default()
}

/// The `from` of a BuildConfig's ImageChange trigger: its own, else the
/// strategy's (`GetInputReference`).
fn bc_trigger_from<'a>(bc: &'a Value, trigger: &'a Value) -> &'a Value {
    let own = &trigger["imageChange"]["from"];
    if own.is_object() {
        return own;
    }
    let st = &bc["spec"]["strategy"];
    for k in ["sourceStrategy", "dockerStrategy", "customStrategy"] {
        if st[k]["from"].is_object() {
            return &st[k]["from"];
        }
    }
    &Value::Null
}

/// Every tag an object follows (paused or not: a change to one is a reason
/// to look at the object).
pub fn references(kind: Kind, obj: &Value) -> Vec<TagRef> {
    let ns = ns_of(obj);
    match kind {
        Kind::Annotation { .. } => annotation_triggers(obj).iter().filter_map(|t| tag_ref(&t["from"], ns)).collect(),
        Kind::DeploymentConfig => obj["spec"]["triggers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "ImageChange")
            .filter_map(|t| tag_ref(&t["imageChangeParams"]["from"], ns))
            .collect(),
        Kind::BuildConfig => obj["spec"]["triggers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "ImageChange")
            .filter_map(|t| tag_ref(bc_trigger_from(obj, t), ns))
            .collect(),
    }
}

fn at_mut<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    path.iter().try_fold(v, |v, k| v.get_mut(*k))
}

/// `UpdateObjectFromImages`: the object with every unpaused annotation
/// trigger's tag written into its field, or `None` when nothing changes.
pub fn apply_annotation(
    obj: &Value,
    spec_path: &[&str],
    base: &str,
    resolve: &dyn Fn(&TagRef) -> Option<String>,
) -> Option<Value> {
    let triggers = annotation_triggers(obj);
    let mut updated = obj.clone();
    let mut changed = false;
    for t in &triggers {
        if t["paused"] == true {
            continue;
        }
        let Some(r) = tag_ref(&t["from"], ns_of(obj)) else { continue };
        let Some(path) = s(t, "fieldPath").strip_prefix(base) else { continue };
        let Some(c) = imagestream::parse_container_path(path).filter(|c| c.rest == "image") else { continue };
        let Some(image) = resolve(&r) else { continue };
        let Some(container) = at_mut(&mut updated, spec_path).and_then(|spec| imagestream::container_mut(spec, &c)) else {
            continue;
        };
        if container["image"] != image.as_str() {
            container["image"] = json!(image);
            changed = true;
        }
    }
    changed.then_some(updated)
}

/// `UpdateDeploymentConfigImages`: every automatic ImageChange trigger's
/// tag into the containers it names and its `lastTriggeredImage`.
pub fn apply_deployment_config(dc: &Value, resolve: &dyn Fn(&TagRef) -> Option<String>) -> Option<Value> {
    let mut updated = dc.clone();
    let mut changed = false;
    let triggers = dc["spec"]["triggers"].as_array().cloned().unwrap_or_default();
    for (i, t) in triggers.iter().enumerate() {
        let p = &t["imageChangeParams"];
        if t["type"] != "ImageChange" || p["automatic"] != true {
            continue;
        }
        let Some(r) = tag_ref(&p["from"], ns_of(dc)) else { continue };
        let last = s(p, "lastTriggeredImage");
        let image = match resolve(&r) {
            Some(i) => i,
            None if last.is_empty() => continue,
            None => last.to_string(),
        };
        if last != image {
            updated["spec"]["triggers"][i]["imageChangeParams"]["lastTriggeredImage"] = json!(image);
            changed = true;
        }
        let names: Vec<&str> = p["containerNames"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        for list in ["containers", "initContainers"] {
            for c in updated["spec"]["template"]["spec"][list].as_array_mut().into_iter().flatten() {
                if names.contains(&s(c, "name")) && c["image"] != image.as_str() {
                    c["image"] = json!(image);
                    changed = true;
                }
            }
        }
    }
    changed.then_some(updated)
}

/// The image a BuildConfig last built from for this trigger: its
/// `status.imageChangeTriggers` entry, else the deprecated spec field.
fn bc_last_triggered(bc: &Value, trigger: &Value, from: &Value) -> String {
    let own = trigger["imageChange"]["from"].is_object();
    let status = bc["status"]["imageChangeTriggers"].as_array().into_iter().flatten().find(|st| {
        if own {
            st["from"]["name"] == from["name"] && s(&st["from"], "namespace") == s(from, "namespace")
        } else {
            st["from"]["name"].is_null() || st["from"]["name"] == from["name"]
        }
    });
    match status.map(|st| s(st, "lastTriggeredImageID")).filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => s(&trigger["imageChange"], "lastTriggeredImageID").to_string(),
    }
}

/// `buildRequestForImageChange`: the BuildRequest the first unpaused
/// ImageChange trigger whose tag moved asks for, if any.
pub fn build_request(bc: &Value, resolve: &dyn Fn(&TagRef) -> Option<String>) -> Option<Value> {
    if bc["metadata"]["annotations"]["openshift.io/build-config.paused"] == "true" {
        return None;
    }
    let triggers = bc["spec"]["triggers"].as_array()?;
    let config_change = triggers.iter().any(|t| t["type"] == "ConfigChange");
    if config_change && bc["status"]["lastVersion"].as_i64().unwrap_or(0) == 0 {
        return None;
    }
    for t in triggers {
        if t["type"] != "ImageChange" || t["imageChange"]["paused"] == true {
            continue;
        }
        let from = bc_trigger_from(bc, t);
        let Some(r) = tag_ref(from, ns_of(bc)) else { continue };
        let Some(image) = resolve(&r) else { continue };
        if bc_last_triggered(bc, t, from) == image {
            continue;
        }
        let from_ref = json!({"kind": "ImageStreamTag", "name": s(from, "name"), "namespace": r.0});
        return Some(json!({
            "kind": "BuildRequest",
            "apiVersion": "build.openshift.io/v1",
            "metadata": {"name": s(&bc["metadata"], "name"), "namespace": ns_of(bc)},
            "triggeredBy": [{"message": IMAGE_CAUSE, "imageChangeBuild": {"imageID": image, "fromRef": from_ref}}],
            "triggeredByImage": {"kind": "DockerImage", "name": image},
            "from": from_ref,
        }));
    }
    None
}

/// The ImageStreams seen, by namespace and name.
#[derive(Default)]
struct Streams(RwLock<HashMap<(String, String), Value>>);

impl Streams {
    fn resolve(&self, r: &TagRef) -> Option<String> {
        let map = self.0.read().unwrap();
        imagestream::resolve_latest_tagged_image(map.get(&(r.0.clone(), r.1.clone()))?, &r.2)
    }
}

fn stream_id(obj: &Value) -> Option<(String, String)> {
    let name = s(&obj["metadata"], "name");
    (!name.is_empty()).then(|| (ns_of(obj).to_string(), name.to_string()))
}

/// Work: a stream that moved (fanned out to its followers), or one object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Item {
    Stream(String, String),
    Object(usize, Key),
}

pub struct ImageTriggerController {
    api: Arc<ApiClient>,
}

impl ImageTriggerController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    /// Follow the image API's APIService; run while it is Available.
    pub async fn run(&self) {
        let changed = Arc::new(tokio::sync::Notify::new());
        let notify = changed.clone();
        let services = self.api.informers.subscribe(
            &self.api.client,
            format!("{}{APISERVICES_PATH}", self.api.base_url),
            move |_, _| notify.notify_one(),
        );
        let mut running: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            if services.feed.ensure_synced().is_ok() {
                let available = services
                    .feed
                    .select(&apimachinery::informer::Index::Name("".into(), IMAGE_API_SERVICE.into()))
                    .ok()
                    .and_then(|v| v.into_iter().next())
                    .is_some_and(|a| {
                        a["status"]["conditions"].as_array().into_iter().flatten()
                            .any(|c| c["type"] == "Available" && c["status"] == "True")
                    });
                match (available, running.is_some()) {
                    (true, false) => {
                        info!("image triggers: {IMAGE_API_SERVICE} is Available, following ImageStreams");
                        let api = self.api.clone();
                        running = Some(tokio::spawn(async move { follow(api).await }));
                    }
                    (false, true) => {
                        info!("image triggers: {IMAGE_API_SERVICE} is not Available, stopped");
                        if let Some(t) = running.take() {
                            t.abort();
                        }
                    }
                    _ => {}
                }
            }
            changed.notified().await;
        }
    }
}

/// Aborts the task it holds when dropped (the controller's term ends).
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The streams' reflector, the sources' feeds, and the work they make.
async fn follow(api: Arc<ApiClient>) {
    let queue = WorkQueue::<Item>::new();
    let streams = Arc::new(Streams::default());
    let subs: Vec<_> = SOURCES
        .iter()
        .enumerate()
        .map(|(i, src)| {
            let q = queue.clone();
            let kind = src.kind;
            api.informers.subscribe(&api.client, format!("{}{}/{}", api.base_url, src.api, src.resource), move |changes, _| {
                for c in changes {
                    if let Some(o) = c.new.as_ref().filter(|o| !references(kind, o).is_empty()) {
                        if let Ok(key) = Key::of(o) {
                            q.add(Item::Object(i, key));
                        }
                    }
                }
            })
        })
        .collect();
    // Held for the life of this loop: dropping it stops the reflector.
    let _reflector = {
        let (q, streams, client, url) = (queue.clone(), streams.clone(), api.client.clone(), format!("{}{STREAMS_PATH}", api.base_url));
        AbortOnDrop(tokio::spawn(async move {
            apimachinery::reflector::run_events(client, url, move |change| {
                let mut map = streams.0.write().unwrap();
                match change {
                    Change::Reset { snapshot, .. } => {
                        let mut next = HashMap::new();
                        for o in snapshot["items"].as_array().into_iter().flatten() {
                            if let Some(id) = stream_id(o) {
                                next.insert(id, o.clone());
                            }
                        }
                        for id in map.keys().chain(next.keys()) {
                            if map.get(id).map(|o| &o["status"]) != next.get(id).map(|o| &o["status"]) {
                                q.add(Item::Stream(id.0.clone(), id.1.clone()));
                            }
                        }
                        *map = next;
                    }
                    Change::Applied { event, semantic_change } => {
                        let o = &event["object"];
                        if let Some(id) = stream_id(o) {
                            if event["type"] == "DELETED" {
                                map.remove(&id);
                            } else {
                                map.insert(id.clone(), o.clone());
                            }
                            if semantic_change {
                                q.add(Item::Stream(id.0, id.1));
                            }
                        }
                    }
                    Change::Connected | Change::Unavailable => {}
                }
                true
            })
            .await
        }))
    };
    let mut failures: HashMap<Item, u32> = HashMap::new();
    loop {
        let work = queue.next().await;
        let item = work.key().clone();
        let result = match &item {
            Item::Stream(ns, name) => {
                for (i, sub) in subs.iter().enumerate() {
                    for o in sub.feed.list().unwrap_or_default() {
                        if references(SOURCES[i].kind, &o).iter().any(|r| &r.0 == ns && &r.1 == name) {
                            if let Ok(key) = Key::of(&o) {
                                queue.add(Item::Object(i, key));
                            }
                        }
                    }
                }
                Ok(())
            }
            Item::Object(i, key) => match subs[*i].feed.get(key) {
                Ok(Some(o)) => reconcile(&api, &SOURCES[*i], &o, &streams).await,
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            },
        };
        match result {
            Ok(()) => {
                failures.remove(&item);
            }
            Err(e) => {
                let n = failures.entry(item.clone()).or_default();
                *n = n.saturating_add(1);
                warn!(?item, error = %format!("{e:#}"), "image trigger failed");
                let delay = Duration::from_millis((100_u64 << (*n).min(8)).min(30_000));
                queue.add_at(item, tokio::time::Instant::now() + delay);
            }
        }
        drop(work);
    }
}

async fn reconcile(api: &ApiClient, src: &Source, obj: &Value, streams: &Streams) -> anyhow::Result<()> {
    if !obj["metadata"]["deletionTimestamp"].is_null() {
        return Ok(());
    }
    let resolve = |r: &TagRef| streams.resolve(r);
    let (ns, name) = (ns_of(obj), s(&obj["metadata"], "name"));
    let path = format!("{}/namespaces/{ns}/{}/{name}", src.api, src.resource);
    match src.kind {
        Kind::Annotation { spec, base } => {
            if let Some(updated) = apply_annotation(obj, spec, base, &resolve) {
                api.update(&path, &updated).await?;
                info!("{} {ns}/{name}: image updated from its trigger", src.resource);
            }
        }
        Kind::DeploymentConfig => {
            if let Some(updated) = apply_deployment_config(obj, &resolve) {
                api.update(&path, &updated).await?;
                info!("DeploymentConfig {ns}/{name}: image trigger resolved");
            }
        }
        Kind::BuildConfig => {
            if let Some(req) = build_request(obj, &resolve) {
                match api.create(&format!("{path}/instantiate"), &req).await {
                    Ok(b) => info!("BuildConfig {ns}/{name}: {} started (image change)", s(&b["metadata"], "name")),
                    // Another pass got there first: instantiate refuses the
                    // same image twice, as upstream's does.
                    Err(e) if e.to_string().contains("has already instantiated a build for imageid")
                        || e.to_string().contains("is paused") => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: &str = "node1.lo:5000/p/web@sha256:1111";

    fn resolve(r: &TagRef) -> Option<String> {
        (r == &("p".to_string(), "web".to_string(), "latest".to_string())).then(|| IMG.to_string())
    }

    fn deployment(field: &str, paused: bool) -> Value {
        let triggers = json!([{"from": {"kind": "ImageStreamTag", "name": "web:latest"}, "fieldPath": field, "paused": paused}]);
        json!({"metadata": {"name": "web", "namespace": "p", "uid": "u1",
                            "annotations": {TRIGGER_ANNOTATION: triggers.to_string()}},
               "spec": {"template": {"spec": {"containers": [{"name": "side", "image": "busybox"}, {"name": "web", "image": "old"}]}}}})
    }

    #[test]
    fn annotation_trigger_writes_the_field_it_names() {
        let Kind::Annotation { spec, base } = TEMPLATE else { unreachable!() };
        let d = deployment("spec.template.spec.containers[?(@.name==\"web\")].image", false);
        assert_eq!(references(TEMPLATE, &d), vec![("p".into(), "web".into(), "latest".into())]);
        let out = apply_annotation(&d, spec, base, &resolve).unwrap();
        assert_eq!(out["spec"]["template"]["spec"]["containers"][1]["image"], IMG);
        assert_eq!(out["spec"]["template"]["spec"]["containers"][0]["image"], "busybox");
        assert!(apply_annotation(&out, spec, base, &resolve).is_none(), "already there: no write");
        assert!(apply_annotation(&deployment("spec.template.spec.containers[?(@.name==\"web\")].image", true), spec, base, &resolve).is_none(),
                "paused");
        assert!(apply_annotation(&deployment("spec.containers[0].image", false), spec, base, &resolve).is_none(), "outside the base path");
        assert!(apply_annotation(&deployment("spec.template.spec.containers[?(@.name==\"nope\")].image", false), spec, base, &resolve).is_none());
        let by_index = apply_annotation(&deployment("spec.template.spec.containers[0].image", false), spec, base, &resolve).unwrap();
        assert_eq!(by_index["spec"]["template"]["spec"]["containers"][0]["image"], IMG);
        let mut d = deployment("x", false);
        d["metadata"]["annotations"][TRIGGER_ANNOTATION] = json!("not json");
        assert!(references(TEMPLATE, &d).is_empty());
    }

    #[test]
    fn cronjob_base_path() {
        let Kind::Annotation { spec, base } = SOURCES[3].kind else { unreachable!() };
        let triggers = json!([{"from": {"kind": "ImageStreamTag", "name": "web:latest", "namespace": "p"},
                               "fieldPath": "spec.jobTemplate.spec.template.spec.containers[?(@.name==\"job\")].image"}]);
        let cj = json!({"metadata": {"name": "c", "namespace": "other", "annotations": {TRIGGER_ANNOTATION: triggers.to_string()}},
                        "spec": {"jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": "job", "image": "x"}]}}}}}});
        let out = apply_annotation(&cj, spec, base, &resolve).unwrap();
        assert_eq!(out["spec"]["jobTemplate"]["spec"]["template"]["spec"]["containers"][0]["image"], IMG, "cross-namespace from");
    }

    fn dc(automatic: bool, last: &str) -> Value {
        json!({"metadata": {"name": "web", "namespace": "p"},
               "spec": {"triggers": [{"type": "ConfigChange"},
                                     {"type": "ImageChange", "imageChangeParams": {"automatic": automatic, "containerNames": ["web"],
                                      "from": {"kind": "ImageStreamTag", "name": "web:latest"}, "lastTriggeredImage": last}}],
                        "template": {"spec": {"containers": [{"name": "web", "image": " "}, {"name": "other", "image": "keep"}]}}}})
    }

    #[test]
    fn deployment_config_triggers() {
        let out = apply_deployment_config(&dc(true, ""), &resolve).unwrap();
        assert_eq!(out["spec"]["triggers"][1]["imageChangeParams"]["lastTriggeredImage"], IMG);
        assert_eq!(out["spec"]["template"]["spec"]["containers"][0]["image"], IMG);
        assert_eq!(out["spec"]["template"]["spec"]["containers"][1]["image"], "keep");
        assert!(apply_deployment_config(&out, &resolve).is_none());
        assert!(apply_deployment_config(&dc(false, ""), &resolve).is_none(), "a manual trigger waits for oc rollout latest");
        let none = |_: &TagRef| None;
        assert!(apply_deployment_config(&dc(true, ""), &none).is_none(), "unresolvable, nothing yet");
        // The tag gone: the last image is kept in the containers.
        let mut d = dc(true, IMG);
        d["spec"]["template"]["spec"]["containers"][0]["image"] = json!("drifted");
        let out = apply_deployment_config(&d, &none).unwrap();
        assert_eq!(out["spec"]["template"]["spec"]["containers"][0]["image"], IMG);
    }

    fn bc(last_version: i64, last: &str, own_from: bool) -> Value {
        let mut t = json!({"type": "ImageChange", "imageChange": {}});
        if own_from {
            t["imageChange"]["from"] = json!({"kind": "ImageStreamTag", "name": "web:latest"});
        }
        json!({"metadata": {"name": "b", "namespace": "p"},
               "spec": {"triggers": [{"type": "ConfigChange"}, t],
                        "strategy": {"type": "Source", "sourceStrategy": {"from": {"kind": "ImageStreamTag", "name": "web:latest"}}}},
               "status": {"lastVersion": last_version,
                          "imageChangeTriggers": [{"lastTriggeredImageID": last, "from": {"name": "web:latest"}}]}})
    }

    #[test]
    fn build_config_triggers() {
        assert!(build_request(&bc(0, "", false), &resolve).is_none(), "the ConfigChange build goes first");
        let req = build_request(&bc(1, "older", false), &resolve).unwrap();
        assert_eq!(req["triggeredByImage"], json!({"kind": "DockerImage", "name": IMG}));
        assert_eq!(req["from"]["name"], "web:latest");
        assert_eq!(req["triggeredBy"][0]["imageChangeBuild"]["imageID"], IMG);
        assert!(build_request(&bc(1, IMG, false), &resolve).is_none(), "already built from it");
        assert!(build_request(&bc(1, "older", true), &resolve).is_some(), "an own from");
        let mut paused = bc(1, "older", false);
        paused["spec"]["triggers"][1]["imageChange"]["paused"] = json!(true);
        assert!(build_request(&paused, &resolve).is_none());
        let mut spec_only = bc(1, "", false);
        spec_only["status"]["imageChangeTriggers"] = json!([]);
        spec_only["spec"]["triggers"][1]["imageChange"]["lastTriggeredImageID"] = json!(IMG);
        assert!(build_request(&spec_only, &resolve).is_none(), "the deprecated spec field is read too");
    }
}
