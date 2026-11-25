//! Image policy admission (#385, sc#162): OpenShift's `imagepolicy` plugin
//! as its default configuration runs it — a mutating step that resolves a
//! local image name to the image it names, by digest.
//!
//! Ported from openshift/apiserver-library-go `pkg/admission/imagepolicy`
//! (`imagepolicy.go`, `accept.go`, `apis/imagepolicy/v1/defaults.go`,
//! `imagereferencemutators/{meta,pods}.go`). Under the default config
//! (`resolveImages: Attempt`, no execution rules) every rule's policy is
//! `DoNotAttempt`, so the plugin only ever rewrites a **local name**:
//!
//! - a container's (or init container's, or image volume's) image that names
//!   no registry and no namespace, `<stream>[:<tag>]`, is looked up as the
//!   ImageStreamTag `<stream>:<tag>` (`latest` by default) in the object's
//!   namespace. When the tag's `lookupPolicy.local` is true (`oc set
//!   image-lookup <stream>`), the image becomes the tag's
//!   `image.dockerImageReference` with its tag dropped and the image's name
//!   (its digest) as `@<digest>`;
//! - when the object — or its pod template — carries
//!   `alpha.image.policy.openshift.io/resolve-names: "*"` (`oc set
//!   image-lookup deploy/web` sets it on the template), every image is
//!   looked up so in the object's namespace whatever the stream's
//!   `lookupPolicy` says;
//! - a tag the stream does not have yet, in a stream that resolves (local
//!   lookup or the annotation), becomes `<status.dockerImageRepository>:<tag>`.
//!
//! What it covers: Pods, ReplicationControllers, DeploymentConfigs and the
//! `apps` (and `extensions`) workloads, Jobs and CronJobs — creates and
//! updates of the object itself, never a subresource. An object with a
//! controller owner reference is left alone (its owner's template was
//! resolved), and on an update an image the stored object already has, in a
//! container of the same name, is not looked up again unless the
//! annotation came or went. A Job is not rewritten on update. Builds are not
//! covered: a Build's `from` is its own (#386).
//!
//! **ImageStreams are the registry's** (#385, decided 2026-11-25): the
//! `image.openshift.io/v1` APIService (stormblock-registry#235, #258) serves
//! them, and this step reads them through the aggregator as
//! `system:apiserver`. A resolution that fails — no APIService, the registry
//! down or slow, a 403, no such tag — **leaves the image as it was**; this
//! step never refuses a write (upstream's `Attempt`). After the first
//! failure to reach the registry, the rest of the write's images are not
//! looked up.

use crate::admission::{Operation, RequestAttrs};
use crate::handlers::AppState;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

/// The annotation `oc set image-lookup <workload>` sets.
pub const RESOLVE_NAMES: &str = "alpha.image.policy.openshift.io/resolve-names";

/// Where ImageStreams are served.
const IMAGE_API: &str = "/apis/image.openshift.io/v1";

/// How long one lookup may take: a pod write waits on it.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// A covered resource: where its pod spec is, and its template's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    /// Path to the pod spec from the object's root.
    spec: &'static [&'static str],
    /// Path to the pod template's `metadata`, for a workload.
    template: Option<&'static [&'static str]>,
    /// Not rewritten on update (upstream's `skipImageRewriteOnUpdate`).
    create_only: bool,
}

const TEMPLATE_SPEC: &[&str] = &["spec", "template", "spec"];
const TEMPLATE_META: &[&str] = &["spec", "template", "metadata"];

/// The resources upstream's default `resolutionRules` name (builds aside).
pub(crate) fn target(group: &str, resource: &str) -> Option<Target> {
    let workload = Target { spec: TEMPLATE_SPEC, template: Some(TEMPLATE_META), create_only: false };
    Some(match (group, resource) {
        ("", "pods") => Target { spec: &["spec"], template: None, create_only: false },
        ("", "replicationcontrollers")
        | ("apps.openshift.io", "deploymentconfigs")
        | ("apps" | "extensions", "daemonsets" | "deployments" | "replicasets")
        | ("apps", "statefulsets") => workload,
        ("batch", "jobs") => Target { create_only: true, ..workload },
        ("batch", "cronjobs") => Target {
            spec: &["spec", "jobTemplate", "spec", "template", "spec"],
            template: Some(&["spec", "jobTemplate", "spec", "template", "metadata"]),
            create_only: false,
        },
        _ => return None,
    })
}

fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(v, |v, k| &v[*k])
}

fn at_mut<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    path.iter().try_fold(v, |v, k| v.get_mut(*k))
}

/// `ResolveAllNames`: the template's annotation, then the object's own.
fn resolve_all(target: &Target, obj: &Value) -> bool {
    let star = |meta: &Value| meta["annotations"][RESOLVE_NAMES].as_str() == Some("*");
    target.template.is_some_and(|t| star(at(obj, t))) || star(&obj["metadata"])
}

/// An image reference as library-go's `reference.Parse` splits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ImageRef {
    pub registry: String,
    pub namespace: String,
    pub name: String,
    pub tag: String,
    pub id: String,
}

impl ImageRef {
    pub(crate) fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        if spec.is_empty() || spec.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        let (rest, id) = match spec.split_once('@') {
            Some((r, d)) if !d.is_empty() && d.contains(':') => (r, d.to_string()),
            Some(_) => return None,
            None => (spec, String::new()),
        };
        let last_slash = rest.rfind('/').map(|i| i + 1).unwrap_or(0);
        let (path, tag) = match rest[last_slash..].rfind(':') {
            Some(i) => (&rest[..last_slash + i], rest[last_slash + i + 1..].to_string()),
            None => (rest, String::new()),
        };
        if path.is_empty() || path.split('/').any(str::is_empty) {
            return None;
        }
        let mut r = ImageRef { tag, id, ..Default::default() };
        let mut name = path;
        if let Some((first, after)) = path.split_once('/') {
            if first.contains(['.', ':']) || first == "localhost" {
                r.registry = first.to_string();
                name = after;
            }
        }
        match name.split_once('/') {
            Some((ns, n)) => {
                r.namespace = ns.to_string();
                r.name = n.to_string();
            }
            None => r.name = name.to_string(),
        }
        Some(r)
    }

    /// `Exact()`: the digest when there is one, else the tag.
    pub(crate) fn exact(&self) -> String {
        let mut out = [&self.registry, &self.namespace, &self.name]
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("/");
        if !self.id.is_empty() {
            out.push('@');
            out.push_str(&self.id);
        } else if !self.tag.is_empty() {
            out.push(':');
            out.push_str(&self.tag);
        }
        out
    }
}

/// What a lookup through the aggregator came back with.
#[derive(Debug, Clone)]
pub enum Fetched {
    Found(Value),
    NotFound,
    /// The registry could not answer: no APIService, down, slow, refused.
    Failed(String),
}

/// A stream name or tag safe to put in a request path.
fn path_safe(s: &str, tag: bool) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_') || (tag && c.is_ascii_uppercase()))
        && s != "."
        && s != ".."
}

/// `resolveImageReference` for a reference without a digest, under the
/// default config: `Ok(Some(image))` to rewrite, `Ok(None)` to leave it,
/// `Err` when the registry could not answer (the image is left too).
pub(crate) async fn resolve<F, Fut>(get: &F, image: &str, namespace: &str, force: bool) -> Result<Option<String>, String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    let Some(mut r) = ImageRef::parse(image) else { return Ok(None) };
    // By digest: only checked against execution rules, which the default
    // config has none of. A full reference is never rewritten either: only
    // a local name sets `LocalRewrite`.
    if !r.id.is_empty() {
        return Ok(None);
    }
    let partial = force || (r.registry.is_empty() && r.namespace.is_empty());
    if !partial {
        return Ok(None);
    }
    if r.tag.is_empty() {
        r.tag = "latest".into();
    }
    if !path_safe(&r.name, false) || !path_safe(&r.tag, true) {
        return Ok(None);
    }
    let base = format!("{IMAGE_API}/namespaces/{namespace}");
    match get(format!("{base}/imagestreamtags/{}:{}", r.name, r.tag)).await {
        Fetched::Found(ist) => {
            if !force && ist["lookupPolicy"]["local"].as_bool() != Some(true) {
                return Ok(None);
            }
            let Some(mut to) = ist["image"]["dockerImageReference"].as_str().and_then(ImageRef::parse) else {
                return Ok(None);
            };
            let digest = ist["image"]["metadata"]["name"].as_str().filter(|d| d.contains(':'));
            to.tag.clear();
            match digest {
                Some(d) => to.id = d.to_string(),
                None if !to.id.is_empty() => {}
                None => return Ok(None),
            }
            Ok(Some(to.exact()))
        }
        Fetched::NotFound => match get(format!("{base}/imagestreams/{}", r.name)).await {
            Fetched::Found(stream) => {
                let local = force || stream["spec"]["lookupPolicy"]["local"].as_bool() == Some(true);
                let repo = stream["status"]["dockerImageRepository"].as_str().filter(|s| !s.is_empty());
                match (local, repo.and_then(ImageRef::parse)) {
                    (true, Some(mut to)) => {
                        to.tag = r.tag;
                        to.id.clear();
                        Ok(Some(to.exact()))
                    }
                    _ => Ok(None),
                }
            }
            Fetched::NotFound => Ok(None),
            Fetched::Failed(e) => Err(e),
        },
        Fetched::Failed(e) => Err(e),
    }
}

/// One image field of a pod spec: (list, entry name, pointer to the image).
fn image_fields(spec: &Value) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    for list in ["initContainers", "containers"] {
        for (i, c) in spec[list].as_array().into_iter().flatten().enumerate() {
            if c["image"].is_string() {
                out.push((list, c["name"].as_str().unwrap_or("").to_string(), format!("/{list}/{i}/image")));
            }
        }
    }
    for (i, v) in spec["volumes"].as_array().into_iter().flatten().enumerate() {
        if v["image"]["reference"].as_str().is_some_and(|r| !r.is_empty()) {
            out.push(("volumes", v["name"].as_str().unwrap_or("").to_string(), format!("/volumes/{i}/image/reference")));
        }
    }
    out
}

/// `hasIdenticalPodSpecImage` / `…VolumeImage`: the stored spec has this
/// image under this name (a container's looked up among init containers,
/// then containers).
fn unchanged(old: Option<&Value>, list: &str, name: &str, image: &str) -> bool {
    let Some(old) = old else { return false };
    if list == "volumes" {
        return old["volumes"].as_array().into_iter().flatten().find(|v| v["name"] == name)
            .is_some_and(|v| v["image"]["reference"] == image);
    }
    ["initContainers", "containers"]
        .iter()
        .flat_map(|l| old[*l].as_array().into_iter().flatten())
        .find(|c| c["name"] == name)
        .is_some_and(|c| c["image"] == image)
}

/// Resolve `obj`'s images in place; the number rewritten. `get` reads the
/// image API. Never fails: what cannot be resolved is left.
pub(crate) async fn resolve_object<F, Fut>(
    get: &F,
    target: Target,
    namespace: &str,
    obj: &mut Value,
    old: Option<&Value>,
) -> usize
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    let force = resolve_all(&target, obj);
    let changed = force != old.is_some_and(|o| resolve_all(&target, o));
    let old_spec = old.map(|o| at(o, target.spec)).filter(|s| s.is_object());
    let Some(spec) = at_mut(obj, target.spec).filter(|s| s.is_object()) else { return 0 };
    let mut decided: HashMap<String, Option<String>> = HashMap::new();
    let mut rewritten = 0;
    for (list, name, pointer) in image_fields(spec) {
        let Some(image) = spec.pointer(&pointer).and_then(Value::as_str).map(str::to_string) else { continue };
        if !changed && unchanged(old_spec, list, &name, &image) {
            continue;
        }
        let to = match decided.get(&image) {
            Some(to) => to.clone(),
            None => match resolve(get, &image, namespace, force).await {
                Ok(to) => {
                    decided.insert(image.clone(), to.clone());
                    to
                }
                Err(e) => {
                    tracing::warn!(namespace, image, "image policy: left unresolved, the image API did not answer: {e}");
                    return rewritten;
                }
            },
        };
        if let Some(to) = to.filter(|to| *to != image) {
            if let Some(slot) = spec.pointer_mut(&pointer) {
                tracing::debug!(namespace, from = image, to, "image policy: resolved");
                *slot = Value::String(to);
                rewritten += 1;
            }
        }
    }
    rewritten
}

/// A GET of the image API through the aggregator, as this apiserver makes
/// it for itself (#386: `instantiate` of a DeploymentConfig or
/// BuildConfig, a build's ImageStreamTag). No APIService for the group at
/// all is a missing stream: nothing serves ImageStreams here.
pub(crate) async fn fetch(state: &AppState, path: String) -> Fetched {
    if state.aggregator.claims(&path).is_none() {
        return Fetched::NotFound;
    }
    match state.aggregator.get_own(&state.storage, &path, LOOKUP_TIMEOUT).await {
        Ok(Some(v)) => Fetched::Found(v),
        Ok(None) => Fetched::NotFound,
        Err(e) => Fetched::Failed(e),
    }
}

/// The admission step, on a create or update the request names. Called by
/// [`crate::admission::admit`] before the mutating webhooks.
pub async fn admit(state: &AppState, request: &RequestAttrs, op: Operation, obj: &mut Value, old: Option<&Value>) {
    if request.subresource.is_some() || op == Operation::Delete {
        return;
    }
    let Some(target) = target(&request.group, &request.resource) else { return };
    if target.create_only && op == Operation::Update {
        return;
    }
    let Some(namespace) = request.namespace.clone().or_else(|| obj["metadata"]["namespace"].as_str().map(str::to_string)) else {
        return;
    };
    let controlled = obj["metadata"]["ownerReferences"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|r| r["controller"].as_bool() == Some(true)));
    if controlled {
        return;
    }
    // Nothing serves ImageStreams: nothing to resolve against, no wait.
    if state.aggregator.claims(IMAGE_API).is_none_or(|api| !api.available) {
        return;
    }
    let get = |path: String| async move {
        match state.aggregator.get_own(&state.storage, &path, LOOKUP_TIMEOUT).await {
            Ok(Some(v)) => Fetched::Found(v),
            Ok(None) => Fetched::NotFound,
            Err(e) => Fetched::Failed(e),
        }
    };
    resolve_object(&get, target, &namespace, obj, old).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const REG: &str = "node1.lo:5000";

    /// A registry with `web` (local lookup, tag `latest`), `api` (no local
    /// lookup, tag `v1`) and `empty` (local lookup, nothing pushed), all in
    /// namespace `p`; or none at all when `down`.
    struct Registry {
        down: bool,
        local: bool,
        asked: Mutex<Vec<String>>,
    }

    impl Registry {
        fn new() -> Self {
            Registry { down: false, local: true, asked: Mutex::new(Vec::new()) }
        }
        fn get(&self, path: String) -> Fetched {
            self.asked.lock().unwrap().push(path.clone());
            if self.down {
                return Fetched::Failed("connection refused".into());
            }
            let ist = |stream: &str, tag: &str, local: bool| {
                json!({"kind": "ImageStreamTag", "metadata": {"name": format!("{stream}:{tag}")},
                       "lookupPolicy": {"local": local},
                       "image": {"metadata": {"name": DIGEST},
                                 "dockerImageReference": format!("{REG}/p/{stream}@{DIGEST}")}})
            };
            let stream = |name: &str, local: bool| {
                json!({"metadata": {"name": name}, "spec": {"lookupPolicy": {"local": local}},
                       "status": {"dockerImageRepository": format!("{REG}/p/{name}")}})
            };
            let p = path.strip_prefix("/apis/image.openshift.io/v1/namespaces/p/").unwrap_or("");
            match p {
                "imagestreamtags/web:latest" => Fetched::Found(ist("web", "latest", self.local)),
                "imagestreamtags/api:v1" => Fetched::Found(ist("api", "v1", false)),
                "imagestreams/web" => Fetched::Found(stream("web", self.local)),
                "imagestreams/api" => Fetched::Found(stream("api", false)),
                "imagestreams/empty" => Fetched::Found(stream("empty", true)),
                _ => Fetched::NotFound,
            }
        }
        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    async fn run(reg: &Registry, resource: (&str, &str), obj: &mut Value, old: Option<&Value>) -> usize {
        let get = |path: String| {
            let f = reg.get(path);
            async move { f }
        };
        resolve_object(&get, target(resource.0, resource.1).unwrap(), "p", obj, old).await
    }

    fn pod(images: &[&str]) -> Value {
        let containers: Vec<Value> =
            images.iter().enumerate().map(|(i, im)| json!({"name": format!("c{i}"), "image": im})).collect();
        json!({"metadata": {"name": "x", "namespace": "p"}, "spec": {"containers": containers}})
    }

    fn deployment(image: &str, annotated: bool) -> Value {
        let mut d = json!({"metadata": {"name": "web", "namespace": "p"},
            "spec": {"template": {"metadata": {"labels": {"app": "web"}},
                                  "spec": {"containers": [{"name": "web", "image": image}]}}}});
        if annotated {
            d["spec"]["template"]["metadata"]["annotations"] = json!({RESOLVE_NAMES: "*"});
        }
        d
    }

    fn resolved(stream: &str) -> String {
        format!("{REG}/p/{stream}@{DIGEST}")
    }

    #[test]
    fn references_split_as_library_go_splits_them() {
        let r = ImageRef::parse("web").unwrap();
        assert_eq!((r.registry.as_str(), r.namespace.as_str(), r.name.as_str(), r.tag.as_str()), ("", "", "web", ""));
        let r = ImageRef::parse("web:v1").unwrap();
        assert_eq!((r.name.as_str(), r.tag.as_str()), ("web", "v1"));
        let r = ImageRef::parse("library/nginx:1.25").unwrap();
        assert_eq!((r.registry.as_str(), r.namespace.as_str(), r.name.as_str()), ("", "library", "nginx"));
        let r = ImageRef::parse("localhost:5000/a/b/c:t").unwrap();
        assert_eq!((r.registry.as_str(), r.namespace.as_str(), r.name.as_str(), r.tag.as_str()), ("localhost:5000", "a", "b/c", "t"));
        let r = ImageRef::parse(&format!("{REG}/p/web@{DIGEST}")).unwrap();
        assert_eq!((r.registry.as_str(), r.id.as_str(), r.tag.as_str()), (REG, DIGEST, ""));
        assert_eq!(r.exact(), resolved("web"));
        assert!(ImageRef::parse("").is_none() && ImageRef::parse("a//b").is_none() && ImageRef::parse("a@").is_none());
    }

    #[tokio::test]
    async fn a_local_name_resolves_by_digest_where_the_stream_looks_up_locally() {
        let reg = Registry::new();
        let mut p = pod(&["web", "web:latest", "nginx:1.25", "quay.io/x/web:latest", "p/web:latest"]);
        assert_eq!(run(&reg, ("", "pods"), &mut p, None).await, 2);
        let images: Vec<&str> = p["spec"]["containers"].as_array().unwrap().iter().map(|c| c["image"].as_str().unwrap()).collect();
        assert_eq!(images, [resolved("web").as_str(), &resolved("web"), "nginx:1.25", "quay.io/x/web:latest", "p/web:latest"],
                   "a registry or a namespace in the name: not a local name");
        assert!(!reg.asked.lock().unwrap().iter().any(|p| p.contains("quay")), "a full reference is never looked up");
    }

    #[tokio::test]
    async fn no_local_lookup_no_rewrite() {
        let mut reg = Registry::new();
        reg.local = false;
        let mut p = pod(&["web:latest", "api:v1"]);
        assert_eq!(run(&reg, ("", "pods"), &mut p, None).await, 0, "--enabled=false stops it");
        assert_eq!(p["spec"]["containers"][0]["image"], "web:latest");
    }

    #[tokio::test]
    async fn resolve_names_resolves_whatever_the_stream_says() {
        let reg = Registry::new();
        let mut d = deployment("api:v1", true);
        assert_eq!(run(&reg, ("apps", "deployments"), &mut d, None).await, 1);
        assert_eq!(d["spec"]["template"]["spec"]["containers"][0]["image"], resolved("api"));
        let mut d = deployment("api:v1", false);
        assert_eq!(run(&reg, ("apps", "deployments"), &mut d, None).await, 0, "no annotation, no local lookup");
        // On a Pod the annotation is the Pod's own.
        let mut p = pod(&["api:v1"]);
        p["metadata"]["annotations"] = json!({RESOLVE_NAMES: "*"});
        assert_eq!(run(&reg, ("", "pods"), &mut p, None).await, 1);
        // A CronJob's template is under its jobTemplate.
        let mut cj = json!({"metadata": {"name": "c"}, "spec": {"jobTemplate": {"spec": {"template": {
            "metadata": {"annotations": {RESOLVE_NAMES: "*"}}, "spec": {"initContainers": [{"name": "i", "image": "api:v1"}],
            "containers": [{"name": "c", "image": "busybox"}]}}}}}});
        assert_eq!(run(&reg, ("batch", "cronjobs"), &mut cj, None).await, 1);
        assert_eq!(cj["spec"]["jobTemplate"]["spec"]["template"]["spec"]["initContainers"][0]["image"], resolved("api"));
    }

    #[tokio::test]
    async fn a_missing_tag_of_a_resolving_stream_names_its_repository() {
        let reg = Registry::new();
        let mut p = pod(&["empty:v2", "nothere:v1"]);
        assert_eq!(run(&reg, ("", "pods"), &mut p, None).await, 1);
        assert_eq!(p["spec"]["containers"][0]["image"], format!("{REG}/p/empty:v2"));
        assert_eq!(p["spec"]["containers"][1]["image"], "nothere:v1");
    }

    #[tokio::test]
    async fn an_unreachable_registry_leaves_every_image_and_is_asked_once() {
        let mut reg = Registry::new();
        reg.down = true;
        let mut p = pod(&["web", "web:v2", "other"]);
        let before = p.clone();
        assert_eq!(run(&reg, ("", "pods"), &mut p, None).await, 0);
        assert_eq!(p, before);
        assert_eq!(reg.asked(), 1, "the first failure ends the lookups");
    }

    #[tokio::test]
    async fn an_update_looks_up_only_what_changed_unless_the_annotation_did() {
        let reg = Registry::new();
        let old = deployment("api:v1", false);
        let mut d = deployment("api:v1", false);
        d["spec"]["template"]["spec"]["containers"].as_array_mut().unwrap().push(json!({"name": "side", "image": "web"}));
        assert_eq!(run(&reg, ("apps", "deployments"), &mut d, Some(&old)).await, 1);
        assert_eq!(reg.asked(), 1, "the unchanged container is not looked up");
        // `oc set image-lookup deploy/web`: the annotation arrives, the
        // unchanged image is resolved.
        let mut d = deployment("api:v1", true);
        assert_eq!(run(&reg, ("apps", "deployments"), &mut d, Some(&old)).await, 1);
        assert_eq!(d["spec"]["template"]["spec"]["containers"][0]["image"], resolved("api"));
        // `--enabled=false`: the annotation goes, a resolved image stays
        // (by digest: nothing to look up), a new local name is not resolved.
        let annotated = deployment(&resolved("api"), true);
        let mut d = deployment(&resolved("api"), false);
        assert_eq!(run(&reg, ("apps", "deployments"), &mut d, Some(&annotated)).await, 0);
        let mut d = deployment("api:v1", false);
        assert_eq!(run(&reg, ("apps", "deployments"), &mut d, Some(&annotated)).await, 0);
        assert_eq!(d["spec"]["template"]["spec"]["containers"][0]["image"], "api:v1");
    }

    #[tokio::test]
    async fn image_volumes_are_resolved_too() {
        let reg = Registry::new();
        let mut p = pod(&["busybox"]);
        p["spec"]["volumes"] = json!([{"name": "data", "image": {"reference": "web"}}, {"name": "e", "emptyDir": {}}]);
        assert_eq!(run(&reg, ("", "pods"), &mut p, None).await, 1);
        assert_eq!(p["spec"]["volumes"][0]["image"]["reference"], resolved("web"));
    }

    #[test]
    fn the_covered_resources_are_upstreams_defaults() {
        for (g, r) in [("", "pods"), ("", "replicationcontrollers"), ("apps", "deployments"), ("apps", "statefulsets"),
                       ("apps", "daemonsets"), ("apps", "replicasets"), ("batch", "jobs"), ("batch", "cronjobs"),
                       ("apps.openshift.io", "deploymentconfigs"), ("extensions", "deployments")] {
            assert!(target(g, r).is_some(), "{g}/{r}");
        }
        assert!(target("", "services").is_none() && target("build.openshift.io", "builds").is_none());
        assert!(target("batch", "jobs").unwrap().create_only && !target("apps", "deployments").unwrap().create_only);
    }

    #[test]
    fn a_name_that_could_escape_the_path_is_not_looked_up() {
        assert!(path_safe("web", false) && path_safe("v1.2_RC", true));
        for bad in ["..", "a?b", "a%2f", "A", "a#b", ""] {
            assert!(!path_safe(bad, false), "{bad}");
        }
    }
}
