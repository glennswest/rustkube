//! OpenShift Builds (#374, sc#151, sc#162): the pod that runs a build, and
//! the build's phase from it.
//!
//! Two controllers, as openshift-controller-manager's `pkg/build`:
//!
//! - [`BuildController`]: a New build runs when its BuildConfig's run policy
//!   lets it (Serial: one at a time, oldest first; SerialLatestOnly: older
//!   queued ones cancelled; Parallel: beside each other, not beside a serial
//!   one); its pod `<build>-build` is OpenShift's build pod for the strategy
//!   — OpenShift's **builder image** (`--build-image`), given the build as
//!   `BUILD`, with `openshift-git-clone`, `openshift-extract-image-content`
//!   and `openshift-manage-dockerfile` as init containers and
//!   `openshift-docker-build` / `openshift-sti-build` as the build; a Custom
//!   build runs its own image. The builder image clones, pulls, builds, runs
//!   the post-commit hook and pushes itself, reading the secrets mounted at
//!   OpenShift's paths (`oc set build-secret`: source, pull, push). The
//!   build's phase, start, completion, duration and log snippet follow the
//!   pod; `status.cancelled` (`oc cancel-build`) deletes the pod; a finished
//!   build prunes its BuildConfig's history past its limits.
//! - [`BuildConfigController`]: a BuildConfig with a ConfigChange trigger
//!   gets its first build; its history is pruned.
//!
//! Adapted to a stormcos node, which has no OpenShift node paths: the blob
//! caches and containers storage are emptyDirs and there are no node pull
//! secrets (`/var/lib/kubelet/config.json`). Build pods are privileged
//! (buildah), so the controller-manager may `use` the `privileged` SCC
//! (#374, option A). Registries the pod may reach over plain HTTP (the node
//! registry, `:5100`) are `--build-insecure-registries`, written into the
//! build's `registries.conf` as OpenShift writes the cluster image config's.
//!
//! Ported from openshift-controller-manager `pkg/build` (read 2026-11-23).

use crate::owned::{self, Controller, Dependency, Deps};
use crate::runner::ApiClient;
use apimachinery::informer::{Index, Key};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tracing::{info, warn};

pub const BUILDS_PATH: &str = "/apis/build.openshift.io/v1/builds";
pub const BUILD_CONFIGS_PATH: &str = "/apis/build.openshift.io/v1/buildconfigs";
/// OpenShift's builder image, 5.0.0 (the oc release the port follows), by
/// digest. stormcos names the golden it ships with `--build-image`.
pub const DEFAULT_BUILD_IMAGE: &str =
    "quay.io/openshift/origin-docker-builder@sha256:a11e3b12bd7497f111ba2999f95428c0e5c2de5aa9d0c40b25466124923c0572";

const BUILD_LABEL: &str = "openshift.io/build.name";
const BUILD_CONFIG_LABEL: &str = "openshift.io/build-config.name";
const BUILD_RUN_POLICY_LABEL: &str = "openshift.io/build.start-policy";
const BUILD_ANNOTATION: &str = "openshift.io/build.name";
const BUILD_NUMBER_ANNOTATION: &str = "openshift.io/build.number";
const BUILD_POD_NAME_ANNOTATION: &str = "openshift.io/build.pod-name";
const BUILD_CONFIG_ANNOTATION: &str = "openshift.io/build-config.name";

const BUILD_WORK_DIR: &str = "/tmp/build";
const BLOBS_META_CACHE: &str = "/var/lib/containers/cache";
const BLOBS_CONTENT_CACHE: &str = "/var/cache/blobs";
const SOURCE_SECRET_PATH: &str = "/var/run/secrets/openshift.io/source";
const PUSH_SECRET_PATH: &str = "/var/run/secrets/openshift.io/push";
const PULL_SECRET_PATH: &str = "/var/run/secrets/openshift.io/pull";
const SOURCE_IMAGE_PULL_SECRET_PATH: &str = "/var/run/secrets/openshift.io/source-image";
const INPUT_SECRETS_PATH: &str = "/var/run/secrets/openshift.io/build";
const INPUT_CONFIGMAPS_PATH: &str = "/var/run/configs/openshift.io/build";
const SYSTEM_CONFIGS_PATH: &str = "/var/run/configs/openshift.io/build-system";
const BUILD_VOLUMES_PATH: &str = "/var/run/openshift.io/volumes";
/// One week: OpenShift's build pod deadline without `completionDeadlineSeconds`.
const DEFAULT_DEADLINE: i64 = 604_800;
/// The variables a strategy's env may pass to the builder
/// (`WhitelistEnvVarNames`).
const TRUSTED_ENV: [&str; 10] = ["BUILD_LOGLEVEL", "GIT_SSL_NO_VERIFY", "GIT_LFS_SKIP_SMUDGE", "LANG", "HTTP_PROXY",
                                 "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy"];
/// Source builds drop these from the assemble run (`DefaultDropCaps`).
const SOURCE_DROP_CAPS: &str = "KILL,MKNOD,SETGID,SETUID";
const MAX_EXCERPT_LINES: usize = 5;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn name_of(o: &Value) -> String {
    s(&o["metadata"], "name").to_string()
}
fn ns_of(o: &Value) -> String {
    s(&o["metadata"], "namespace").to_string()
}
fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
fn is_complete(b: &Value) -> bool {
    matches!(s(&b["status"], "phase"), "Complete" | "Failed" | "Error" | "Cancelled")
}

/// library-go's `naming.GetName`, as the build controller names things.
pub fn get_name(base: &str, suffix: &str, max: usize) -> String {
    let name = format!("{base}-{suffix}");
    if name.len() <= max {
        return name;
    }
    let hash = |s: &str| {
        let mut h: u32 = 0x811c9dc5;
        for b in s.bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x01000193);
        }
        format!("{h:08x}")
    };
    match max.checked_sub(10 + suffix.len()) {
        None => {
            let prefix = &base[..base.len().min(max.saturating_sub(9))];
            let short = format!("{prefix}-{}", hash(&name));
            short[..max.min(short.len())].to_string()
        }
        Some(len) => format!("{}-{}-{suffix}", &base[..len.min(base.len())], hash(base)),
    }
}

pub fn build_pod_name(build: &str) -> String {
    get_name(build, "build", 63)
}
fn sys_config_name(build: &str) -> String {
    get_name(build, "sys-config", 253)
}
fn config_name_of(b: &Value) -> String {
    let a = s(&b["metadata"]["annotations"], BUILD_CONFIG_ANNOTATION);
    if !a.is_empty() {
        return a.to_string();
    }
    let l = s(&b["metadata"]["labels"], BUILD_CONFIG_LABEL);
    if !l.is_empty() {
        return l.to_string();
    }
    s(&b["status"]["config"], "name").to_string()
}
fn build_number(b: &Value) -> i64 {
    s(&b["metadata"]["annotations"], BUILD_NUMBER_ANNOTATION).parse().unwrap_or(0)
}
fn run_policy(b: &Value) -> &str {
    match s(&b["metadata"]["labels"], BUILD_RUN_POLICY_LABEL) {
        "Parallel" => "Parallel",
        "SerialLatestOnly" => "SerialLatestOnly",
        _ => "Serial",
    }
}

/// Whether a New build may run now: its config's run policy
/// (`policy.IsRunnable`) over the builds of the same config. A
/// SerialLatestOnly build also names the older queued ones to cancel.
pub fn runnable(build: &Value, siblings: &[Value]) -> (bool, Vec<String>) {
    let config = config_name_of(build);
    if config.is_empty() {
        return (true, Vec::new());
    }
    let same: Vec<&Value> = siblings.iter().filter(|b| config_name_of(b) == config).collect();
    let running = |serial_only: bool| {
        same.iter().any(|b| {
            matches!(s(&b["status"], "phase"), "Pending" | "Running")
                && (!serial_only || run_policy(b) != "Parallel")
        })
    };
    let policy = run_policy(build);
    if policy == "Parallel" {
        return (!running(true), Vec::new());
    }
    let mut cancel = Vec::new();
    if policy == "SerialLatestOnly" {
        let mine = build_number(build);
        cancel = same
            .iter()
            .filter(|b| !is_complete(b) && s(&b["status"], "phase") != "Running" && build_number(b) < mine
                    && b["status"]["cancelled"] != true)
            .map(|b| name_of(b))
            .collect();
    }
    if running(false) {
        return (false, cancel);
    }
    // GetNextConfigBuild: the lowest-numbered New build not being cancelled.
    let next = same
        .iter()
        .filter(|b| s(&b["status"], "phase") == "New" && !cancel.contains(&name_of(b)) && b["status"]["cancelled"] != true)
        .min_by_key(|b| build_number(b));
    let ok = match next {
        Some(n) if run_policy(n) == "Parallel" => true,
        Some(n) => name_of(n) == name_of(build),
        None => false,
    };
    (ok, cancel)
}

/// `LabelValue`: the name, truncated until it is a valid label value.
pub fn label_value(name: &str) -> String {
    let valid = |v: &str| {
        v.len() <= 63
            && (v.is_empty()
                || (v.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                    && v.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
                    && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))))
    };
    let mut n = name.to_string();
    while !n.is_empty() {
        if valid(&n) {
            return n;
        }
        n.pop();
    }
    name.to_string()
}

fn env(name: &str, value: &str) -> Value {
    json!({"name": name, "value": value})
}

/// A volume for `obj` of `kind` (secret or configMap) mounted read-only at
/// `path` in `container`, named as `mountVolume` names it.
fn mount(pod: &mut Value, container: &mut Value, kind: &str, obj: &str, path: &str, suffix: &str, source: Option<Value>) {
    let volume = get_name(obj, suffix, 63).replace('.', "-");
    let vols = pod["spec"]["volumes"].as_array_mut().expect("volumes is an array");
    if !vols.iter().any(|v| v["name"] == volume.as_str()) {
        let src = source.unwrap_or_else(|| match kind {
            "secret" => json!({"secretName": obj, "defaultMode": 0o600}),
            _ => json!({"name": obj, "defaultMode": 0o600}),
        });
        let mut v = json!({"name": volume});
        v[kind] = src;
        vols.push(v);
    }
    push(container, "volumeMounts", json!({"name": volume, "mountPath": path, "readOnly": true}));
}

fn push(container: &mut Value, field: &str, item: Value) {
    if !container[field].is_array() {
        container[field] = json!([]);
    }
    container[field].as_array_mut().expect("an array").push(item);
}

/// `securityContextForBuild`: privileged as root, unless the strategy's
/// `BUILD_PRIVILEGED` says otherwise.
fn build_security_context(strategy_env: &Value) -> Value {
    let mut privileged = true;
    for e in strategy_env.as_array().into_iter().flatten() {
        if e["name"] == "BUILD_PRIVILEGED" {
            let v = s(e, "value");
            privileged = match v.parse::<bool>() {
                Ok(b) => b,
                Err(_) => v.parse::<i64>().map(|i| i != 0).unwrap_or(privileged),
            };
        }
    }
    if privileged {
        json!({"privileged": true, "runAsUser": 0, "runAsGroup": 0, "seccompProfile": {"type": "Unconfined"}})
    } else {
        json!({"privileged": false, "seccompProfile": {"type": "Unconfined"},
               "capabilities": {"add": ["CAP_SETFCAP"], "drop": ["CAP_KILL", "CAP_MKNOD"]}})
    }
}

/// `builderMinSecurityContext`: the init containers' (root, CHOWN and
/// DAC_OVERRIDE only).
fn init_security_context() -> Value {
    json!({"privileged": false, "runAsNonRoot": false, "runAsUser": 0, "runAsGroup": 0,
           "seccompProfile": {"type": "RuntimeDefault"},
           "capabilities": {"drop": ["ALL"], "add": ["CHOWN", "DAC_OVERRIDE"]}})
}

/// The build pod for `build`: OpenShift's, for its strategy (`docker.go`,
/// `sti.go`, `custom.go` and `util.go`'s setup), on a stormcos node.
pub fn build_pod(build: &Value, builder_image: &str) -> anyhow::Result<Value> {
    let spec = &build["spec"];
    let st = &spec["strategy"];
    let (kind, strategy) = if st["dockerStrategy"].is_object() {
        ("docker", &st["dockerStrategy"])
    } else if st["sourceStrategy"].is_object() {
        ("source", &st["sourceStrategy"])
    } else if st["customStrategy"].is_object() {
        ("custom", &st["customStrategy"])
    } else {
        anyhow::bail!("unsupported build strategy");
    };
    let mut base_env = vec![env("BUILD", &build.to_string()), env("LANG", "C.utf8")];
    let src = &spec["source"];
    if kind != "custom" || src["git"].is_object() {
        if let Some(uri) = src["git"]["uri"].as_str() {
            base_env.push(env("SOURCE_REPOSITORY", uri));
            base_env.push(env("SOURCE_URI", uri));
        }
        if let Some(d) = src["contextDir"].as_str().filter(|d| !d.is_empty()) {
            base_env.push(env("SOURCE_CONTEXT_DIR", d));
        }
        if let Some(r) = src["git"]["ref"].as_str().filter(|r| !r.is_empty()) {
            base_env.push(env("SOURCE_REF", r));
        }
    }
    if let Some(m) = spec["mountTrustedCA"].as_bool() {
        base_env.push(env("BUILD_MOUNT_ETC_PKI_CATRUST", &m.to_string()));
    }
    if kind == "source" {
        base_env.push(env("ALLOWED_UIDS", "1-"));
        base_env.push(env("DROP_CAPS", SOURCE_DROP_CAPS));
    }
    // The strategy's env: only the trusted names reach the builder; a
    // Custom build's image gets all of it.
    for e in strategy["env"].as_array().into_iter().flatten() {
        let n = s(e, "name");
        if kind != "custom" && !TRUSTED_ENV.contains(&n) {
            continue;
        }
        base_env.retain(|x| x["name"] != n);
        base_env.push(e.clone());
    }
    if kind == "custom" {
        if let Some(to) = spec["output"]["to"].as_object().filter(|t| t.get("kind").and_then(Value::as_str) == Some("DockerImage")) {
            let name = to.get("name").and_then(Value::as_str).unwrap_or("");
            let (registry, image) = match name.split_once('/') {
                Some((r, i)) if r.contains('.') || r.contains(':') || r == "localhost" => (r, i),
                _ => ("", name),
            };
            base_env.push(env("OUTPUT_REGISTRY", registry));
            base_env.push(env("OUTPUT_IMAGE", image));
        }
    }
    let sa = match s(spec, "serviceAccount") {
        "" => "builder",
        v => v,
    };
    let build_name = name_of(build);
    let pod_name = build_pod_name(&build_name);
    let main_name = match kind {
        "docker" => "docker-build",
        "source" => "sti-build",
        _ => "custom-build",
    };
    let sc = build_security_context(&strategy["env"]);
    let mut main = json!({
        "name": main_name,
        "image": if kind == "custom" { s(&strategy["from"], "name") } else { builder_image },
        "env": base_env.clone(),
        "securityContext": sc,
        "terminationMessagePolicy": "FallbackToLogsOnError",
        "imagePullPolicy": "IfNotPresent",
        "volumeMounts": [],
    });
    if kind != "custom" {
        main["args"] = json!([if kind == "docker" { "openshift-docker-build" } else { "openshift-sti-build" }]);
        main["volumeMounts"] = json!([
            {"name": "buildworkdir", "mountPath": BUILD_WORK_DIR},
            {"name": "buildcachedir", "mountPath": BLOBS_META_CACHE},
        ]);
    }
    if !spec["resources"].is_null() {
        main["resources"] = spec["resources"].clone();
    }
    let mut labels = Map::new();
    labels.insert(BUILD_LABEL.into(), json!(label_value(&build_name)));
    let mut pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": pod_name,
            "namespace": ns_of(build),
            "labels": labels,
            "annotations": {BUILD_ANNOTATION: build_name},
            "ownerReferences": [{"apiVersion": "build.openshift.io/v1", "kind": "Build", "name": build_name,
                                 "uid": build["metadata"]["uid"], "controller": true}],
        },
        "spec": {
            "serviceAccountName": sa,
            "containers": [],
            "initContainers": [],
            "volumes": [],
            "restartPolicy": "Never",
            "activeDeadlineSeconds": spec["completionDeadlineSeconds"].as_i64().unwrap_or(DEFAULT_DEADLINE),
        },
    });
    if !spec["nodeSelector"].is_null() {
        pod["spec"]["nodeSelector"] = spec["nodeSelector"].clone();
    }
    if kind != "custom" {
        pod["spec"]["volumes"] = json!([
            {"name": "buildcachedir", "emptyDir": {}},
            {"name": "buildworkdir", "emptyDir": {}},
        ]);
    }
    let mut inits: Vec<Value> = Vec::new();
    if kind != "custom" {
        let init = |name: &str, arg: &str, sc: Value| {
            json!({"name": name, "image": builder_image, "args": [arg], "env": base_env.clone(), "securityContext": sc,
                   "terminationMessagePolicy": "FallbackToLogsOnError", "imagePullPolicy": "IfNotPresent",
                   "volumeMounts": [{"name": "buildworkdir", "mountPath": BUILD_WORK_DIR}],
                   "resources": spec["resources"].clone()})
        };
        if src["git"].is_object() {
            let mut git = init("git-clone", "openshift-git-clone", init_security_context());
            if let Some(n) = src["sourceSecret"]["name"].as_str() {
                mount(&mut pod, &mut git, "secret", n, SOURCE_SECRET_PATH, "source", None);
                push(&mut git, "env", env("SOURCE_SECRET_PATH", SOURCE_SECRET_PATH));
            }
            inits.push(git);
        }
        if src["images"].as_array().is_some_and(|i| !i.is_empty()) {
            let mut extract = init("extract-image-content", "openshift-extract-image-content", build_security_context(&strategy["env"]));
            push(&mut extract, "volumeMounts", json!({"name": "buildcachedir", "mountPath": BLOBS_META_CACHE}));
            docker_secrets(&mut pod, &mut extract, spec, strategy);
            containers_storage(&mut pod, &mut extract);
            inits.push(extract);
        }
        inits.push(init("manage-dockerfile", "openshift-manage-dockerfile", init_security_context()));
    }
    docker_secrets(&mut pod, &mut main, spec, strategy);
    if kind == "custom" {
        if let Some(n) = src["sourceSecret"]["name"].as_str() {
            mount(&mut pod, &mut main, "secret", n, SOURCE_SECRET_PATH, "source", None);
            push(&mut main, "env", env("SOURCE_SECRET_PATH", SOURCE_SECRET_PATH));
        }
    }
    for i in src["secrets"].as_array().into_iter().flatten() {
        let n = s(&i["secret"], "name");
        mount(&mut pod, &mut main, "secret", n, &format!("{INPUT_SECRETS_PATH}/{n}"), "build", None);
    }
    for i in src["configMaps"].as_array().into_iter().flatten() {
        let n = s(&i["configMap"], "name");
        mount(&mut pod, &mut main, "configMap", n, &format!("{INPUT_CONFIGMAPS_PATH}/{n}"), "build", None);
    }
    if kind == "custom" {
        for sec in strategy["secrets"].as_array().into_iter().flatten() {
            mount(&mut pod, &mut main, "secret", s(&sec["secretSource"], "name"), s(sec, "mountPath"), "secret", None);
        }
    }
    containers_storage(&mut pod, &mut main);
    // A strategy's build volumes, at OpenShift's paths.
    for v in strategy["volumes"].as_array().into_iter().flatten() {
        let src = &v["source"];
        let (kind, obj, source) = match s(src, "type") {
            "Secret" => ("secret", s(&src["secret"], "secretName").to_lowercase(), src["secret"].clone()),
            "ConfigMap" => ("configMap", s(&src["configMap"], "name").to_lowercase(), src["configMap"].clone()),
            "CSI" => ("csi", s(v, "name").to_lowercase(), src["csi"].clone()),
            other => anyhow::bail!("encountered unsupported build volume source type {other:?}"),
        };
        let path = format!("{BUILD_VOLUMES_PATH}/{}", get_name(&obj, "user-build-volume", 63));
        mount(&mut pod, &mut main, kind, &obj, &path, "user-build-volume", Some(source));
    }
    let mut containers = vec![main];
    pod["spec"]["initContainers"] = json!(inits);
    // setupContainersConfigs: the build's registries.conf and policy.
    pod["spec"]["volumes"].as_array_mut().expect("an array").push(
        json!({"name": "build-system-configs", "configMap": {"name": sys_config_name(&build_name)}}));
    // setupBlobCache: the blob cache, an emptyDir.
    pod["spec"]["volumes"].as_array_mut().expect("an array").push(json!({"name": "build-blob-cache", "emptyDir": {}}));
    let mut all_inits: Vec<Value> = pod["spec"]["initContainers"].as_array().cloned().unwrap_or_default();
    for c in containers.iter_mut().chain(all_inits.iter_mut()) {
        push(c, "volumeMounts", json!({"name": "build-system-configs", "mountPath": SYSTEM_CONFIGS_PATH, "readOnly": true}));
        for (k, f) in [("BUILD_REGISTRIES_CONF_PATH", "registries.conf"), ("BUILD_REGISTRIES_DIR_PATH", "registries.d"),
                       ("BUILD_SIGNATURE_POLICY_PATH", "policy.json"), ("BUILD_STORAGE_CONF_PATH", "storage.conf")] {
            push(c, "env", env(k, &format!("{SYSTEM_CONFIGS_PATH}/{f}")));
        }
        push(c, "volumeMounts", json!({"name": "build-blob-cache", "mountPath": BLOBS_CONTENT_CACHE}));
        push(c, "env", env("BUILD_BLOBCACHE_DIR", BLOBS_CONTENT_CACHE));
    }
    pod["spec"]["containers"] = json!(containers);
    pod["spec"]["initContainers"] = json!(all_inits);
    Ok(pod)
}

/// `setupDockerSecrets`: push, pull and source-image pull secrets.
fn docker_secrets(pod: &mut Value, container: &mut Value, spec: &Value, strategy: &Value) {
    if let Some(n) = spec["output"]["pushSecret"]["name"].as_str() {
        mount(pod, container, "secret", n, PUSH_SECRET_PATH, "push", None);
        push(container, "env", env("PUSH_DOCKERCFG_PATH", PUSH_SECRET_PATH));
    }
    if let Some(n) = strategy["pullSecret"]["name"].as_str() {
        mount(pod, container, "secret", n, PULL_SECRET_PATH, "pull", None);
        push(container, "env", env("PULL_DOCKERCFG_PATH", PULL_SECRET_PATH));
    }
    for (i, image) in spec["source"]["images"].as_array().into_iter().flatten().enumerate() {
        if let Some(n) = image["pullSecret"]["name"].as_str() {
            let path = format!("{SOURCE_IMAGE_PULL_SECRET_PATH}/{i}");
            mount(pod, container, "secret", n, &path, &format!("source-image{i}"), None);
            push(container, "env", env(&format!("PULL_SOURCE_DOCKERCFG_PATH_{i}"), &path));
        }
    }
}

/// `setupContainersStorage`: buildah's storage, emptyDirs.
fn containers_storage(pod: &mut Value, container: &mut Value) {
    let vols = pod["spec"]["volumes"].as_array_mut().expect("an array");
    for n in ["container-storage-root", "container-storage-run"] {
        if !vols.iter().any(|v| v["name"] == n) {
            vols.push(json!({"name": n, "emptyDir": {}}));
        }
    }
    push(container, "volumeMounts", json!({"name": "container-storage-root", "mountPath": "/var/lib/containers"}));
    push(container, "volumeMounts", json!({"name": "container-storage-run", "mountPath": "/var/run/containers"}));
}

/// The build's `registries.conf`: the registries it may reach over HTTP.
pub fn registries_conf(insecure: &[String]) -> Option<String> {
    if insecure.is_empty() {
        return None;
    }
    let mut out = String::from("unqualified-search-registries = [\"docker.io\"]\n");
    for r in insecure {
        out.push_str(&format!("\n[[registry]]\n  prefix = \"\"\n  location = {}\n  insecure = true\n", serde_json::to_string(r).unwrap_or_default()));
    }
    Some(out)
}

/// A transition of the build: phase, reason, message.
#[derive(Default, Debug, PartialEq)]
pub struct Update {
    pub phase: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub start: Option<String>,
}

fn to(phase: &str, reason: &str, message: &str) -> Update {
    Update { phase: Some(phase.into()), reason: Some(reason.into()), message: Some(message.into()), start: None }
}

/// `handleActiveBuild`: the build's phase from its pod.
pub fn from_pod(build: &Value, pod: Option<&Value>) -> Update {
    let Some(pod) = pod else {
        return to("Error", "BuildPodDeleted", "The pod for this build was deleted before the build completed.");
    };
    let mut phase = s(&pod["status"], "phase").to_string();
    if phase == "Pending" {
        let cloning = pod["status"]["initContainerStatuses"].as_array().into_iter().flatten().any(|c| {
            c["name"] == "git-clone" && (c["state"]["running"].is_object() || c["state"]["terminated"].is_object())
        });
        if cloning {
            phase = "Running".into();
        }
    }
    let current = s(&build["status"], "phase");
    let terminated_badly = |p: &Value| {
        p["status"]["containerStatuses"].as_array().into_iter().flatten()
            .any(|c| c["state"]["terminated"]["exitCode"].as_i64().is_some_and(|x| x != 0))
    };
    let oom = pod["status"]["reason"] == "OOMKilled"
        || pod["status"]["initContainerStatuses"].as_array().into_iter().flatten()
            .chain(pod["status"]["containerStatuses"].as_array().into_iter().flatten())
            .any(|c| c["state"]["terminated"]["reason"] == "OOMKilled");
    match phase.as_str() {
        "Pending" if current == "New" => to("Pending", "", ""),
        "Running" if current != "Running" => {
            let mut u = to("Running", "", "");
            u.start = pod["status"]["startTime"].as_str().map(str::to_string);
            u
        }
        "Succeeded" => {
            if pod["status"]["containerStatuses"].as_array().is_none_or(|c| c.is_empty()) {
                to("Error", "NoBuildContainerStatus", "The pod for this build has no container statuses indicating success or failure.")
            } else if terminated_badly(pod) {
                to("Error", "FailedContainer", "The pod for this build has at least one container with a non-zero exit status.")
            } else if current != "Complete" {
                to("Complete", "", "")
            } else {
                Update::default()
            }
        }
        "Failed" if oom => to("Failed", "OutOfMemoryKilled", "The build pod was killed due to an out of memory condition."),
        "Failed" if pod["status"]["reason"] == "Evicted" => to("Failed", "BuildPodEvicted", s(&pod["status"], "message")),
        "Failed" if current != "Failed" => {
            if pod["metadata"]["deletionTimestamp"].is_null() {
                to("Failed", "GenericBuildFailed", "Generic Build failure - check logs for details.")
            } else {
                to("Error", "BuildPodDeleted", "The pod for this build was deleted before the build completed.")
            }
        }
        _ => Update::default(),
    }
}

/// `setBuildCompletionData`: start, completion, duration (nanoseconds, as
/// Go's `time.Duration` encodes) and, for a bad build, the log snippet from
/// the failed container's termination message.
pub fn completion(build: &Value, pod: Option<&Value>, failed: bool) -> Map<String, Value> {
    let mut out = Map::new();
    let start = build["status"]["startTimestamp"]
        .as_str()
        .or(pod.and_then(|p| p["status"]["startTime"].as_str()))
        .map(str::to_string)
        .unwrap_or_else(now);
    out.insert("startTimestamp".into(), json!(start));
    if build["status"]["completionTimestamp"].is_null() {
        let end = now();
        out.insert("completionTimestamp".into(), json!(end));
        let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok();
        if let (Some(a), Some(b)) = (parse(&start), parse(&end)) {
            let nanos = (b - a).num_seconds().max(0) * 1_000_000_000;
            out.insert("duration".into(), json!(nanos));
        }
    }
    if failed && s(&build["status"], "logSnippet").is_empty() {
        if let Some(p) = pod {
            let main = &p["status"]["containerStatuses"][0]["state"]["terminated"];
            let mut msg = if main.is_object() { s(main, "message").to_string() } else { String::new() };
            if msg.is_empty() {
                if let Some(i) = p["status"]["initContainerStatuses"].as_array().into_iter().flatten()
                    .find(|c| c["state"]["terminated"].is_object()) {
                    msg = s(&i["state"]["terminated"], "message").to_string();
                }
            }
            if !msg.is_empty() {
                let lines: Vec<&str> = msg.trim_end_matches('\n').split('\n').collect();
                let excerpt: Vec<String> = lines[lines.len().saturating_sub(MAX_EXCERPT_LINES)..]
                    .iter()
                    .map(|l| if l.len() > 120 && l.is_char_boundary(58) && l.is_char_boundary(l.len() - 59) {
                        format!("{}...{}", &l[..58], &l[l.len() - 59..])
                    } else {
                        l.to_string()
                    })
                    .collect();
                out.insert("logSnippet".into(), json!(excerpt.join("\n")));
            }
        }
    }
    out
}

/// `isValidTransition`.
fn valid_transition(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    match from {
        "Complete" | "Failed" | "Error" | "Cancelled" => false,
        "Pending" => to != "New",
        "Running" => to != "New" && to != "Pending",
        _ => true,
    }
}

pub struct BuildController {
    api: Arc<ApiClient>,
    image: String,
    insecure_registries: Vec<String>,
}

impl BuildController {
    pub fn new(api: Arc<ApiClient>, image: String, insecure_registries: Vec<String>) -> Self {
        Self { api, image, insecure_registries }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }

    fn build_path(b: &Value) -> String {
        format!("/apis/build.openshift.io/v1/namespaces/{}/builds/{}", ns_of(b), name_of(b))
    }

    /// The build with `update` and the completion data applied, written
    /// through the main resource (Builds have no `/status`).
    async fn write(&self, build: &Value, update: Update, pod: Option<&Value>) -> anyhow::Result<()> {
        let mut b = build.clone();
        let from = s(&build["status"], "phase").to_string();
        let mut changed = false;
        if let Some(p) = &update.phase {
            if !valid_transition(&from, p) {
                anyhow::bail!("invalid phase transition {}/{}: {from} -> {p}", ns_of(build), name_of(build));
            }
            if *p != from {
                changed = true;
                b["status"]["phase"] = json!(p);
            }
        }
        for (k, v) in [("reason", &update.reason), ("message", &update.message)] {
            if let Some(v) = v {
                if b["status"][k].as_str().unwrap_or("") != v.as_str() {
                    b["status"][k] = json!(v);
                    changed = true;
                }
            }
        }
        if let Some(t) = &update.start {
            if b["status"]["startTimestamp"].is_null() {
                b["status"]["startTimestamp"] = json!(t);
                changed = true;
            }
        }
        let phase = s(&b["status"], "phase").to_string();
        if matches!(phase.as_str(), "Complete" | "Failed" | "Error" | "Cancelled") && (changed || b["status"]["completionTimestamp"].is_null()) {
            for (k, v) in completion(&b, pod, matches!(phase.as_str(), "Failed" | "Error")) {
                if b["status"][&k] != v {
                    b["status"][&k] = v;
                    changed = true;
                }
            }
        }
        if let Some(p) = pod {
            if s(&b["metadata"]["annotations"], BUILD_POD_NAME_ANNOTATION).is_empty() {
                b["metadata"]["annotations"][BUILD_POD_NAME_ANNOTATION] = json!(name_of(p));
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        self.api.update(&Self::build_path(build), &b).await?;
        if phase != from {
            info!("Build {}/{}: {from} -> {phase}", ns_of(build), name_of(build));
        }
        Ok(())
    }

    async fn create_pod(&self, build: &Value) -> anyhow::Result<Update> {
        let pod = match build_pod(build, &self.image) {
            Ok(p) => p,
            Err(e) => {
                return Ok(Update { reason: Some("CannotCreateBuildPodSpec".into()),
                                   message: Some(format!("Failed to create pod spec: {e}")), ..Default::default() })
            }
        };
        // An ImageStream output resolves through ImageStreams, which are not
        // served: OpenShift's InvalidOutputReference.
        let out_kind = s(&build["spec"]["output"]["to"], "kind");
        if !out_kind.is_empty() && out_kind != "DockerImage" {
            return Ok(Update { reason: Some("InvalidOutputReference".into()),
                               message: Some("Output image could not be resolved.".into()), ..Default::default() });
        }
        let ns = ns_of(build);
        let created = match self.api.create(&format!("/api/v1/namespaces/{ns}/pods"), &pod).await {
            Ok(p) => p,
            Err(e) => {
                let existing = self.api.get(&format!("/api/v1/namespaces/{ns}/pods/{}", name_of(&pod))).await?;
                if !existing.status().is_success() {
                    return Ok(Update { reason: Some("CannotCreateBuildPod".into()),
                                       message: Some(format!("Failed creating build pod: {e}")), ..Default::default() });
                }
                let existing: Value = existing.json().await?;
                let ours = existing["metadata"]["ownerReferences"].as_array().into_iter().flatten()
                    .any(|r| r["uid"] == build["metadata"]["uid"]);
                if !ours {
                    return Ok(to("Error", "BuildPodExists", "The pod for this build already exists and is older than the build."));
                }
                existing
            }
        };
        // The build's registries.conf, owned by its pod as upstream's.
        let mut data = Map::new();
        if let Some(conf) = registries_conf(&self.insecure_registries) {
            data.insert("registries.conf".into(), json!(conf));
        }
        let cm = json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": sys_config_name(&name_of(build)), "namespace": ns,
                         "ownerReferences": [{"apiVersion": "v1", "kind": "Pod", "name": name_of(&created),
                                              "uid": created["metadata"]["uid"]}]},
            "data": data,
        });
        if let Err(e) = self.api.create(&format!("/api/v1/namespaces/{ns}/configmaps"), &cm).await {
            let found = self.api.get(&format!("/api/v1/namespaces/{ns}/configmaps/{}", sys_config_name(&name_of(build)))).await?;
            anyhow::ensure!(found.status().is_success(), "creating the build's system config: {e}");
        }
        Ok(from_pod(build, Some(&created)))
    }
}

/// Every build of the same config, for its queued siblings: a build that
/// finishes lets the next one run.
fn route_siblings(delta: &apimachinery::informer::Delta, primary: &apimachinery::informers::Feed) -> Vec<Key> {
    let mut keys = Vec::new();
    for b in delta.old.iter().chain(delta.new.iter()) {
        let config = config_name_of(b);
        if config.is_empty() {
            continue;
        }
        for o in primary.select(&Index::Namespace(ns_of(b))).unwrap_or_default() {
            if config_name_of(&o) == config && s(&o["status"], "phase") == "New" {
                if let Ok(k) = Key::of(&o) {
                    keys.push(k);
                }
            }
        }
    }
    keys
}

#[async_trait::async_trait]
impl Controller for BuildController {
    fn name(&self) -> &'static str {
        "build"
    }
    fn primary(&self) -> &str {
        BUILDS_PATH
    }
    fn children(&self) -> Option<&'static str> {
        Some("/api/v1/pods")
    }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency { path: BUILDS_PATH.into(), route: Arc::new(route_siblings) }]
    }
    async fn reconcile(&self, build: &Value, children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        if build["spec"]["strategy"]["jenkinsPipelineStrategy"].is_object() || !build["metadata"]["deletionTimestamp"].is_null() {
            return Ok(());
        }
        let pod_name = build_pod_name(&name_of(build));
        let pod = children.iter().find(|p| name_of(p) == pod_name);
        let phase = s(&build["status"], "phase").to_string();
        if is_complete(build) {
            // shouldIgnore: done, once its completion data is written.
            let done = !build["status"]["completionTimestamp"].is_null()
                && (phase != "Failed" || !s(&build["status"], "logSnippet").is_empty() || pod.is_none());
            if !done {
                self.write(build, Update::default(), pod).await?;
            }
            return Ok(());
        }
        if build["status"]["cancelled"] == true {
            let path = format!("/api/v1/namespaces/{}/pods/{pod_name}", ns_of(build));
            let resp = self.api.delete(&path).await?;
            anyhow::ensure!(resp.status().is_success() || resp.status().as_u16() == 404,
                            "could not delete build pod {}/{pod_name}: {}", ns_of(build), resp.status());
            return self.write(build, to("Cancelled", "CancelledBuild", "The build was cancelled by the user."), pod).await;
        }
        let update = match phase.as_str() {
            "New" | "" => {
                if let Some(p) = pod {
                    from_pod(build, Some(p))
                } else {
                    let siblings = deps.feed(0).select(&Index::Namespace(ns_of(build)))?;
                    let (ok, cancel) = runnable(build, &siblings);
                    for name in cancel {
                        if let Some(mut b) = siblings.iter().find(|b| name_of(b) == name).cloned() {
                            b["status"]["cancelled"] = json!(true);
                            if let Err(e) = self.api.update(&Self::build_path(&b), &b).await {
                                warn!("Build {}/{name}: cancelling it for a later one: {e}", ns_of(build));
                            }
                        }
                    }
                    if !ok {
                        return Ok(());
                    }
                    self.create_pod(build).await?
                }
            }
            _ => from_pod(build, pod),
        };
        self.write(build, update, pod).await
    }
}

/// `HandleBuildPruning`: a config's builds past its history limits, oldest
/// first (Complete past `successfulBuildsHistoryLimit`; Failed, Error and
/// Cancelled past `failedBuildsHistoryLimit`).
pub fn prunable(bc: &Value, builds: &[Value]) -> Vec<String> {
    let mine: Vec<&Value> = builds.iter().filter(|b| config_name_of(b) == name_of(bc)).collect();
    let mut out = Vec::new();
    for (limit, phases) in [("successfulBuildsHistoryLimit", &["Complete"][..]),
                            ("failedBuildsHistoryLimit", &["Failed", "Cancelled", "Error"][..])] {
        let Some(limit) = bc["spec"][limit].as_i64() else { continue };
        let mut set: Vec<&&Value> = mine.iter().filter(|b| phases.contains(&s(&b["status"], "phase"))).collect();
        // Newest first; everything past the limit goes.
        set.sort_by(|a, b| s(&b["metadata"], "creationTimestamp").cmp(s(&a["metadata"], "creationTimestamp"))
            .then(build_number(b).cmp(&build_number(a))));
        out.extend(set.iter().skip(limit.max(0) as usize).map(|b| name_of(b)));
    }
    out
}

pub struct BuildConfigController {
    api: Arc<ApiClient>,
}

impl BuildConfigController {
    pub fn new(api: Arc<ApiClient>) -> Self {
        Self { api }
    }

    pub async fn run(&self) {
        owned::run(&self.api, self).await;
    }
}

/// The builds of a config, for its BuildConfig.
fn route_config(delta: &apimachinery::informer::Delta, primary: &apimachinery::informers::Feed) -> Vec<Key> {
    delta
        .old
        .iter()
        .chain(delta.new.iter())
        .filter(|b| is_complete(b))
        .flat_map(|b| owned::keys_at(primary, Index::Name(ns_of(b), config_name_of(b))))
        .collect()
}

#[async_trait::async_trait]
impl Controller for BuildConfigController {
    fn name(&self) -> &'static str {
        "buildconfig"
    }
    fn primary(&self) -> &str {
        BUILD_CONFIGS_PATH
    }
    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency { path: BUILDS_PATH.into(), route: Arc::new(route_config) }]
    }
    async fn reconcile(&self, bc: &Value, _children: &[Value], deps: &Deps) -> anyhow::Result<()> {
        let ns = ns_of(bc);
        let builds = deps.feed(0).select(&Index::Namespace(ns.clone()))?;
        for name in prunable(bc, &builds) {
            let path = format!("/apis/build.openshift.io/v1/namespaces/{ns}/builds/{name}");
            match self.api.delete(&path).await {
                Ok(_) => info!("Build {ns}/{name}: pruned by its BuildConfig's history limit"),
                Err(e) => warn!("Build {ns}/{name}: pruning: {e}"),
            }
        }
        let config_change = bc["spec"]["triggers"].as_array().into_iter().flatten().any(|t| t["type"] == "ConfigChange");
        if !config_change || bc["status"]["lastVersion"].as_i64().unwrap_or(0) > 0 {
            return Ok(());
        }
        let req = json!({"kind": "BuildRequest", "apiVersion": "build.openshift.io/v1",
                         "metadata": {"name": name_of(bc), "namespace": ns},
                         "triggeredBy": [{"message": "Build configuration change"}], "lastVersion": 0});
        let path = format!("/apis/build.openshift.io/v1/namespaces/{ns}/buildconfigs/{}/instantiate", name_of(bc));
        match self.api.create(&path, &req).await {
            Ok(b) => {
                info!("BuildConfig {ns}/{}: {} started (config change)", name_of(bc), name_of(&b));
                Ok(())
            }
            // The version moved on meanwhile, or the config cannot build
            // (paused, a missing image stream): upstream gives up on those.
            Err(e) if e.to_string().contains("does not match the build request LastVersion(0)")
                || e.to_string().contains("is paused") => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(name: &str, number: i64, phase: &str, policy: &str) -> Value {
        json!({"metadata": {"name": name, "namespace": "demo", "uid": format!("u-{name}"),
                            "annotations": {BUILD_NUMBER_ANNOTATION: number.to_string(), BUILD_CONFIG_ANNOTATION: "web"},
                            "labels": {BUILD_CONFIG_LABEL: "web", BUILD_RUN_POLICY_LABEL: policy}},
               "spec": {"serviceAccount": "builder",
                        "source": {"type": "Git", "git": {"uri": "https://git.example/web.git", "ref": "main"},
                                   "sourceSecret": {"name": "gitcreds"}},
                        "strategy": {"type": "Docker", "dockerStrategy": {"pullSecret": {"name": "pull"},
                                     "env": [{"name": "BUILD_LOGLEVEL", "value": "5"}, {"name": "SECRET", "value": "no"}]}},
                        "output": {"to": {"kind": "DockerImage", "name": "10.0.0.5:5100/demo/web:latest"},
                                   "pushSecret": {"name": "push"}},
                        "postCommit": {"script": "exit 1"}},
               "status": {"phase": phase}})
    }

    #[test]
    fn the_build_pod_is_openshifts() {
        let b = build("web-1", 1, "New", "Serial");
        let p = build_pod(&b, "builder:1").unwrap();
        assert_eq!(p["metadata"]["name"], "web-1-build");
        assert_eq!(p["metadata"]["labels"][BUILD_LABEL], "web-1");
        assert_eq!(p["metadata"]["ownerReferences"][0]["kind"], "Build");
        assert_eq!(p["spec"]["serviceAccountName"], "builder");
        assert_eq!(p["spec"]["restartPolicy"], "Never");
        assert_eq!(p["spec"]["activeDeadlineSeconds"], DEFAULT_DEADLINE);
        let inits: Vec<&str> = p["spec"]["initContainers"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(inits, ["git-clone", "manage-dockerfile"]);
        let main = &p["spec"]["containers"][0];
        assert_eq!((main["name"].clone(), main["image"].clone(), main["args"].clone()),
                   (json!("docker-build"), json!("builder:1"), json!(["openshift-docker-build"])));
        assert_eq!(main["securityContext"]["privileged"], true);
        let env: Vec<&str> = main["env"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        for want in ["BUILD", "SOURCE_URI", "SOURCE_REF", "PUSH_DOCKERCFG_PATH", "PULL_DOCKERCFG_PATH", "BUILD_LOGLEVEL",
                     "BUILD_REGISTRIES_CONF_PATH", "BUILD_BLOBCACHE_DIR"] {
            assert!(env.contains(&want), "{want} in {env:?}");
        }
        assert!(!env.contains(&"SECRET"), "only trusted strategy env reaches the builder");
        let build_json: Value = serde_json::from_str(main["env"][0]["value"].as_str().unwrap()).unwrap();
        assert_eq!(build_json["spec"]["postCommit"]["script"], "exit 1", "the post-commit hook travels in BUILD");
        // The secrets at OpenShift's paths: push and pull on the build, the
        // source secret on git-clone.
        let mounts = |c: &Value| c["volumeMounts"].as_array().unwrap().iter().map(|m| m["mountPath"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert!(mounts(main).contains(&PUSH_SECRET_PATH.to_string()));
        assert!(mounts(main).contains(&PULL_SECRET_PATH.to_string()));
        assert!(mounts(&p["spec"]["initContainers"][0]).contains(&SOURCE_SECRET_PATH.to_string()));
        let vols = p["spec"]["volumes"].as_array().unwrap();
        let vol = |n: &str| vols.iter().find(|v| v["name"] == n).cloned();
        assert_eq!(vol("gitcreds-source").unwrap()["secret"]["secretName"], "gitcreds");
        assert_eq!(vol("build-system-configs").unwrap()["configMap"]["name"], "web-1-sys-config");
        assert!(vol("container-storage-root").unwrap()["emptyDir"].is_object(), "no node paths on stormcos");
        assert!(vols.iter().all(|v| v.get("hostPath").is_none()));
        assert_eq!(p["spec"]["initContainers"][0]["securityContext"]["capabilities"]["drop"], json!(["ALL"]));
    }

    #[test]
    fn source_and_custom_pods() {
        let mut b = build("web-1", 1, "New", "Serial");
        b["spec"]["strategy"] = json!({"type": "Source", "sourceStrategy": {"from": {"kind": "DockerImage", "name": "ruby:3"}}});
        let p = build_pod(&b, "builder:1").unwrap();
        assert_eq!(p["spec"]["containers"][0]["name"], "sti-build");
        assert_eq!(p["spec"]["containers"][0]["args"], json!(["openshift-sti-build"]));
        assert!(p["spec"]["containers"][0]["env"].as_array().unwrap().contains(&env("DROP_CAPS", SOURCE_DROP_CAPS)));
        b["spec"]["strategy"] = json!({"type": "Custom", "customStrategy": {"from": {"kind": "DockerImage", "name": "my/builder:1"},
                                       "env": [{"name": "ANY", "value": "x"}]}});
        let p = build_pod(&b, "builder:1").unwrap();
        let c = &p["spec"]["containers"][0];
        assert_eq!((c["name"].clone(), c["image"].clone()), (json!("custom-build"), json!("my/builder:1")));
        let env: Vec<&str> = c["env"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(env.contains(&"ANY") && env.contains(&"OUTPUT_REGISTRY") && env.contains(&"SOURCE_SECRET_PATH"));
        assert!(p["spec"]["initContainers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn run_policies() {
        let siblings = vec![build("web-1", 1, "Running", "Serial"), build("web-2", 2, "New", "Serial"), build("web-3", 3, "New", "Serial")];
        assert!(!runnable(&siblings[1], &siblings).0, "serial: one runs at a time");
        let mut done = siblings.clone();
        done[0]["status"]["phase"] = json!("Complete");
        assert!(runnable(&done[1], &done).0, "the oldest queued one runs next");
        assert!(!runnable(&done[2], &done).0);
        let latest: Vec<Value> = done.iter().map(|b| { let mut b = b.clone(); b["metadata"]["labels"][BUILD_RUN_POLICY_LABEL] = json!("SerialLatestOnly"); b }).collect();
        let (ok, cancel) = runnable(&latest[2], &latest);
        assert!(ok && cancel == ["web-2"], "the latest runs, the older queued one is cancelled: {ok} {cancel:?}");
        let par: Vec<Value> = done.iter().map(|b| { let mut b = b.clone(); b["metadata"]["labels"][BUILD_RUN_POLICY_LABEL] = json!("Parallel"); b }).collect();
        assert!(runnable(&par[1], &par).0 && runnable(&par[2], &par).0, "parallel builds run together");
    }

    #[test]
    fn the_phase_follows_the_pod() {
        let b = build("web-1", 1, "New", "Serial");
        assert_eq!(from_pod(&b, None).reason.as_deref(), Some("BuildPodDeleted"));
        let pending = json!({"metadata": {}, "status": {"phase": "Pending"}});
        assert_eq!(from_pod(&b, Some(&pending)).phase.as_deref(), Some("Pending"));
        let cloning = json!({"metadata": {}, "status": {"phase": "Pending",
            "initContainerStatuses": [{"name": "git-clone", "state": {"running": {}}}]}});
        assert_eq!(from_pod(&b, Some(&cloning)).phase.as_deref(), Some("Running"));
        let ok = json!({"metadata": {}, "status": {"phase": "Succeeded", "containerStatuses": [{"state": {"terminated": {"exitCode": 0}}}]}});
        assert_eq!(from_pod(&b, Some(&ok)).phase.as_deref(), Some("Complete"));
        let failed = json!({"metadata": {}, "status": {"phase": "Failed"}});
        assert_eq!(from_pod(&b, Some(&failed)).reason.as_deref(), Some("GenericBuildFailed"));
        let oom = json!({"metadata": {}, "status": {"phase": "Failed", "containerStatuses": [{"state": {"terminated": {"reason": "OOMKilled"}}}]}});
        assert_eq!(from_pod(&b, Some(&oom)).reason.as_deref(), Some("OutOfMemoryKilled"));
    }

    #[test]
    fn a_failed_build_keeps_the_tail_of_its_message() {
        let b = build("web-1", 1, "Failed", "Serial");
        let pod = json!({"status": {"startTime": "2026-11-23T10:00:00Z",
            "containerStatuses": [{"state": {"terminated": {"exitCode": 1, "message": "1\n2\n3\n4\n5\n6\nerror: build error: exit status 1\n"}}}]}});
        let c = completion(&b, Some(&pod), true);
        assert_eq!(c["logSnippet"], "3\n4\n5\n6\nerror: build error: exit status 1");
        assert_eq!(c["startTimestamp"], "2026-11-23T10:00:00Z");
        assert!(c["duration"].as_i64().unwrap() > 0);
    }

    #[test]
    fn history_limits_prune_the_oldest() {
        let mut bc = json!({"metadata": {"name": "web"}, "spec": {"successfulBuildsHistoryLimit": 1, "failedBuildsHistoryLimit": 0}});
        let mut builds: Vec<Value> = (1..=3).map(|n| {
            let mut b = build(&format!("web-{n}"), n, "Complete", "Serial");
            b["metadata"]["creationTimestamp"] = json!(format!("2026-11-23T10:00:0{n}Z"));
            b
        }).collect();
        builds.push(build("web-4", 4, "Failed", "Serial"));
        let mut p = prunable(&bc, &builds);
        p.sort();
        assert_eq!(p, ["web-1", "web-2", "web-4"]);
        bc["spec"] = json!({});
        assert!(prunable(&bc, &builds).is_empty(), "no limits, no pruning");
    }

    #[test]
    fn registries_conf_marks_the_node_registry_insecure() {
        assert_eq!(registries_conf(&[]), None);
        let c = registries_conf(&["10.0.0.5:5100".into()]).unwrap();
        assert!(c.contains("location = \"10.0.0.5:5100\"\n  insecure = true"), "{c}");
    }
}
