//! `build.openshift.io/v1` Builds and BuildConfigs (#374, sc#151, sc#162):
//! the API half — what openshift-apiserver's `pkg/build` does. The pods that
//! run a build are the controller-manager's (`builds` there).
//!
//! Ported from openshift/openshift-apiserver (read 2026-11-23):
//! `apis/build/v1/defaults.go` and `conversion.go` ([`normalize_config`],
//! [`normalize_build`]), `apis/build/validation` ([`validate_config`],
//! [`validate_build`], [`validate_build_update`]), `registry/build` and
//! `registry/buildconfig` (generation, conditions, `lastVersion` never going
//! back, unknown triggers dropped), `apiserver/buildgenerator`
//! ([`instantiate`], [`clone`]), the details strategy ([`details`]) and the
//! `BuildByStrategy` admission plugin ([`authorize_strategy`]).
//!
//! **What this cluster cannot run is refused, not stored** (the no-stubs
//! rule): the JenkinsPipeline strategy (no Jenkins) and a Binary source (no
//! `instantiatebinary`) are each a field error, `Unsupported value`.
//!
//! **Triggers** (#386): ConfigChange, ImageChange and the four webhooks
//! (GitHub, Generic, GitLab, Bitbucket), validated as upstream's. An
//! `ImageStreamTag` or `ImageStreamImage` reference is resolved through the
//! aggregator — ImageStreams are the registry's (#385) — and fails as
//! OpenShift's does when the stream or tag is not there. [`instantiate`]
//! records each ImageChange trigger's image (`updateImageTriggers`, in the
//! spec's `lastTriggeredImageID` and `status.imageChangeTriggers`); the
//! controller-manager's `imagetrigger` controller starts a build when a
//! trigger's tag moves. [`webhook`] is `buildconfigs/<name>/webhooks/
//! <secret>/<type>`.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::image_policy::Fetched;
use std::future::Future;
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
use crate::storage::ResourceStorage;
use serde_json::{json, Map, Value};

pub const GROUP: &str = "build.openshift.io";
pub const API_VERSION: &str = "build.openshift.io/v1";
pub const BUILDS: &str = "builds";
pub const BUILD_CONFIGS: &str = "buildconfigs";

pub const BUILD_ANNOTATION: &str = "openshift.io/build.name";
pub const BUILD_CONFIG_ANNOTATION: &str = "openshift.io/build-config.name";
pub const BUILD_CLONE_ANNOTATION: &str = "openshift.io/build.clone-of";
pub const BUILD_NUMBER_ANNOTATION: &str = "openshift.io/build.number";
pub const BUILD_POD_NAME_ANNOTATION: &str = "openshift.io/build.pod-name";
pub const BUILD_CONFIG_PAUSED_ANNOTATION: &str = "openshift.io/build-config.paused";
pub const BUILD_CONFIG_LABEL: &str = "openshift.io/build-config.name";
pub const BUILD_CONFIG_LABEL_DEPRECATED: &str = "buildconfig";
pub const BUILD_LABEL: &str = "openshift.io/build.name";
pub const BUILD_RUN_POLICY_LABEL: &str = "openshift.io/build.start-policy";
/// OpenShift's `bootstrappolicy.BuilderServiceAccountName`.
pub const BUILDER_SERVICE_ACCOUNT: &str = "builder";
const CUSTOM_BASE_IMAGE_ENV: &str = "OPENSHIFT_CUSTOM_BUILD_BASE_IMAGE";

/// The strategies this cluster runs, as `spec.strategy.type` names them.
const STRATEGIES: [&str; 3] = ["Docker", "Source", "Custom"];
/// The trigger types this cluster fires.
const TRIGGERS: [&str; 6] = ["ConfigChange", "ImageChange", "GitHub", "Generic", "GitLab", "Bitbucket"];
/// The key of a webhook trigger's `secretReference` Secret.
pub const WEBHOOK_SECRET_KEY: &str = "WebHookSecretKey";
/// OpenShift's `KnownTriggerTypes`: any other is dropped on write.
const KNOWN_TRIGGERS: [&str; 6] = ["GitHub", "Generic", "ImageChange", "ConfigChange", "GitLab", "Bitbucket"];
/// The source types a build can take here (`Binary` needs `instantiatebinary`).
const SOURCE_TYPES: [&str; 4] = ["Git", "Dockerfile", "Image", "None"];

// --- field errors, upstream's text ------------------------------------------

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}
fn invalid(path: &str, value: &str, detail: &str) -> String {
    format!("{path}: Invalid value: {value}: {detail}")
}
fn required(path: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("{path}: Required value")
    } else {
        format!("{path}: Required value: {detail}")
    }
}
fn not_supported(path: &str, value: &str, valid: &[&str]) -> String {
    let quoted: Vec<String> = valid.iter().map(|v| q(v)).collect();
    format!("{path}: Unsupported value: {}: supported values: {}", q(value), quoted.join(", "))
}
fn refuse(kind: &str, obj: &Value, errs: Vec<String>) -> Result<(), ApiError> {
    if errs.is_empty() {
        return Ok(());
    }
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let what = if errs.len() == 1 { errs[0].clone() } else { format!("[{}]", errs.join(", ")) };
    Err(ApiError::invalid(&format!("{kind}.{GROUP} {name:?} is invalid: {what}")))
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

// --- defaulting and v1's type fields --------------------------------------------

/// `SetDefaults_*` and the v1 conversion's `type` fields, on a `CommonSpec`.
fn normalize_common(spec: &mut Value) {
    if !spec.is_object() {
        return;
    }
    // SetDefaults_BuildSource: a Binary type implies a binary source.
    if spec["source"]["type"] == "Binary" && spec["source"]["binary"].is_null() {
        spec["source"]["binary"] = json!({});
    }
    // SetDefaults_BuildStrategy: a Docker type implies a docker strategy.
    if spec["strategy"]["type"] == "Docker" && spec["strategy"]["dockerStrategy"].is_null() {
        spec["strategy"]["dockerStrategy"] = json!({});
    }
    for (field, always) in [("sourceStrategy", true), ("dockerStrategy", false), ("customStrategy", true)] {
        let Some(st) = spec.get_mut("strategy").and_then(|v| v.get_mut(field)).filter(|v| v.is_object()) else {
            continue;
        };
        if (always || st["from"].is_object()) && s(&st["from"], "kind").is_empty() {
            st["from"]["kind"] = json!("ImageStreamTag");
        }
    }
    // The v1 conversion writes the type from what is set.
    let st = &spec["strategy"];
    let ty = if st["sourceStrategy"].is_object() {
        "Source"
    } else if st["dockerStrategy"].is_object() {
        "Docker"
    } else if st["customStrategy"].is_object() {
        "Custom"
    } else if st["jenkinsPipelineStrategy"].is_object() {
        "JenkinsPipeline"
    } else {
        ""
    };
    if spec["strategy"].is_object() || !ty.is_empty() {
        spec["strategy"]["type"] = json!(ty);
    }
    let src = &spec["source"];
    let sty = if src["git"].is_object() {
        "Git"
    } else if src["binary"].is_object() {
        "Binary"
    } else if !src["dockerfile"].is_null() {
        "Dockerfile"
    } else if src["images"].as_array().is_some_and(|a| !a.is_empty()) {
        "Image"
    } else {
        "None"
    };
    spec["source"]["type"] = json!(sty);
    // An ImageStreamTag output without a tag is `:latest`.
    let Some(to) = spec.get_mut("output").and_then(|v| v.get_mut("to")) else { return };
    if to["kind"] == "ImageStreamTag" {
        if let Some(n) = to["name"].as_str().filter(|n| !n.is_empty() && !n.contains(':')) {
            to["name"] = json!(format!("{n}:latest"));
        }
    }
}

/// A BuildConfig as OpenShift stores it: defaults, the trigger types'
/// canonical names, unknown triggers dropped.
pub fn normalize_config(bc: &mut Value) {
    if !bc["spec"].is_object() {
        bc["spec"] = json!({});
    }
    if s(&bc["spec"], "runPolicy").is_empty() {
        bc["spec"]["runPolicy"] = json!("Serial");
    }
    let triggers: Vec<Value> = bc["spec"]["triggers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|mut t| {
            let canonical = match s(&t, "type") {
                "imageChange" => "ImageChange",
                "generic" => "Generic",
                "github" => "GitHub",
                other => other,
            }
            .to_string();
            t["type"] = json!(canonical);
            if canonical == "ImageChange" && t["imageChange"].is_null() {
                t["imageChange"] = json!({});
            }
            KNOWN_TRIGGERS.contains(&canonical.as_str()).then_some(t)
        })
        .collect();
    bc["spec"]["triggers"] = json!(triggers);
    normalize_common(&mut bc["spec"]);
    if !bc["status"].is_object() {
        bc["status"] = json!({"lastVersion": 0});
    } else if bc["status"]["lastVersion"].is_null() {
        bc["status"]["lastVersion"] = json!(0);
    }
}

pub fn normalize_build(build: &mut Value) {
    if !build["spec"].is_object() {
        build["spec"] = json!({});
    }
    normalize_common(&mut build["spec"]);
}

// --- validation ---------------------------------------------------------------------

fn validate_name(obj: &Value, errs: &mut Vec<String>) {
    let name = s(&obj["metadata"], "name");
    if name.is_empty() {
        errs.push(required("metadata.name", "name or generateName is required"));
    } else if !crate::user_groups::is_dns_subdomain(name) {
        errs.push(invalid(
            "metadata.name",
            &q(name),
            "a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters, '-' or '.', and must \
             start and end with an alphanumeric character (e.g. 'example.com', regex used for validation is \
             '[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*')",
        ));
    }
}

/// `path.Clean`, enough for the relative-path checks.
fn clean(p: &str) -> String {
    let abs = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else if !abs {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    match (abs, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// A docker pull spec, as `reference.Parse` accepts one: an optional
/// registry host, lower-case path components, an optional tag and digest.
pub fn valid_pull_spec(spec: &str) -> bool {
    if spec.is_empty() || spec.len() > 255 + 128 + 80 {
        return false;
    }
    // The first component is a registry only when it looks like a host
    // (has a `.` or a port, or is `localhost`), as the reference parser
    // decides; otherwise it is part of the (lower-case) repository path.
    let rest = match spec.split_once('/') {
        Some((first, rest)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            let host = regex::Regex::new(
                r"^(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?)*|\[[0-9a-fA-F:]+\])(?::[0-9]+)?$",
            )
            .expect("valid regex");
            if !host.is_match(first) {
                return false;
            }
            rest
        }
        _ => spec,
    };
    let path = regex::Regex::new(
        r"^[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*)*(?::[\w][\w.-]{0,127})?(?:@[A-Za-z][A-Za-z0-9]*(?:[-_+.][A-Za-z][A-Za-z0-9]*)*:[0-9a-fA-F]{32,})?$",
    )
    .expect("valid regex");
    path.is_match(rest)
}

fn valid_git_url(uri: &str) -> bool {
    if uri.chars().any(char::is_whitespace) {
        return false;
    }
    if let Some((scheme, rest)) = uri.split_once("://") {
        return matches!(scheme, "http" | "https" | "ssh" | "git" | "file") && !rest.is_empty();
    }
    // scp-like `user@host:path`, or a local path.
    uri.starts_with('/') || (uri.contains('@') && uri.contains(':'))
}

fn validate_secret_ref(r: &Value, path: &str, errs: &mut Vec<String>) {
    if r.is_object() && s(r, "name").is_empty() {
        errs.push(required(&format!("{path}.name"), ""));
    }
}

fn validate_image_reference(r: &Value, path: &str, errs: &mut Vec<String>) {
    if !r.is_object() {
        return;
    }
    let (kind, name, ns) = (s(r, "kind"), s(r, "name"), s(r, "namespace"));
    match kind {
        "ImageStreamTag" => {
            if name.is_empty() {
                errs.push(required(&format!("{path}.name"), ""));
            } else if name.matches(':').count() != 1 || name.ends_with(':') || name.starts_with(':') {
                errs.push(invalid(&format!("{path}.name"), &q(name), "must be <name>:<tag>"));
            }
            if !ns.is_empty() && !crate::user_groups::is_dns_subdomain(ns) {
                errs.push(invalid(&format!("{path}.namespace"), &q(ns), "must be a valid namespace"));
            }
        }
        "DockerImage" => {
            if !ns.is_empty() {
                errs.push(invalid(&format!("{path}.namespace"), &q(ns), "not valid when used with a 'DockerImage'"));
            }
            if name.is_empty() {
                errs.push(required(&format!("{path}.name"), ""));
            } else if !valid_pull_spec(name) {
                errs.push(invalid(&format!("{path}.name"), &q(name), "not a valid Docker pull specification: invalid reference format"));
            }
        }
        "ImageStreamImage" => {
            if name.is_empty() {
                errs.push(required(&format!("{path}.name"), ""));
            }
            if !ns.is_empty() && !crate::user_groups::is_dns_subdomain(ns) {
                errs.push(invalid(&format!("{path}.namespace"), &q(ns), "must be a valid namespace"));
            }
        }
        "" => errs.push(required(&format!("{path}.kind"), "")),
        other => errs.push(invalid(
            &format!("{path}.kind"),
            &q(other),
            "must be 'ImageStreamTag', 'ImageStreamImage', or 'DockerImage'",
        )),
    }
}

fn validate_to_reference(r: &Value, path: &str, errs: &mut Vec<String>) {
    let (kind, name, ns) = (s(r, "kind"), s(r, "name"), s(r, "namespace"));
    match kind {
        "ImageStreamTag" => {
            if name.is_empty() {
                errs.push(required(&format!("{path}.name"), ""));
            } else if name.matches(':').count() != 1 {
                errs.push(invalid(
                    &format!("{path}.name"),
                    &q(name),
                    "ImageStreamTag object references must be in the form <name>:<tag>",
                ));
            }
            if !ns.is_empty() && !crate::user_groups::is_dns_subdomain(ns) {
                errs.push(invalid(&format!("{path}.namespace"), &q(ns), "namespace must be a valid subdomain"));
            }
        }
        "DockerImage" => {
            if !ns.is_empty() {
                errs.push(invalid(&format!("{path}.namespace"), &q(ns), "namespace is not valid when used with a 'DockerImage'"));
            }
            if !valid_pull_spec(name) {
                errs.push(invalid(
                    &format!("{path}.name"),
                    &q(name),
                    "name is not a valid Docker pull specification: invalid reference format",
                ));
            }
        }
        "" => errs.push(required(&format!("{path}.kind"), "")),
        other => errs.push(invalid(
            &format!("{path}.kind"),
            &q(other),
            "the target of build output must be an 'ImageStreamTag' or 'DockerImage'",
        )),
    }
}

fn validate_env(env: &Value, path: &str, errs: &mut Vec<String>) {
    let re = regex::Regex::new(r"^[-._a-zA-Z][-._a-zA-Z0-9]*$").expect("valid regex");
    for (i, ev) in env.as_array().into_iter().flatten().enumerate() {
        let name = s(ev, "name");
        if name.is_empty() {
            errs.push(required(&format!("{path}[{i}].name"), ""));
        } else if !re.is_match(name) {
            errs.push(invalid(
                &format!("{path}[{i}].name"),
                &q(name),
                "a valid environment variable name must consist of alphabetic characters, digits, '_', '-', or '.', \
                 and must not start with a digit (e.g. 'my.env-name',  or 'MY_ENV.NAME',  or 'MyEnvName1', regex used \
                 for validation is '[-._a-zA-Z][-._a-zA-Z0-9]*')",
            ));
        }
        if ev["valueFrom"]["resourceFieldRef"].is_object() {
            errs.push(invalid(
                &format!("{path}[{i}].valueFrom.ResourceFieldRef"),
                &q(name),
                "ResourceFieldRef is not valid for valueFrom in build environment variables",
            ));
        }
    }
}

fn validate_relative(p: &str, field: &str, path: &str, errs: &mut Vec<String>) {
    let c = clean(p);
    if c.starts_with('/') || c == ".." || c.starts_with("../") {
        errs.push(invalid(path, &q(p), &format!("{field} must be a relative path within your source location")));
    }
}

fn validate_volumes(vols: &Value, path: &str, errs: &mut Vec<String>) {
    for (i, v) in vols.as_array().into_iter().flatten().enumerate() {
        let p = format!("{path}[{i}]");
        let name = s(v, "name");
        if name.is_empty() {
            errs.push(required(&format!("{p}.name"), ""));
        } else if !crate::user_groups::is_dns_label(name) {
            errs.push(invalid(&format!("{p}.name"), &q(name), "a lowercase RFC 1123 label must consist of lower case alphanumeric characters or '-', and must start and end with an alphanumeric character"));
        }
        let src = &v["source"];
        let ty = s(src, "type");
        if ty.is_empty() {
            errs.push(required(&format!("{p}.source.type"), "must not be blank"));
        }
        let found: Vec<&str> = [("secret", "Secret"), ("configMap", "ConfigMap"), ("csi", "CSI")]
            .into_iter()
            .filter(|(k, _)| src[*k].is_object())
            .map(|(_, t)| t)
            .collect();
        if src["secret"].is_object() && s(&src["secret"], "secretName").is_empty() {
            errs.push(required(&format!("{p}.source.secret.secretName"), ""));
        }
        if src["configMap"].is_object() && s(&src["configMap"], "name").is_empty() {
            errs.push(required(&format!("{p}.source.configMap.name"), ""));
        }
        if src["csi"].is_object() && s(&src["csi"], "driver").is_empty() {
            errs.push(required(&format!("{p}.source.csi.driver"), ""));
        }
        match found.len() {
            0 => errs.push(invalid(&format!("{p}.source"), "{}", "must specify one volume source")),
            1 if found[0] != ty && !ty.is_empty() => {
                errs.push(invalid(&format!("{p}.source.type"), &q(ty), "source type and specified type must match"))
            }
            1 => {}
            _ => errs.push(invalid(&format!("{p}.source"), "{}", "only one volume source is allowed")),
        }
        let mounts = v["mounts"].as_array().cloned().unwrap_or_default();
        if mounts.is_empty() {
            errs.push(required(&format!("{p}.mounts"), "must supply at least one mount"));
        }
        for (j, m) in mounts.iter().enumerate() {
            let d = s(m, "destinationPath");
            if d.is_empty() {
                errs.push(required(&format!("{p}.mounts.destinationPath"), "must not be blank"));
                continue;
            }
            if d.contains(':') {
                errs.push(invalid(&format!("{p}.mounts[{j}].destinationPath"), &q(d), "must not contain ':'"));
            }
            if !d.starts_with('/') {
                errs.push(invalid(&format!("{p}.mounts[{j}].destinationPath"), &q(d), "must be an absolute path"));
            }
        }
    }
}

fn validate_source(spec: &Value, errs: &mut Vec<String>) {
    let src = &spec["source"];
    let is_custom = spec["strategy"]["customStrategy"].is_object();
    let is_docker = spec["strategy"]["dockerStrategy"].is_object();
    if src["git"].is_object() && src["binary"].is_object() && !is_custom {
        errs.push(invalid("spec.source.git", "\"\"", "may not be set when binary is also set"));
        errs.push(invalid("spec.source.binary", "\"\"", "may not be set when git is also set"));
        return;
    }
    // Binary builds need `instantiatebinary`, which is not served.
    let ty = s(src, "type");
    if !ty.is_empty() && !SOURCE_TYPES.contains(&ty) {
        errs.push(not_supported("spec.source.type", ty, &SOURCE_TYPES));
    }
    if src["git"].is_object() {
        let uri = s(&src["git"], "uri");
        if uri.is_empty() {
            errs.push(required("spec.source.git.uri", ""));
        } else if !valid_git_url(uri) {
            errs.push(invalid("spec.source.git.uri", &q(uri), "invalid git url"));
        }
    }
    if let Some(d) = src["dockerfile"].as_str() {
        if d.len() > 60_000 {
            errs.push(invalid("spec.source.dockerfile", "\"\"", "must be smaller than 60000 bytes"));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for (i, image) in src["images"].as_array().into_iter().flatten().enumerate() {
        let p = format!("spec.source.images[{i}]");
        validate_image_reference(&image["from"], &format!("{p}.from"), errs);
        validate_secret_ref(&image["pullSecret"], &format!("{p}.pullSecret"), errs);
        let paths = image["paths"].as_array().cloned().unwrap_or_default();
        let r#as = image["as"].as_array().cloned().unwrap_or_default();
        if paths.is_empty() && r#as.is_empty() {
            errs.push(required(&format!("{p}.paths"), "must specify 'paths' or 'as'"));
            errs.push(required(&format!("{p}.as"), "must specify 'paths' or 'as'"));
        }
        for (j, ip) in paths.iter().enumerate() {
            let (sp, dd) = (s(ip, "sourcePath"), s(ip, "destinationDir"));
            if sp.is_empty() {
                errs.push(required(&format!("{p}.paths[{j}].sourcePath"), ""));
            }
            if dd.is_empty() {
                errs.push(required(&format!("{p}.paths[{j}].destinationDir"), ""));
            }
            if !sp.starts_with('/') {
                errs.push(invalid(&format!("{p}.paths[{j}].sourcePath"), &q(sp), "must be an absolute path"));
            }
            if dd.starts_with('/') {
                errs.push(invalid(&format!("{p}.paths[{j}].destinationDir"), &q(dd), "must be a relative path"));
            }
            if clean(dd).starts_with("..") {
                errs.push(invalid(&format!("{p}.paths[{j}].destinationDir"), &q(dd), "destination dir cannot start with '..'"));
            }
        }
        for (j, a) in r#as.iter().enumerate() {
            let a = a.as_str().unwrap_or("");
            if a.is_empty() {
                errs.push(invalid(&format!("{p}.as[{j}]"), "\"\"", "must not be empty"));
            } else if !seen.insert(a.to_string()) {
                errs.push(format!("{p}.as[{j}]: Duplicate value: {}", q(a)));
            }
        }
    }
    for (field, key, what) in [("secrets", "secret", "secret"), ("configMaps", "configMap", "configMap")] {
        for (i, item) in src[field].as_array().into_iter().flatten().enumerate() {
            let p = format!("spec.source.{field}[{i}]");
            let name = s(&item[key], "name");
            if name.is_empty() {
                errs.push(required(&format!("{p}.{key}"), ""));
            } else if !crate::user_groups::is_dns_subdomain(name) {
                errs.push(invalid(&format!("{p}.{key}"), &q(name), &format!("must be valid {what} name")));
            }
            let dd = s(item, "destinationDir");
            if clean(dd).starts_with("..") {
                errs.push(invalid(&format!("{p}.destinationDir"), &q(dd), "destination dir cannot start with '..'"));
            }
            if is_docker && dd.starts_with('/') {
                errs.push(invalid(
                    &format!("{p}.destinationDir"),
                    &q(dd),
                    "for the docker strategy the destinationDir has to be relative path",
                ));
            }
        }
    }
    validate_secret_ref(&src["sourceSecret"], "spec.source.sourceSecret", errs);
    let cd = s(src, "contextDir");
    if !cd.is_empty() && clean(cd).starts_with("..") {
        errs.push(invalid("spec.source.contextDir", &q(cd), "context dir must not be a relative path"));
    }
}

fn validate_strategy(spec: &Value, errs: &mut Vec<String>) {
    let st = &spec["strategy"];
    let set: Vec<&str> = ["sourceStrategy", "dockerStrategy", "customStrategy", "jenkinsPipelineStrategy"]
        .into_iter()
        .filter(|k| st[*k].is_object())
        .collect();
    if set.len() != 1 {
        errs.push(invalid(
            "spec.strategy",
            "{}",
            "must provide a value for exactly one of sourceStrategy, customStrategy, dockerStrategy, or jenkinsPipelineStrategy",
        ));
        return;
    }
    let ty = s(st, "type");
    if !STRATEGIES.contains(&ty) {
        errs.push(not_supported("spec.strategy.type", ty, &STRATEGIES));
        return;
    }
    match set[0] {
        "dockerStrategy" => {
            let d = &st["dockerStrategy"];
            validate_image_reference(&d["from"], "spec.strategy.dockerStrategy.from", errs);
            validate_secret_ref(&d["pullSecret"], "spec.strategy.dockerStrategy.pullSecret", errs);
            if let Some(p) = d["imageOptimizationPolicy"].as_str() {
                if !["None", "SkipLayers", "SkipLayersAndWarn"].contains(&p) {
                    errs.push(invalid(
                        "spec.strategy.dockerStrategy.imageOptimizationPolicy",
                        &q(p),
                        "must be unset, 'None', 'SkipLayers', or 'SkipLayersAndWarn'",
                    ));
                }
            }
            if let Some(p) = d["dockerfilePath"].as_str().filter(|p| !p.is_empty()) {
                validate_relative(p, "dockerfilePath", "spec.strategy.dockerStrategy.dockerfilePath", errs);
            }
            validate_volumes(&d["volumes"], "spec.strategy.dockerStrategy.volumes", errs);
            validate_env(&d["env"], "spec.strategy.dockerStrategy.env", errs);
        }
        "sourceStrategy" => {
            let d = &st["sourceStrategy"];
            validate_image_reference(&d["from"], "spec.strategy.sourceStrategy.from", errs);
            validate_secret_ref(&d["pullSecret"], "spec.strategy.sourceStrategy.pullSecret", errs);
            validate_env(&d["env"], "spec.strategy.sourceStrategy.env", errs);
            validate_volumes(&d["volumes"], "spec.strategy.sourceStrategy.volumes", errs);
        }
        _ => {
            let d = &st["customStrategy"];
            if d["exposeDockerSocket"] == true {
                // No node here has a Docker socket to give it.
                errs.push(not_supported("spec.strategy.customStrategy.exposeDockerSocket", "true", &["false"]));
            }
            validate_image_reference(&d["from"], "spec.strategy.customStrategy.from", errs);
            validate_secret_ref(&d["pullSecret"], "spec.strategy.customStrategy.pullSecret", errs);
            validate_env(&d["env"], "spec.strategy.customStrategy.env", errs);
        }
    }
}

fn validate_common(spec: &Value, errs: &mut Vec<String>) {
    let src = &spec["source"];
    if spec["strategy"]["dockerStrategy"].is_object()
        && !src["git"].is_object()
        && !src["binary"].is_object()
        && src["dockerfile"].is_null()
        && src["images"].is_null()
    {
        errs.push(invalid(
            "spec.source",
            "\"\"",
            "must provide a value for at least one source input(git, binary, dockerfile, images).",
        ));
    }
    validate_source(spec, errs);
    if let Some(d) = spec["completionDeadlineSeconds"].as_i64() {
        if d <= 0 {
            errs.push(invalid(
                "spec.completionDeadlineSeconds",
                &d.to_string(),
                "completionDeadlineSeconds must be a positive integer greater than 0",
            ));
        }
    }
    let out = &spec["output"];
    if out["to"].is_object() {
        validate_to_reference(&out["to"], "spec.output.to", errs);
    }
    validate_secret_ref(&out["pushSecret"], "spec.output.pushSecret", errs);
    let mut seen = std::collections::HashSet::new();
    for (i, l) in out["imageLabels"].as_array().into_iter().flatten().enumerate() {
        let n = s(l, "name");
        if n.is_empty() {
            errs.push(required(&format!("spec.output.imageLabels[{i}].name"), ""));
        } else if !seen.insert(n.to_string()) {
            errs.push(invalid(&format!("spec.output.imageLabels[{i}].name"), &q(n), "duplicate name"));
        }
    }
    validate_strategy(spec, errs);
    let pc = &spec["postCommit"];
    if !s(pc, "script").is_empty() && pc["command"].as_array().is_some_and(|c| !c.is_empty()) {
        errs.push(invalid("spec.postCommit", "{}", "cannot use command and script together"));
    }
}

/// The field a webhook trigger type keeps its secret under.
pub fn webhook_field(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "GitHub" => "github",
        "Generic" => "generic",
        "GitLab" => "gitlab",
        "Bitbucket" => "bitbucket",
        _ => return None,
    })
}

/// `GetInputReference`: the strategy's base image reference.
pub fn strategy_from(spec: &Value) -> &Value {
    let st = &spec["strategy"];
    for k in ["sourceStrategy", "dockerStrategy", "customStrategy"] {
        if st[k]["from"].is_object() {
            return &st[k]["from"];
        }
    }
    &Value::Null
}

/// `validateTrigger` for every trigger, and the config-wide rules on
/// ImageChange triggers (one default, no two the same).
fn validate_triggers(bc: &Value, errs: &mut Vec<String>) {
    let mut defaults = 0;
    let mut seen: Vec<(String, String)> = Vec::new();
    for (i, t) in bc["spec"]["triggers"].as_array().into_iter().flatten().enumerate() {
        let at = format!("spec.triggers[{i}]");
        let ty = s(t, "type");
        if ty.is_empty() {
            errs.push(required(&format!("{at}.type"), ""));
            continue;
        }
        if !TRIGGERS.contains(&ty) {
            errs.push(not_supported(&format!("{at}.type"), ty, &TRIGGERS));
            continue;
        }
        if let Some(field) = webhook_field(ty) {
            let w = &t[field];
            if !w.is_object() {
                errs.push(required(&format!("{at}.{field}"), ""));
                continue;
            }
            let has_ref = w["secretReference"].is_object();
            match (s(w, "secret").is_empty(), has_ref) {
                (true, false) => errs.push(required(&format!("{at}.{field}.secret"), "")),
                (false, true) => errs.push(invalid(&format!("{at}.{field}.secretReference"), "{}",
                                                   "secret and secretReference may not both be set")),
                _ => {}
            }
            if has_ref && s(&w["secretReference"], "name").is_empty() {
                errs.push(required(&format!("{at}.{field}.secretReference.name"), ""));
            }
            continue;
        }
        if ty != "ImageChange" {
            continue;
        }
        let ic = &t["imageChange"];
        if !ic.is_object() {
            errs.push(required(&format!("{at}.imageChange"), ""));
            continue;
        }
        let from = &ic["from"];
        let effective = if from.is_object() {
            let (kind, name) = (s(from, "kind"), s(from, "name"));
            if kind != "ImageStreamTag" {
                errs.push(invalid(&format!("{at}.imageChange.from.kind"), &q(kind), "only an ImageStreamTag type of reference is allowed"));
            }
            if name.is_empty() {
                errs.push(required(&format!("{at}.imageChange.from.name"), ""));
            } else if !apimachinery::imagestream::valid_tag_name(name) {
                errs.push(invalid(&format!("{at}.imageChange.from.name"), &q(name),
                                  "ImageStreamTag object references must be in the form <name>:<tag>"));
            }
            from
        } else {
            defaults += 1;
            let sf = strategy_from(&bc["spec"]);
            if s(sf, "kind") != "ImageStreamTag" {
                errs.push(invalid(&format!("{at}.imageChange"), "{}",
                                  "a default ImageChange trigger can only be used when the strategy includes an ImageStreamTag reference."));
                continue;
            }
            sf
        };
        let ns = match s(effective, "namespace") {
            "" => s(&bc["metadata"], "namespace"),
            n => n,
        };
        let key = (ns.to_string(), s(effective, "name").to_string());
        if seen.contains(&key) {
            errs.push(invalid(&format!("{at}.imageChange"), "{}", "multiple ImageChange triggers with the same reference are not allowed"));
        }
        seen.push(key);
    }
    if defaults > 1 {
        errs.push(invalid("spec.triggers", "[]", "only one ImageChange trigger may have no from reference"));
    }
}

pub fn validate_config(bc: &Value) -> Result<(), ApiError> {
    let mut errs = Vec::new();
    validate_name(bc, &mut errs);
    validate_triggers(bc, &mut errs);
    let rp = s(&bc["spec"], "runPolicy");
    if !["Parallel", "Serial", "SerialLatestOnly"].contains(&rp) {
        errs.push(invalid("spec.runPolicy", &q(rp), "run policy must Parallel, Serial, or SerialLatestOnly"));
    }
    for f in ["successfulBuildsHistoryLimit", "failedBuildsHistoryLimit"] {
        if let Some(n) = bc["spec"][f].as_i64().filter(|n| *n < 0) {
            errs.push(invalid(&format!("spec.{f}"), &n.to_string(), "must be greater than or equal to 0"));
        }
    }
    validate_common(&bc["spec"], &mut errs);
    refuse("BuildConfig", bc, errs)
}

pub fn validate_build(build: &Value) -> Result<(), ApiError> {
    let mut errs = Vec::new();
    validate_name(build, &mut errs);
    validate_common(&build["spec"], &mut errs);
    refuse("Build", build, errs)
}

pub fn is_complete(build: &Value) -> bool {
    matches!(s(&build["status"], "phase"), "Complete" | "Failed" | "Error" | "Cancelled")
}

/// `ValidateBuildUpdate`: the spec is immutable but for `output.pushSecret`,
/// and a finished build's phase does not change.
pub fn validate_build_update(build: &Value, old: &Value) -> Result<(), ApiError> {
    let mut errs = Vec::new();
    validate_name(build, &mut errs);
    if is_complete(old) && old["status"]["phase"] != build["status"]["phase"] {
        errs.push(invalid(
            "status.phase",
            &q(s(&build["status"], "phase")),
            "phase cannot be updated from a terminal state",
        ));
    }
    // The push secret may change (upstream lies about the old one); the rest
    // may not.
    let without_push_secret = |v: &Value| {
        let mut v = v.clone();
        let emptied = match v.get_mut("output").and_then(Value::as_object_mut) {
            Some(o) => {
                o.remove("pushSecret");
                o.is_empty()
            }
            None => false,
        };
        if emptied {
            if let Some(m) = v.as_object_mut() {
                m.remove("output");
            }
        }
        v
    };
    if without_push_secret(&build["spec"]) != without_push_secret(&old["spec"]) {
        errs.push(invalid(
            "spec",
            "\"content of spec is not printed out, please refer to the details\"",
            "spec is immutable",
        ));
    }
    refuse("Build", build, errs)
}

// --- the registry strategies -------------------------------------------------------

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `manageConditions`: the condition of the current phase True (with the
/// status's reason and message), every other False.
pub fn manage_conditions(build: &mut Value) {
    let phase = s(&build["status"], "phase").to_string();
    if phase.is_empty() {
        return;
    }
    let (reason, message) = (s(&build["status"], "reason").to_string(), s(&build["status"], "message").to_string());
    let t = now();
    let mut conds = build["status"]["conditions"].as_array().cloned().unwrap_or_default();
    let mut found = false;
    for c in conds.iter_mut() {
        if c["type"] == phase.as_str() {
            found = true;
            if c["status"] != "True" || s(c, "reason") != reason || s(c, "message") != message {
                if c["status"] != "True" {
                    c["status"] = json!("True");
                    c["lastTransitionTime"] = json!(t);
                }
                c["lastUpdateTime"] = json!(t);
                c["reason"] = json!(reason);
                c["message"] = json!(message);
            }
        } else if c["status"] != "False" {
            c["status"] = json!("False");
            c["lastTransitionTime"] = json!(t);
            c["lastUpdateTime"] = json!(t);
            c["reason"] = json!("");
            c["message"] = json!("");
        }
    }
    if !found {
        conds.push(json!({"type": phase, "status": "True", "lastUpdateTime": t, "lastTransitionTime": t,
                          "reason": reason, "message": message}));
    }
    build["status"]["conditions"] = json!(conds);
}

/// A Build create: defaults, phase New, conditions, validation.
pub fn admit_build_create(build: &mut Value) -> Result<(), ApiError> {
    normalize_build(build);
    build["metadata"]["generation"] = json!(1);
    if s(&build["status"], "phase").is_empty() {
        if !build["status"].is_object() {
            build["status"] = json!({});
        }
        build["status"]["phase"] = json!("New");
    }
    manage_conditions(build);
    validate_build(build)
}

/// A Build update through the main resource (the build controller's, and
/// `oc cancel-build`'s): a failed build keeps its reason, conditions follow
/// the phase, the spec stays.
pub fn admit_build_update(build: &mut Value, old: &Value) -> Result<(), ApiError> {
    normalize_build(build);
    if old["status"]["phase"] == "Failed" && build["status"]["reason"] != "OutOfMemoryKilled" {
        build["status"]["reason"] = old["status"]["reason"].clone();
        build["status"]["message"] = old["status"]["message"].clone();
    }
    manage_conditions(build);
    let gen = old["metadata"]["generation"].as_i64().unwrap_or(1);
    build["metadata"]["generation"] = json!(if build["spec"] != old["spec"] { gen + 1 } else { gen });
    validate_build_update(build, old)
}

pub fn admit_config_create(bc: &mut Value) -> Result<(), ApiError> {
    normalize_config(bc);
    // The group API's layered defaults (`groupStrategy.PrepareForCreate`).
    for k in ["successfulBuildsHistoryLimit", "failedBuildsHistoryLimit"] {
        if bc["spec"][k].is_null() {
            bc["spec"][k] = json!(5);
        }
    }
    bc["metadata"]["generation"] = json!(1);
    validate_config(bc)
}

/// A BuildConfig update: `status.lastVersion` never goes back.
pub fn admit_config_update(bc: &mut Value, old: &Value) -> Result<(), ApiError> {
    normalize_config(bc);
    let (new_v, old_v) = (bc["status"]["lastVersion"].as_i64().unwrap_or(0), old["status"]["lastVersion"].as_i64().unwrap_or(0));
    if new_v < old_v {
        bc["status"]["lastVersion"] = json!(old_v);
    }
    let gen = old["metadata"]["generation"].as_i64().unwrap_or(1);
    let changed = bc["spec"] != old["spec"] || bc["status"]["lastVersion"] != old["status"]["lastVersion"];
    bc["metadata"]["generation"] = json!(if changed { gen + 1 } else { gen });
    validate_config(bc)
}

/// `builds/details` (the builder's own update): only the revision, the
/// stages, the reason and message, the output, and a terminal phase come
/// from the body; a revision once set does not change.
pub fn details(body: &Value, stored: &Value) -> Result<Value, ApiError> {
    let (old_rev, new_rev) = (&stored["spec"]["revision"], &body["spec"]["revision"]);
    let mut errs = Vec::new();
    if new_rev.is_null() && !old_rev.is_null() {
        errs.push(invalid("spec.revision", "\"null\"", "cannot set an empty revision in build spec"));
    } else if !old_rev.is_null() && old_rev != new_rev {
        errs.push(format!("spec.revision: Duplicate value: {old_rev}"));
    }
    refuse("Build", stored, errs)?;
    let mut out = stored.clone();
    if is_complete(body) {
        out["status"]["phase"] = body["status"]["phase"].clone();
    }
    out["status"]["stages"] = body["status"]["stages"].clone();
    out["spec"]["revision"] = new_rev.clone();
    out["status"]["reason"] = body["status"]["reason"].clone();
    out["status"]["message"] = body["status"]["message"].clone();
    match &body["status"]["output"]["to"] {
        Value::Null => {
            if let Some(o) = out["status"].get_mut("output").and_then(Value::as_object_mut) {
                o.remove("to");
            }
        }
        to => out["status"]["output"]["to"] = to.clone(),
    }
    // The resourceVersion the builder sent is not a precondition here
    // (`AllowUnconditionalUpdate`): the stored one is.
    manage_conditions(&mut out);
    Ok(out)
}

// --- strategy restriction (BuildByStrategy) ---------------------------------------

/// The subresource of `builds` a strategy needs `create` on.
fn strategy_resource(strategy: &Value) -> Option<&'static str> {
    if let Some(d) = strategy["dockerStrategy"].as_object() {
        let optimized = d.get("imageOptimizationPolicy").and_then(Value::as_str).is_some_and(|p| p != "None");
        return Some(if optimized { "optimizeddocker" } else { "docker" });
    }
    if strategy["customStrategy"].is_object() {
        return Some("custom");
    }
    if strategy["sourceStrategy"].is_object() {
        return Some("source");
    }
    if strategy["jenkinsPipelineStrategy"].is_object() {
        return Some("jenkinspipeline");
    }
    None
}

fn strategy_type(strategy: &Value) -> &'static str {
    if strategy["dockerStrategy"].is_object() {
        "Docker"
    } else if strategy["customStrategy"].is_object() {
        "Custom"
    } else if strategy["sourceStrategy"].is_object() {
        "Source"
    } else if strategy["jenkinsPipelineStrategy"].is_object() {
        "JenkinsPipeline"
    } else {
        ""
    }
}

/// `create` on `builds/<strategy>` in the namespace, for the requester of a
/// Build, a BuildConfig, or a BuildRequest naming one: OpenShift's
/// `BuildByStrategy` admission. Unauthorized requests (no RBAC engine: tests,
/// the controllers' own writes through storage) pass.
pub async fn authorize_strategy(
    rbac: Option<&RbacEngine>,
    user: &UserInfo,
    namespace: &str,
    name: &str,
    resource: &str,
    obj: &Value,
) -> Result<(), ApiError> {
    let Some(rbac) = rbac else { return Ok(()) };
    let strategy = &obj["spec"]["strategy"];
    let Some(sub) = strategy_resource(strategy) else {
        return Err(ApiError::forbidden(&format!(
            "{resource}.{GROUP} {name:?} is forbidden: unrecognized build strategy"
        )));
    };
    let req = AuthorizationRequest {
        verb: "create".into(),
        resource: BUILDS.into(),
        subresource: Some(sub.into()),
        api_group: GROUP.into(),
        namespace: Some(namespace.into()),
        name: Some(name.into()),
    };
    if rbac.authorize(user, &req).await {
        return Ok(());
    }
    Err(ApiError::forbidden(&format!(
        "{resource}.{GROUP} {name:?} is forbidden: build strategy {} is not allowed",
        strategy_type(strategy)
    )))
}

// --- the build generator -----------------------------------------------------------

/// library-go's `naming.GetName`: `base-suffix`, shortened with an FNV hash
/// when longer than `max`.
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
        Some(base_len) => format!("{}-{}-{suffix}", &base[..base_len.min(base.len())], hash(base)),
    }
}

/// `labelValue`: the name, truncated until it is a valid label value.
fn label_value(name: &str) -> String {
    let re = regex::Regex::new(r"^(([A-Za-z0-9][-A-Za-z0-9_.]*)?[A-Za-z0-9])?$").expect("valid regex");
    let mut n = name.to_string();
    while !n.is_empty() {
        if n.len() <= 63 && re.is_match(&n) {
            return n;
        }
        n.pop();
    }
    name.to_string()
}

fn is_paused(bc: &Value) -> bool {
    s(&bc["metadata"]["annotations"], BUILD_CONFIG_PAUSED_ANNOTATION).eq_ignore_ascii_case("true")
}

/// The strategy's env, wherever the strategy keeps it (`buildutil.GetBuildEnv`).
fn env_path(spec: &Value) -> Option<&'static str> {
    ["sourceStrategy", "dockerStrategy", "customStrategy", "jenkinsPipelineStrategy"]
        .into_iter()
        .find(|k| spec["strategy"][*k].is_object())
}

/// `updateBuildEnv`: the request's variables replace those of the same name.
fn update_build_env(build: &mut Value, env: &[Value]) {
    let Some(k) = env_path(&build["spec"]) else { return };
    let names: Vec<&str> = env.iter().map(|e| s(e, "name")).collect();
    let mut out: Vec<Value> = build["spec"]["strategy"][k]["env"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| !names.contains(&s(e, "name")))
        .collect();
    out.extend(env.iter().cloned());
    build["spec"]["strategy"][k]["env"] = json!(out);
}

/// `updateBuildArgs`: the request's build args replace those of the same name.
fn update_build_args(old: &Value, new: &[Value]) -> Value {
    let mut combined: Vec<(String, Value)> = Vec::new();
    for e in old.as_array().into_iter().flatten().chain(new.iter()) {
        let n = s(e, "name").to_string();
        let v = e["value"].clone();
        match combined.iter_mut().find(|(k, _)| *k == n) {
            Some(slot) => slot.1 = v,
            None => combined.push((n, v)),
        }
    }
    json!(combined.into_iter().map(|(n, v)| json!({"name": n, "value": v})).collect::<Vec<_>>())
}

/// The request's strategy options and env on a generated build.
fn apply_request(build: &mut Value, req: &Value, bc_name: &str, ns: &str) -> Result<(), ApiError> {
    build["spec"]["triggeredBy"] = req["triggeredBy"].clone();
    if let Some(env) = req["env"].as_array().filter(|e| !e.is_empty()) {
        update_build_env(build, env);
    }
    let docker = &req["dockerStrategyOptions"];
    let not_docker = || {
        ApiError::bad_request(&format!(
            "Cannot specify Docker build specific options on {ns}/{bc_name}, not a Docker build."
        ))
    };
    if let Some(args) = docker["buildArgs"].as_array().filter(|a| !a.is_empty()) {
        if !build["spec"]["strategy"]["dockerStrategy"].is_object() {
            return Err(not_docker());
        }
        let merged = update_build_args(&build["spec"]["strategy"]["dockerStrategy"]["buildArgs"], args);
        build["spec"]["strategy"]["dockerStrategy"]["buildArgs"] = merged;
    }
    if let Some(nc) = docker["noCache"].as_bool() {
        if !build["spec"]["strategy"]["dockerStrategy"].is_object() {
            return Err(not_docker());
        }
        build["spec"]["strategy"]["dockerStrategy"]["noCache"] = json!(nc);
    }
    if let Some(inc) = req["sourceStrategyOptions"]["incremental"].as_bool() {
        if !build["spec"]["strategy"]["sourceStrategy"].is_object() {
            return Err(ApiError::bad_request(&format!(
                "Cannot specify Source build specific options on {ns}/{bc_name}, not a Source build."
            )));
        }
        build["spec"]["strategy"]["sourceStrategy"]["incremental"] = json!(inc);
    }
    Ok(())
}

fn unresolved(kind: &str, name: &str, ns: &str, resource: &str, missing: &str) -> ApiError {
    let msg = format!("Error resolving {kind} {name} in namespace {ns}: {resource} {missing:?} not found");
    ApiError {
        status: axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        reason: "Invalid".into(),
        message: msg.clone(),
        continue_token: None,
        details: Some(json!({"kind": kind, "name": name, "causes": [{"field": "from", "message": msg}]})),
    }
}

fn path_part_ok(s: &str) -> bool {
    !s.is_empty() && !s.contains(['/', '?', '#', '%'])
}

/// `resolveImageStreamReference`: a DockerImage is itself; an
/// ImageStreamTag is the tag's latest image (`ResolveLatestTaggedImage`) and
/// an ImageStreamImage the image's pull spec, read from the registry
/// through the aggregator (`get`, #386). A stream or tag that is not there
/// fails as OpenShift's does; a registry that cannot answer is a 500.
pub async fn resolve_image<F, Fut>(get: &F, from: &Value, default_ns: &str) -> Result<String, ApiError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    let (kind, name) = (s(from, "kind"), s(from, "name"));
    let ns = if s(from, "namespace").is_empty() { default_ns } else { s(from, "namespace") };
    let base = format!("/apis/image.openshift.io/v1/namespaces/{ns}");
    match kind {
        "DockerImage" => Ok(name.to_string()),
        "ImageStreamTag" => {
            let (stream, tag, _) = apimachinery::imagestream::split_tag(name);
            let missing = || unresolved(kind, name, ns, "imagestreamtags.image.openshift.io", name);
            if !path_part_ok(stream) || !path_part_ok(ns) {
                return Err(missing());
            }
            match get(format!("{base}/imagestreams/{stream}")).await {
                Fetched::Found(v) => apimachinery::imagestream::resolve_latest_tagged_image(&v, tag).ok_or_else(missing),
                Fetched::NotFound => Err(unresolved(kind, name, ns, "imagestreams.image.openshift.io", stream)),
                Fetched::Failed(e) => Err(ApiError::internal(&format!("error resolving ImageStreamTag {ns}/{name}: {e}"))),
            }
        }
        "ImageStreamImage" => {
            let missing = || unresolved(kind, name, ns, "imagestreamimages.image.openshift.io", name);
            if !path_part_ok(name) || !path_part_ok(ns) {
                return Err(missing());
            }
            match get(format!("{base}/imagestreamimages/{name}")).await {
                Fetched::Found(v) => v["image"]["dockerImageReference"].as_str().filter(|r| !r.is_empty())
                    .map(str::to_string).ok_or_else(missing),
                Fetched::NotFound => Err(missing()),
                Fetched::Failed(e) => Err(ApiError::internal(&format!("error resolving ImageStreamImage {ns}/{name}: {e}"))),
            }
        }
        other => Err(ApiError::bad_request(&format!("unable to locate any images in image repositories with kind {other:?}"))),
    }
}

/// The registry host of a pull spec, as a docker keyring keys it.
fn registry_of(image: &str) -> &str {
    match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => first,
        _ => "docker.io",
    }
}

/// `findDockerSecretAsReference`: the first of the builder account's
/// docker-config secrets with credentials for the image's registry.
fn find_docker_secret(secrets: &[Value], image: &str) -> Option<Value> {
    use base64::Engine;
    let host = registry_of(image);
    for secret in secrets {
        let (key, nested) = match s(secret, "type") {
            "kubernetes.io/dockerconfigjson" => (".dockerconfigjson", true),
            "kubernetes.io/dockercfg" => (".dockercfg", false),
            _ => continue,
        };
        let Some(raw) = secret["data"][key].as_str() else { continue };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(raw) else { continue };
        let Ok(cfg) = serde_json::from_slice::<Value>(&bytes) else { continue };
        let auths = if nested { &cfg["auths"] } else { &cfg };
        let matches = auths.as_object().into_iter().flatten().any(|(k, _)| {
            let k = k.trim_start_matches("https://").trim_start_matches("http://");
            let k = k.split('/').next().unwrap_or(k);
            k == host || (host == "docker.io" && (k == "index.docker.io" || k == "registry-1.docker.io"))
        });
        if matches {
            return Some(json!({"name": s(&secret["metadata"], "name")}));
        }
    }
    None
}

/// `fetchServiceAccountSecrets`: the secrets the build's service account
/// lists; the account itself must exist.
async fn service_account_secrets(storage: &ResourceStorage, ns: &str, sa: &str) -> Result<Vec<Value>, ApiError> {
    let account = storage.get(&ResourceStorage::namespaced_key("serviceaccounts", ns, sa)).await.map_err(|_| {
        ApiError::internal(&format!(
            "error getting push/pull secrets for service account {ns}/{sa}: serviceaccounts {sa:?} not found"
        ))
    })?;
    let mut out = Vec::new();
    for r in account["secrets"].as_array().into_iter().flatten() {
        if let Ok(secret) = storage.get(&ResourceStorage::namespaced_key("secrets", ns, s(r, "name"))).await {
            out.push(secret);
        }
    }
    Ok(out)
}

/// `setBaseImageAndPullSecretForBuildStrategy` and `setBuildSourceImage`:
/// every image reference resolved to a pull spec, pull secrets found. The
/// strategy's image is `strategy_image` when its default ImageChange
/// trigger has just recorded one, as upstream takes it from the trigger.
async fn resolve_images<F, Fut>(
    storage: &ResourceStorage,
    get: &F,
    spec: &mut Value,
    ns: &str,
    sa: &str,
    strategy_image: Option<String>,
) -> Result<(), ApiError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    let secrets = service_account_secrets(storage, ns, sa).await?;
    let images = spec["source"]["images"].as_array().cloned().unwrap_or_default();
    let mut resolved = Vec::new();
    for mut image in images {
        let spec_ref = resolve_image(get, &image["from"], ns).await?;
        if image["pullSecret"].is_null() {
            if let Some(found) = find_docker_secret(&secrets, &spec_ref) {
                image["pullSecret"] = found;
            }
        }
        image["from"] = json!({"kind": "DockerImage", "name": spec_ref});
        resolved.push(image);
    }
    if !resolved.is_empty() {
        spec["source"]["images"] = json!(resolved);
    }
    for k in ["sourceStrategy", "dockerStrategy", "customStrategy"] {
        let Some(st) = spec.get_mut("strategy").and_then(|v| v.get_mut(k)).filter(|v| v.is_object()) else {
            continue;
        };
        if k == "dockerStrategy" && !st["from"].is_object() {
            continue;
        }
        let image = match &strategy_image {
            Some(i) => i.clone(),
            None => resolve_image(get, &st["from"], ns).await?,
        };
        st["from"] = json!({"kind": "DockerImage", "name": image});
        if st["pullSecret"].is_null() {
            if let Some(found) = find_docker_secret(&secrets, &image) {
                st["pullSecret"] = found;
            }
        }
        if k == "customStrategy" {
            let mut env = st["env"].as_array().cloned().unwrap_or_default();
            match env.iter_mut().find(|e| e["name"] == CUSTOM_BASE_IMAGE_ENV) {
                Some(e) => e["value"] = json!(image),
                None => env.push(json!({"name": CUSTOM_BASE_IMAGE_ENV, "value": image})),
            }
            st["env"] = json!(env);
        }
    }
    Ok(())
}

/// The effective `from` of an ImageChange trigger, its namespace filled.
fn trigger_from(bc: &Value, trigger: &Value) -> Option<Value> {
    let own = &trigger["imageChange"]["from"];
    let from = if own.is_object() { own } else { strategy_from(&bc["spec"]) };
    if !from.is_object() {
        return None;
    }
    let mut from = from.clone();
    if s(&from, "namespace").is_empty() {
        from["namespace"] = json!(s(&bc["metadata"], "namespace"));
    }
    Some(from)
}

/// `updateImageTriggers`: every ImageChange trigger's image recorded — the
/// request's image for the trigger that asked (refused when it is the one
/// already built), the stream's latest for the others. A default trigger's
/// stream that cannot be resolved fails the request; another's is recorded
/// empty, as upstream's. Returns the default trigger's image: the build's
/// strategy image.
async fn update_image_triggers<F, Fut>(get: &F, bc: &mut Value, req: &Value) -> Result<Option<String>, ApiError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    let (ns, name) = (s(&bc["metadata"], "namespace").to_string(), s(&bc["metadata"], "name").to_string());
    let by_image = s(&req["triggeredByImage"], "name").to_string();
    let asked = req["from"].is_object().then(|| {
        let mut f = req["from"].clone();
        if s(&f, "namespace").is_empty() {
            f["namespace"] = json!(ns);
        }
        f
    });
    let n = bc["spec"]["triggers"].as_array().map_or(0, Vec::len);
    let previous = bc["status"]["imageChangeTriggers"].as_array().cloned().unwrap_or_default();
    let mut statuses = Vec::new();
    let mut strategy_image = None;
    for i in 0..n {
        let t = bc["spec"]["triggers"][i].clone();
        if t["type"] != "ImageChange" {
            continue;
        }
        let Some(from) = trigger_from(bc, &t) else { continue };
        let default = !t["imageChange"]["from"].is_object();
        let is_asked = asked.as_ref().is_some_and(|a| a["name"] == from["name"] && a["namespace"] == from["namespace"]);
        let status_from = json!({"name": from["name"], "namespace": from["namespace"]});
        let old = previous.iter().find(|p| p["from"]["name"] == status_from["name"]
            && (s(&p["from"], "namespace").is_empty() || p["from"]["namespace"] == status_from["namespace"]));
        let last_before = old.map(|o| s(o, "lastTriggeredImageID")).filter(|l| !l.is_empty())
            .unwrap_or_else(|| s(&t["imageChange"], "lastTriggeredImageID")).to_string();
        let (image, at) = if is_asked && !by_image.is_empty() {
            if last_before == by_image {
                return Err(ApiError::bad_request(&format!(
                    "build config {ns}/{name} has already instantiated a build for imageid {by_image}"
                )));
            }
            (by_image.clone(), Some(now()))
        } else {
            match resolve_image(get, &from, &ns).await {
                Ok(i) => (i, None),
                Err(e) if default => return Err(e),
                Err(e) => {
                    tracing::warn!(namespace = %ns, buildconfig = %name, "could not resolve an ImageChange trigger's reference: {}", e.message);
                    (String::new(), None)
                }
            }
        };
        bc["spec"]["triggers"][i]["imageChange"]["lastTriggeredImageID"] = json!(image);
        let mut st = json!({"lastTriggeredImageID": image, "from": status_from});
        if let Some(t) = at.or_else(|| old.and_then(|o| o["lastTriggerTime"].as_str().map(String::from))) {
            st["lastTriggerTime"] = json!(t);
        }
        statuses.push(st);
        if default && !image.is_empty() {
            strategy_image = Some(image);
        }
    }
    if !statuses.is_empty() {
        bc["status"]["imageChangeTriggers"] = json!(statuses);
    }
    Ok(strategy_image)
}

/// A BuildConfig's generation moves on with its `lastVersion`, as its
/// strategy's `PrepareForUpdate` does.
fn bump_generation(bc: &mut Value) {
    let g = bc["metadata"]["generation"].as_i64().unwrap_or(1);
    bc["metadata"]["generation"] = json!(g + 1);
}

/// Admission of a write through `builds` or `buildconfigs`: the registry
/// strategy, then the strategy restriction for the requester (a request's;
/// none for the apiserver's own writes). `old` is the stored object of an
/// update. Upstream skips the restriction for an update that changes only
/// what the garbage collector writes; an update leaving the strategy as it
/// was is that here too.
pub async fn admit(resource: &str, namespace: &str, obj: &mut Value, old: Option<&Value>) -> Result<(), ApiError> {
    match (resource, old) {
        (BUILDS, None) => admit_build_create(obj)?,
        (BUILDS, Some(old)) => admit_build_update(obj, old)?,
        (BUILD_CONFIGS, None) => admit_config_create(obj)?,
        (BUILD_CONFIGS, Some(old)) => admit_config_update(obj, old)?,
        _ => return Ok(()),
    }
    if old.is_some_and(|o| o["spec"]["strategy"] == obj["spec"]["strategy"]) {
        return Ok(());
    }
    let Some(req) = crate::admission::current() else { return Ok(()) };
    let name = match obj["metadata"]["name"].as_str().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => s(&obj["metadata"], "generateName").to_string(),
    };
    authorize_strategy(req.rbac.as_deref(), &req.user, namespace, &name, resource, obj).await
}

fn new_status(config: Option<(&str, &str)>) -> Value {
    let t = now();
    let mut status = json!({
        "phase": "New",
        "conditions": [{"type": "New", "status": "True", "lastUpdateTime": t, "lastTransitionTime": t}],
    });
    if let Some((name, ns)) = config {
        status["config"] = json!({"kind": "BuildConfig", "name": name, "namespace": ns});
    }
    status
}

/// Store a generated build: the metadata a POST would give it, then create.
async fn create_build(storage: &ResourceStorage, ns: &str, mut build: Value) -> Result<Value, ApiError> {
    build["apiVersion"] = json!(API_VERSION);
    build["kind"] = json!("Build");
    let name = s(&build["metadata"], "name").to_string();
    crate::handlers::resource::ensure_metadata_pub(&mut build, &name, Some(ns));
    build["metadata"]["generation"] = json!(1);
    validate_build(&build)?;
    storage.create(&ResourceStorage::namespaced_key(BUILDS, ns, &name), build).await
}

/// `buildconfigs/instantiate`: the BuildConfig's next build, its
/// `lastVersion` bumped by a conditional update (a conflict is retried, as
/// upstream retries it), then the build created. `rbac`/`user` are the
/// requester's, for the strategy restriction.
pub async fn instantiate<F, Fut>(
    storage: &ResourceStorage,
    get: &F,
    rbac: Option<&RbacEngine>,
    user: &UserInfo,
    ns: &str,
    name: &str,
    req: &Value,
) -> Result<Value, ApiError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    if !req["binary"].is_null() {
        return Err(ApiError::bad_request(
            "a BuildRequest with binary input must use buildconfigs/instantiatebinary, which this cluster does not serve",
        ));
    }
    let key = ResourceStorage::namespaced_key(BUILD_CONFIGS, ns, name);
    for _ in 0..3 {
        let mut bc = storage.get(&key).await.map_err(|e| {
            if e.status == axum::http::StatusCode::NOT_FOUND {
                ApiError::not_found("buildconfigs.build.openshift.io", name)
            } else {
                e
            }
        })?;
        authorize_strategy(rbac, user, ns, name, BUILD_CONFIGS, &bc).await?;
        if is_paused(&bc) {
            return Err(ApiError::bad_request(&format!(
                "can't instantiate from BuildConfig {ns}/{name}: BuildConfig is paused"
            )));
        }
        let last = bc["status"]["lastVersion"].as_i64().unwrap_or(0);
        if let Some(want) = req["lastVersion"].as_i64() {
            if want != last {
                return Err(ApiError::bad_request(&format!(
                    "the LastVersion({last}) on build config {ns}/{name} does not match the build request LastVersion({want})"
                )));
            }
        }
        let strategy_image = update_image_triggers(get, &mut bc, req).await?;
        let version = last + 1;
        let build_name = get_name(name, &version.to_string(), 253);
        let sa = match s(&bc["spec"], "serviceAccount") {
            "" => BUILDER_SERVICE_ACCOUNT.to_string(),
            other => other.to_string(),
        };
        let mut spec = json!({});
        for k in ["source", "strategy", "output", "resources", "postCommit", "completionDeadlineSeconds",
                  "nodeSelector", "mountTrustedCA"] {
            if !bc["spec"][k].is_null() {
                spec[k] = bc["spec"][k].clone();
            }
        }
        spec["serviceAccount"] = json!(sa);
        if !req["revision"].is_null() {
            spec["revision"] = req["revision"].clone();
        }
        // setBuildSource: not a binary build.
        if let Some(src) = spec["source"].as_object_mut() {
            src.remove("binary");
        }
        resolve_images(storage, get, &mut spec, ns, &sa, strategy_image).await?;
        let mut labels: Map<String, Value> = bc["metadata"]["labels"].as_object().cloned().unwrap_or_default();
        labels.insert(BUILD_CONFIG_LABEL_DEPRECATED.into(), json!(label_value(name)));
        labels.insert(BUILD_CONFIG_LABEL.into(), json!(label_value(name)));
        labels.insert(BUILD_RUN_POLICY_LABEL.into(), json!(s(&bc["spec"], "runPolicy")));
        let mut annotations = Map::new();
        annotations.insert(BUILD_NUMBER_ANNOTATION.into(), json!(version.to_string()));
        annotations.insert(BUILD_CONFIG_ANNOTATION.into(), json!(name));
        // The request's labels and annotations, under the system ones.
        let merge = |into: &mut Map<String, Value>, from: &Value| {
            for (k, v) in from.as_object().into_iter().flatten() {
                into.entry(k.clone()).or_insert_with(|| v.clone());
            }
        };
        merge(&mut labels, &req["metadata"]["labels"]);
        merge(&mut annotations, &req["metadata"]["annotations"]);
        let mut build = json!({
            "metadata": {
                "name": build_name,
                "namespace": ns,
                "labels": labels,
                "annotations": annotations,
                "ownerReferences": [{"apiVersion": API_VERSION, "kind": "BuildConfig", "name": name,
                                     "uid": bc["metadata"]["uid"], "controller": true}],
            },
            "spec": spec,
            "status": new_status(Some((name, ns))),
        });
        normalize_build(&mut build);
        apply_request(&mut build, req, name, ns)?;
        // lastVersion first, conditional on what was read: two instantiations
        // at once cannot both take a version (upstream's order, for the same
        // reason).
        bc["status"]["lastVersion"] = json!(version);
        bump_generation(&mut bc);
        let rev = bc["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
        match storage.update(&key, bc, rev).await {
            Ok(_) => return create_build(storage, ns, build).await,
            Err(e) if e.reason == "Conflict" => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::conflict(&format!(
        "Operation cannot be fulfilled on buildconfigs.{GROUP} {name:?}: the object has been modified; please apply your changes to the latest version and try again"
    )))
}

// --- webhooks ---------------------------------------------------------------------

/// `webhook.DefaultConfigRef`: the branch a config without a ref builds.
const DEFAULT_CONFIG_REF: &str = "master";

/// `GitRefMatches`: the event's branch is the config's (`master` when it
/// names none), `refs/heads/` ignored on both.
pub fn git_ref_matches(event_ref: &str, source: &Value) -> bool {
    let config = match s(&source["git"], "ref") {
        "" => DEFAULT_CONFIG_REF,
        r => r,
    };
    event_ref.trim_start_matches("refs/heads/") == config.trim_start_matches("refs/heads/")
}

/// What a webhook delivery asks for.
#[derive(Debug, PartialEq)]
pub enum HookCall {
    /// A build, with this request's revision, env and Docker options.
    Build { revision: Value, env: Value, docker: Value },
    /// Nothing to build (a ping, another branch): answered 200.
    Skip(String),
}

fn header<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn json_body(headers: &axum::http::HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    let ct = header(headers, "content-type");
    let media = ct.split(';').next().unwrap_or("").trim();
    if media != "application/json" {
        return Err(ApiError::bad_request(&format!("unsupported Content-Type {ct}")));
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(&format!("error decoding the webhook payload: {e}")))
}

fn person(p: &Value) -> Value {
    json!({"name": s(p, "name"), "email": s(p, "email")})
}

fn git_revision(commit: &str, author: Value, committer: Value, message: &str) -> Value {
    json!({"type": "Git", "git": {"commit": commit, "author": author, "committer": committer, "message": message}})
}

/// `Extract` of each webhook plugin: GitHub (and Gogs) `push`, GitLab `Push
/// Hook`, Bitbucket `repo:push`, or the generic payload (optional; `env`
/// only when the trigger has `allowEnv`).
pub fn extract_hook(
    ty: &str,
    trigger: &Value,
    source: &Value,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<HookCall, ApiError> {
    let other_branch = |r: &str| HookCall::Skip(format!("skipping build: branch reference {r:?} does not match the configuration"));
    match ty {
        "GitHub" => {
            let event = match header(headers, "x-github-event") {
                "" => header(headers, "x-gogs-event"),
                e => e,
            };
            match event {
                "" => return Err(ApiError::bad_request("missing X-GitHub-Event or X-Gogs-Event")),
                "ping" => return Ok(HookCall::Skip("ping".into())),
                "push" => {}
                _ => return Err(ApiError::bad_request("Unknown X-GitHub-Event or X-Gogs-Event")),
            }
            let ev = json_body(headers, body)?;
            let r = s(&ev, "ref");
            if !git_ref_matches(r, source) {
                return Ok(other_branch(r));
            }
            let c = &ev["head_commit"];
            let revision = git_revision(s(c, "id"), person(&c["author"]), person(&c["committer"]), s(c, "message"));
            Ok(HookCall::Build { revision, env: Value::Null, docker: Value::Null })
        }
        "GitLab" => {
            match header(headers, "x-gitlab-event") {
                "" => return Err(ApiError::bad_request("missing X-Gitlab-Event")),
                "Push Hook" => {}
                _ => return Err(ApiError::bad_request("Unknown X-Gitlab-Event")),
            }
            let ev = json_body(headers, body)?;
            let r = s(&ev, "ref");
            if !git_ref_matches(r, source) {
                return Ok(other_branch(r));
            }
            let last = ev["commits"].as_array().and_then(|c| c.last()).cloned().unwrap_or(Value::Null);
            let commit = match s(&last, "id") {
                "" => s(&ev, "checkout_sha"),
                id => id,
            };
            let author = person(&last["author"]);
            let revision = git_revision(commit, author.clone(), author, s(&last, "message"));
            Ok(HookCall::Build { revision, env: Value::Null, docker: Value::Null })
        }
        "Bitbucket" => {
            match header(headers, "x-event-key") {
                "" => return Err(ApiError::bad_request("missing X-Event-Key")),
                "repo:push" => {}
                _ => return Err(ApiError::bad_request("Unknown X-Event-Key")),
            }
            let ev = json_body(headers, body)?;
            let change = ev["push"]["changes"].as_array().and_then(|c| c.iter().rev().find(|c| c["new"]["type"] == "branch"));
            let Some(new) = change.map(|c| &c["new"]) else {
                return Ok(HookCall::Skip("no branch was pushed".into()));
            };
            let r = format!("refs/heads/{}", s(new, "name"));
            if !git_ref_matches(&r, source) {
                return Ok(other_branch(&r));
            }
            let target = &new["target"];
            // "Name <email>", as Bitbucket gives an author.
            let raw = s(&target["author"], "raw");
            let (name, email) = match raw.split_once('<') {
                Some((n, e)) => (n.trim().to_string(), e.trim_end_matches('>').trim().to_string()),
                None => (raw.to_string(), String::new()),
            };
            let author = json!({"name": name, "email": email});
            let revision = git_revision(s(target, "hash"), author.clone(), author, s(target, "message"));
            Ok(HookCall::Build { revision, env: Value::Null, docker: Value::Null })
        }
        "Generic" => {
            if body.iter().all(u8::is_ascii_whitespace) {
                return Ok(HookCall::Build { revision: Value::Null, env: Value::Null, docker: Value::Null });
            }
            let ct = header(headers, "content-type");
            let media = ct.split(';').next().unwrap_or("").trim();
            let ev: Value = match media {
                "application/json" => serde_json::from_slice(body)
                    .map_err(|e| ApiError::bad_request(&format!("error decoding the webhook payload: {e}")))?,
                "application/yaml" | "application/x-yaml" | "text/yaml" => serde_yaml::from_slice(body)
                    .map_err(|e| ApiError::bad_request(&format!("error decoding the webhook payload: {e}")))?,
                _ => return Err(ApiError::bad_request(&format!("unsupported Content-Type {ct}"))),
            };
            let mut revision = Value::Null;
            let git = &ev["git"];
            if git.is_object() {
                let r = s(git, "ref");
                if !r.is_empty() && !git_ref_matches(r, source) {
                    return Ok(other_branch(r));
                }
                revision = git_revision(s(git, "commit"), person(&git["author"]), person(&git["committer"]), s(git, "message"));
            }
            let env = if trigger["allowEnv"] == true { ev["env"].clone() } else { Value::Null };
            Ok(HookCall::Build { revision, env, docker: ev["dockerStrategyOptions"].clone() })
        }
        other => Err(ApiError::not_found("buildconfighook", other)),
    }
}

/// The secret a webhook trigger holds: its own, or its Secret's
/// `WebHookSecretKey`.
async fn trigger_secret(storage: &ResourceStorage, ns: &str, hook: &Value) -> Option<String> {
    use base64::Engine;
    if let Some(own) = hook["secret"].as_str().filter(|s| !s.is_empty()) {
        return Some(own.to_string());
    }
    let name = s(&hook["secretReference"], "name");
    if name.is_empty() {
        return None;
    }
    let secret = storage.get(&ResourceStorage::namespaced_key("secrets", ns, name)).await.ok()?;
    let raw = secret["data"][WEBHOOK_SECRET_KEY].as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    String::from_utf8(bytes).ok()
}

/// `POST buildconfigs/<name>/webhooks/<secret>/<type>`: a build of the
/// config, when a trigger of that type holds the secret and the delivery
/// asks for one; `Ok(None)` when it asks for nothing (200, no build). The
/// build is instantiated as the apiserver's (upstream's webhook REST uses
/// its loopback client): the caller needs only `create
/// buildconfigs/webhooks`, which `system:webhook` grants.
#[allow(clippy::too_many_arguments)]
pub async fn webhook<F, Fut>(
    storage: &ResourceStorage,
    get: &F,
    ns: &str,
    name: &str,
    secret: &str,
    hook_type: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Option<Value>, ApiError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Fetched>,
{
    let ty = match hook_type {
        "github" => "GitHub",
        "generic" => "Generic",
        "gitlab" => "GitLab",
        "bitbucket" => "Bitbucket",
        other => return Err(ApiError::not_found("buildconfighook", other)),
    };
    let field = webhook_field(ty).unwrap_or_default();
    let bc = storage.get(&ResourceStorage::namespaced_key(BUILD_CONFIGS, ns, name)).await.map_err(|e| {
        if e.status == axum::http::StatusCode::NOT_FOUND { ApiError::not_found("buildconfigs.build.openshift.io", name) } else { e }
    })?;
    let mut matched = None;
    for t in bc["spec"]["triggers"].as_array().into_iter().flatten().filter(|t| t["type"] == ty) {
        if trigger_secret(storage, ns, &t[field]).await.is_some_and(|s| !s.is_empty() && s == secret) {
            matched = Some(t[field].clone());
            break;
        }
    }
    let Some(trigger) = matched else {
        return Err(ApiError::unauthorized(&format!("the webhook {hook_type:?} for {name:?} did not accept your secret")));
    };
    let (revision, env, docker) = match extract_hook(ty, &trigger, &bc["spec"]["source"], headers, body)? {
        HookCall::Skip(why) => {
            tracing::info!(namespace = ns, buildconfig = name, "{ty} webhook: {why}");
            return Ok(None);
        }
        HookCall::Build { revision, env, docker } => (revision, env, docker),
    };
    let cause_field = format!("{field}WebHook");
    let mut cause = json!({"message": format!("{ty} WebHook")});
    cause[cause_field] = json!({"revision": revision, "secret": "<secret>"});
    let mut req = json!({"kind": "BuildRequest", "apiVersion": API_VERSION, "metadata": {"name": name, "namespace": ns},
                         "triggeredBy": [cause]});
    if !revision.is_null() {
        req["revision"] = revision;
    }
    if env.as_array().is_some_and(|e| !e.is_empty()) {
        req["env"] = env;
    }
    if docker.is_object() {
        req["dockerStrategyOptions"] = docker;
    }
    let apiserver = UserInfo { username: "system:apiserver".into(), groups: vec!["system:masters".into()] };
    instantiate(storage, get, None, &apiserver, ns, name, &req).await.map(Some)
}

/// `system:webhook`: who may deliver a build webhook. Bound to
/// `system:authenticated` (`system:webhooks`), as OpenShift 4.16 binds it;
/// a cluster that takes deliveries from an outside service binds it to
/// `system:unauthenticated` too.
pub fn webhook_role() -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {"name": "system:webhook",
                     "annotations": {"rustkube.io/description": "Deliver BuildConfig webhooks (#386)."}},
        "rules": [{"apiGroups": ["", GROUP], "resources": ["buildconfigs/webhooks"], "verbs": ["create", "get"]}],
    })
}

pub fn webhooks_binding() -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {"name": "system:webhooks"},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "system:webhook"},
        "subjects": [{"apiGroup": "rbac.authorization.k8s.io", "kind": "Group", "name": "system:authenticated"}],
    })
}

/// `builds/clone` (`oc start-build --from-build`): a new build of the same
/// spec — the BuildConfig's next version when it has one, else the name with
/// a time suffix.
pub async fn clone(
    storage: &ResourceStorage,
    rbac: Option<&RbacEngine>,
    user: &UserInfo,
    ns: &str,
    name: &str,
    req: &Value,
) -> Result<Value, ApiError> {
    let old = storage.get(&ResourceStorage::namespaced_key(BUILDS, ns, name)).await.map_err(|e| {
        if e.status == axum::http::StatusCode::NOT_FOUND {
            ApiError::not_found("builds.build.openshift.io", name)
        } else {
            e
        }
    })?;
    authorize_strategy(rbac, user, ns, name, BUILDS, &old).await?;
    let config_name = s(&old["status"]["config"], "name").to_string();
    // A hand-made build's clone carries a time suffix; a clone of a clone
    // replaces it.
    let suffixed = regex::Regex::new(r"^.+-\d{10}$").expect("valid regex");
    for _ in 0..3 {
        let config = if config_name.is_empty() {
            None
        } else {
            match storage.get(&ResourceStorage::namespaced_key(BUILD_CONFIGS, ns, &config_name)).await {
                Ok(bc) => Some(bc),
                Err(e) if e.status == axum::http::StatusCode::NOT_FOUND => None,
                Err(e) => return Err(e),
            }
        };
        if let Some(bc) = &config {
            if is_paused(bc) {
                return Err(ApiError::internal(&format!(
                    "fatal error generating Build from BuildConfig: can't instantiate from BuildConfig {ns}/{config_name}: BuildConfig is paused"
                )));
            }
        }
        let mut annotations = old["metadata"]["annotations"].as_object().cloned().unwrap_or_default();
        for k in [BUILD_POD_NAME_ANNOTATION, "openshift.io/jenkins-status-json", "openshift.io/jenkins-log-url",
                  "openshift.io/jenkins-console-log-url", "openshift.io/jenkins-blueocean-log-url",
                  "openshift.io/jenkins-build-uri"] {
            annotations.remove(k);
        }
        annotations.insert(BUILD_CLONE_ANNOTATION.into(), json!(name));
        let new_name = match &config {
            Some(bc) => {
                let v = bc["status"]["lastVersion"].as_i64().unwrap_or(0) + 1;
                annotations.insert(BUILD_NUMBER_ANNOTATION.into(), json!(v.to_string()));
                get_name(&config_name, &v.to_string(), 253)
            }
            None => {
                annotations.remove(BUILD_NUMBER_ANNOTATION);
                let base = if suffixed.is_match(name) { name.rsplit_once('-').map_or(name, |(b, _)| b) } else { name };
                let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default().to_string();
                let suffix = &nanos[nanos.len().saturating_sub(10)..];
                get_name(base, suffix, 253)
            }
        };
        let mut spec = old["spec"].clone();
        if let Some(src) = spec["source"].as_object_mut() {
            src.remove("binary");
        }
        let mut build = json!({
            "metadata": {
                "name": new_name,
                "namespace": ns,
                "labels": old["metadata"]["labels"],
                "annotations": annotations,
                "ownerReferences": old["metadata"]["ownerReferences"],
            },
            "spec": spec,
            "status": new_status(None),
        });
        build["status"]["config"] = old["status"]["config"].clone();
        normalize_build(&mut build);
        build["spec"]["triggeredBy"] = req["triggeredBy"].clone();
        if let Some(env) = req["env"].as_array().filter(|e| !e.is_empty()) {
            update_build_env(&mut build, env);
        }
        if let Some(args) = req["dockerStrategyOptions"]["buildArgs"].as_array().filter(|a| !a.is_empty()) {
            if !build["spec"]["strategy"]["dockerStrategy"].is_object() {
                return Err(ApiError::bad_request(&format!(
                    "Cannot specify build args on {ns}/{config_name}, not a Docker build."
                )));
            }
            let merged = update_build_args(&build["spec"]["strategy"]["dockerStrategy"]["buildArgs"], args);
            build["spec"]["strategy"]["dockerStrategy"]["buildArgs"] = merged;
        }
        match config {
            None => return create_build(storage, ns, build).await,
            Some(mut bc) => {
                let v = bc["status"]["lastVersion"].as_i64().unwrap_or(0) + 1;
                bc["status"]["lastVersion"] = json!(v);
                bump_generation(&mut bc);
                let rev = bc["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
                let key = ResourceStorage::namespaced_key(BUILD_CONFIGS, ns, &config_name);
                match storage.update(&key, bc, rev).await {
                    Ok(_) => return create_build(storage, ns, build).await,
                    Err(e) if e.reason == "Conflict" => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Err(ApiError::conflict(&format!(
        "Operation cannot be fulfilled on buildconfigs.{GROUP} {config_name:?}: the object has been modified; please apply your changes to the latest version and try again"
    )))
}

// --- the builder's own access -----------------------------------------------------

/// OpenShift's `system:image-builder` ClusterRole: what a build pod's
/// account does — record what it built (`builds/details`) and push to the
/// project's registry (`imagestreams/layers`, which the node registry asks
/// about by SubjectAccessReview, #325).
pub const IMAGE_BUILDER_ROLE: &str = "system:image-builder";
/// The RoleBinding OpenShift puts in every namespace for its `builder`.
pub const IMAGE_BUILDERS_BINDING: &str = "system:image-builders";

pub fn image_builder_role() -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {"name": IMAGE_BUILDER_ROLE,
                     "annotations": {"openshift.io/description": "Grants the right to build, push and pull images from within a project. Used primarily with service accounts for builds."}},
        "rules": [
            {"apiGroups": ["image.openshift.io", ""], "resources": ["imagestreams/layers"], "verbs": ["get", "update"]},
            {"apiGroups": ["image.openshift.io", ""], "resources": ["imagestreams"], "verbs": ["create"]},
            {"apiGroups": [GROUP], "resources": ["builds/details"], "verbs": ["update"]},
            {"apiGroups": [GROUP], "resources": ["builds"], "verbs": ["get"]},
        ],
    })
}

pub fn image_builders_binding(namespace: &str) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {"name": IMAGE_BUILDERS_BINDING, "namespace": namespace,
                     "annotations": {"openshift.io/description": "Allows builds in this namespace to push images to this namespace. Configured by the apiserver (#374)."}},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": IMAGE_BUILDER_ROLE},
        "subjects": [{"kind": "ServiceAccount", "name": BUILDER_SERVICE_ACCOUNT, "namespace": namespace}],
    })
}

/// The pod a build runs in: `<build>-build`, shortened as `GetBuildPodName`.
pub fn build_pod_name(build_name: &str) -> String {
    get_name(build_name, "build", 63)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_bc() -> Value {
        json!({"apiVersion": API_VERSION, "kind": "BuildConfig",
               "metadata": {"name": "web", "namespace": "demo"},
               "spec": {"source": {"git": {"uri": "https://github.com/sclorg/nodejs-ex"}},
                        "strategy": {"dockerStrategy": {}},
                        "output": {"to": {"kind": "DockerImage", "name": "registry.local:5100/demo/web:latest"}},
                        "triggers": [{"type": "ConfigChange"}]}})
    }

    #[test]
    fn defaults_and_types_as_v1_writes_them() {
        let mut bc = docker_bc();
        bc["spec"]["triggers"] = json!([{"type": "ConfigChange"}, {"type": "Nonsense"}]);
        admit_config_create(&mut bc).unwrap();
        assert_eq!(bc["spec"]["runPolicy"], "Serial");
        assert_eq!(bc["spec"]["strategy"]["type"], "Docker");
        assert_eq!(bc["spec"]["source"]["type"], "Git");
        assert_eq!(bc["spec"]["triggers"], json!([{"type": "ConfigChange"}]), "an unknown trigger is dropped");
        assert_eq!(bc["status"]["lastVersion"], 0);
        assert_eq!(bc["metadata"]["generation"], 1);
        assert_eq!((bc["spec"]["successfulBuildsHistoryLimit"].clone(), bc["spec"]["failedBuildsHistoryLimit"].clone()),
                   (json!(5), json!(5)), "the group API's history limits");
        // A Docker type alone, and a Source strategy's from kind.
        let mut b = json!({"metadata": {"name": "b"}, "spec": {"source": {"dockerfile": "FROM x"},
                           "strategy": {"type": "Docker"}}});
        admit_build_create(&mut b).unwrap();
        assert_eq!(b["spec"]["strategy"]["dockerStrategy"], json!({}));
        assert_eq!(b["spec"]["source"]["type"], "Dockerfile");
        assert_eq!(b["status"]["phase"], "New");
        assert_eq!(b["status"]["conditions"][0]["type"], "New");
        let mut spec = json!({"strategy": {"sourceStrategy": {"from": {"name": "ruby:3"}}},
                              "output": {"to": {"kind": "ImageStreamTag", "name": "app"}}});
        normalize_common(&mut spec);
        assert_eq!(spec["strategy"]["sourceStrategy"]["from"]["kind"], "ImageStreamTag");
        assert_eq!(spec["output"]["to"]["name"], "app:latest");
    }

    #[test]
    fn what_this_cluster_cannot_run_is_refused() {
        let cases: Vec<(&str, Value)> = vec![
            ("spec.strategy.type: Unsupported value: \"JenkinsPipeline\"",
             json!({"strategy": {"jenkinsPipelineStrategy": {"jenkinsfile": "node {}"}}})),
            ("spec.source.type: Unsupported value: \"Binary\"",
             json!({"source": {"binary": {}}, "strategy": {"dockerStrategy": {}}})),
        ];
        for (want, spec) in cases {
            let mut bc = json!({"metadata": {"name": "x"}, "spec": spec});
            let e = admit_config_create(&mut bc).unwrap_err();
            assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
            assert!(e.message.contains(want), "{}", e.message);
        }
        let mut bc = docker_bc();
        bc["spec"]["triggers"] = json!([{"type": "Nonsense"}]);
        normalize_config(&mut bc);
        assert_eq!(bc["spec"]["triggers"], json!([]), "unknown types are dropped before validation");
    }

    #[test]
    fn upstream_validation() {
        type Change = Box<dyn Fn(&mut Value)>;
        let refused: Vec<(&str, Change)> = vec![
            ("spec.source: Invalid value: \"\": must provide a value for at least one source input",
             Box::new(|bc| bc["spec"]["source"] = json!({}))),
            ("spec.strategy: Invalid value: {}: must provide a value for exactly one of",
             Box::new(|bc| bc["spec"]["strategy"]["sourceStrategy"] = json!({"from": {"kind": "DockerImage", "name": "x"}}))),
            ("spec.output.to.kind: Invalid value: \"Other\"",
             Box::new(|bc| bc["spec"]["output"]["to"]["kind"] = json!("Other"))),
            ("spec.output.to.name: Invalid value: \"Not Valid\"",
             Box::new(|bc| bc["spec"]["output"]["to"]["name"] = json!("Not Valid"))),
            ("spec.runPolicy: Invalid value: \"Sometimes\"",
             Box::new(|bc| bc["spec"]["runPolicy"] = json!("Sometimes"))),
            ("spec.successfulBuildsHistoryLimit: Invalid value: -1",
             Box::new(|bc| bc["spec"]["successfulBuildsHistoryLimit"] = json!(-1))),
            ("spec.completionDeadlineSeconds: Invalid value: 0",
             Box::new(|bc| bc["spec"]["completionDeadlineSeconds"] = json!(0))),
            ("spec.postCommit: Invalid value: {}: cannot use command and script together",
             Box::new(|bc| bc["spec"]["postCommit"] = json!({"script": "x", "command": ["y"]}))),
            ("spec.source.git.uri: Required value",
             Box::new(|bc| bc["spec"]["source"]["git"]["uri"] = json!(""))),
            ("spec.source.contextDir: Invalid value: \"../up\"",
             Box::new(|bc| bc["spec"]["source"]["contextDir"] = json!("../up"))),
            ("spec.strategy.dockerStrategy.dockerfilePath: Invalid value: \"/etc/Dockerfile\"",
             Box::new(|bc| bc["spec"]["strategy"]["dockerStrategy"]["dockerfilePath"] = json!("/etc/Dockerfile"))),
            ("spec.strategy.dockerStrategy.env[0].name: Invalid value: \"1BAD\"",
             Box::new(|bc| bc["spec"]["strategy"]["dockerStrategy"]["env"] = json!([{"name": "1BAD", "value": "x"}]))),
            ("spec.source.sourceSecret.name: Required value",
             Box::new(|bc| bc["spec"]["source"]["sourceSecret"] = json!({}))),
            ("spec.strategy.dockerStrategy.from.name: Invalid value: \"ruby\": must be <name>:<tag>",
             Box::new(|bc| bc["spec"]["strategy"]["dockerStrategy"]["from"] = json!({"kind": "ImageStreamTag", "name": "ruby"}))),
            ("metadata.name: Invalid value: \"Web\"",
             Box::new(|bc| bc["metadata"]["name"] = json!("Web"))),
        ];
        for (want, change) in refused {
            let mut bc = docker_bc();
            change(&mut bc);
            let e = admit_config_create(&mut bc).unwrap_err();
            assert!(e.message.contains(want), "want {want:?} in {}", e.message);
            assert!(e.message.starts_with("BuildConfig.build.openshift.io"), "{}", e.message);
        }
        let mut ok = docker_bc();
        ok["spec"]["postCommit"] = json!({"script": "exit 1"});
        ok["spec"]["source"]["sourceSecret"] = json!({"name": "gitcreds"});
        ok["spec"]["strategy"]["dockerStrategy"]["pullSecret"] = json!({"name": "pull"});
        ok["spec"]["output"]["pushSecret"] = json!({"name": "push"});
        admit_config_create(&mut ok).unwrap();
    }

    #[test]
    fn a_build_spec_is_immutable_and_a_finished_phase_stays() {
        let mut old = json!({"metadata": {"name": "web-1", "generation": 1},
                             "spec": {"source": {"dockerfile": "FROM x"}, "strategy": {"dockerStrategy": {}}},
                             "status": {"phase": "Running"}});
        normalize_build(&mut old);
        let mut cancel = old.clone();
        cancel["status"]["phase"] = json!("Cancelled");
        cancel["status"]["cancelled"] = json!(true);
        admit_build_update(&mut cancel, &old).unwrap();
        assert_eq!(cancel["status"]["conditions"][0]["type"], "Cancelled");
        let mut push_secret = old.clone();
        push_secret["spec"]["output"]["pushSecret"] = json!({"name": "late"});
        admit_build_update(&mut push_secret, &old).unwrap();
        let mut changed = old.clone();
        changed["spec"]["source"]["dockerfile"] = json!("FROM y");
        let e = admit_build_update(&mut changed, &old).unwrap_err();
        assert!(e.message.contains("spec is immutable"), "{}", e.message);
        let mut done = old.clone();
        done["status"]["phase"] = json!("Complete");
        let mut back = done.clone();
        back["status"]["phase"] = json!("Running");
        let e = admit_build_update(&mut back, &done).unwrap_err();
        assert!(e.message.contains("phase cannot be updated from a terminal state"), "{}", e.message);
    }

    #[test]
    fn conditions_follow_the_phase() {
        let mut b = json!({"status": {"phase": "New"}});
        manage_conditions(&mut b);
        b["status"]["phase"] = json!("Failed");
        b["status"]["reason"] = json!("DockerBuildFailed");
        manage_conditions(&mut b);
        let c = b["status"]["conditions"].as_array().unwrap();
        assert_eq!((c[0]["type"].clone(), c[0]["status"].clone()), (json!("New"), json!("False")));
        assert_eq!((c[1]["type"].clone(), c[1]["status"].clone(), c[1]["reason"].clone()),
                   (json!("Failed"), json!("True"), json!("DockerBuildFailed")));
    }

    #[test]
    fn details_takes_only_what_the_builder_owns() {
        let stored = json!({"metadata": {"name": "web-1"}, "spec": {"strategy": {"dockerStrategy": {}}},
                            "status": {"phase": "Running"}});
        let body = json!({"spec": {"strategy": {"customStrategy": {}},
                                   "revision": {"type": "Git", "git": {"commit": "abc"}}},
                          "status": {"phase": "New", "output": {"to": {"imageDigest": "sha256:1"}},
                                     "stages": [{"name": "Build"}]}});
        let out = details(&body, &stored).unwrap();
        assert_eq!(out["spec"]["strategy"], stored["spec"]["strategy"]);
        assert_eq!(out["spec"]["revision"]["git"]["commit"], "abc");
        assert_eq!(out["status"]["phase"], "Running", "a non-terminal phase is not the builder's");
        assert_eq!(out["status"]["output"]["to"]["imageDigest"], "sha256:1");
        let mut again = body.clone();
        again["spec"]["revision"]["git"]["commit"] = json!("def");
        assert!(details(&again, &out).is_err(), "a revision once set stays");
    }

    #[test]
    fn names_as_library_go_makes_them() {
        assert_eq!(get_name("web", "1", 253), "web-1");
        assert_eq!(build_pod_name("web-1"), "web-1-build");
        let long = "a".repeat(60);
        let pod = build_pod_name(&format!("{long}-1"));
        assert!(pod.len() <= 63 && pod.ends_with("-build"), "{pod}");
        assert_eq!(label_value(&"b".repeat(70)), "b".repeat(63));
    }

    #[test]
    fn pull_specs() {
        for ok in ["nginx", "nginx:1.25", "registry.local:5100/demo/web:latest", "quay.io/a/b@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "localhost/x"] {
            assert!(valid_pull_spec(ok), "{ok}");
        }
        for bad in ["", "Not Valid", "UPPER/case", "a:b:c"] {
            assert!(!valid_pull_spec(bad), "{bad}");
        }
        assert_eq!(registry_of("registry.local:5100/demo/web"), "registry.local:5100");
        assert_eq!(registry_of("library/nginx"), "docker.io");
    }

    #[test]
    fn a_docker_secret_is_found_for_its_registry() {
        use base64::Engine;
        let cfg = base64::engine::general_purpose::STANDARD
            .encode(r#"{"auths":{"registry.local:5100":{"auth":"eDp5"}}}"#);
        let secrets = vec![
            json!({"metadata": {"name": "opaque"}, "type": "Opaque", "data": {}}),
            json!({"metadata": {"name": "local"}, "type": "kubernetes.io/dockerconfigjson", "data": {".dockerconfigjson": cfg}}),
        ];
        assert_eq!(find_docker_secret(&secrets, "registry.local:5100/demo/web:1"), Some(json!({"name": "local"})));
        assert_eq!(find_docker_secret(&secrets, "quay.io/x/y"), None);
    }

    const RUBY: &str = "node1.lo:5000/demo/ruby@sha256:3333";

    /// The registry: `demo/ruby` with tag `3`; `down` cannot be reached.
    fn registry(path: String) -> std::future::Ready<Fetched> {
        std::future::ready(match path.as_str() {
            "/apis/image.openshift.io/v1/namespaces/demo/imagestreams/ruby" => Fetched::Found(json!({
                "metadata": {"name": "ruby", "namespace": "demo"},
                "status": {"tags": [{"tag": "3", "items": [{"dockerImageReference": RUBY, "image": "sha256:3333"}]}]}})),
            "/apis/image.openshift.io/v1/namespaces/demo/imagestreamimages/ruby@sha256:3333" => Fetched::Found(json!({
                "image": {"dockerImageReference": RUBY}})),
            p if p.contains("/namespaces/down/") => Fetched::Failed("connection refused".into()),
            _ => Fetched::NotFound,
        })
    }

    #[tokio::test]
    async fn image_stream_references_resolve_through_the_registry() {
        let ist = |name: &str, ns: &str| json!({"kind": "ImageStreamTag", "name": name, "namespace": ns});
        assert_eq!(resolve_image(&registry, &ist("ruby:3", ""), "demo").await.unwrap(), RUBY);
        assert_eq!(resolve_image(&registry, &json!({"kind": "ImageStreamImage", "name": "ruby@sha256:3333"}), "demo").await.unwrap(), RUBY);
        for (name, ns, missing) in [("ruby:4", "", "imagestreamtags"), ("node:20", "", "imagestreams"), ("ruby:3", "other", "imagestreams")] {
            let e = resolve_image(&registry, &ist(name, ns), "demo").await.unwrap_err();
            assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
            assert!(e.message.starts_with(&format!("Error resolving ImageStreamTag {name} in namespace")), "{}", e.message);
            assert!(e.message.contains(missing), "{}", e.message);
        }
        let e = resolve_image(&registry, &ist("ruby:3", "down"), "demo").await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resolve_image(&registry, &json!({"kind": "DockerImage", "name": "nginx"}), "demo").await.unwrap(), "nginx");
    }

    fn source_bc() -> Value {
        json!({"metadata": {"name": "web", "namespace": "demo"},
               "spec": {"source": {"git": {"uri": "https://git.example/web.git"}},
                        "strategy": {"sourceStrategy": {"from": {"kind": "ImageStreamTag", "name": "ruby:3"}}},
                        "triggers": [{"type": "ImageChange", "imageChange": {}}]}})
    }

    #[test]
    fn triggers_validate_as_upstreams() {
        let mut ok = source_bc();
        ok["spec"]["triggers"] = json!([{"type": "ConfigChange"}, {"type": "ImageChange", "imageChange": {}},
            {"type": "ImageChange", "imageChange": {"from": {"kind": "ImageStreamTag", "name": "base:1"}}},
            {"type": "GitHub", "github": {"secret": "s3cret"}}, {"type": "Generic", "generic": {"secretReference": {"name": "hook"}}},
            {"type": "GitLab", "gitlab": {"secret": "x"}}, {"type": "Bitbucket", "bitbucket": {"secret": "y"}}]);
        admit_config_create(&mut ok).unwrap();
        let cases: Vec<(&str, Value)> = vec![
            ("spec.triggers[0].github: Required value", json!([{"type": "GitHub"}])),
            ("spec.triggers[0].generic.secret: Required value", json!([{"type": "Generic", "generic": {}}])),
            ("spec.triggers[0].gitlab.secretReference: Invalid value",
             json!([{"type": "GitLab", "gitlab": {"secret": "a", "secretReference": {"name": "b"}}}])),
            ("spec.triggers[0].imageChange.from.name: Invalid value: \"base\"",
             json!([{"type": "ImageChange", "imageChange": {"from": {"kind": "ImageStreamTag", "name": "base"}}}])),
            ("spec.triggers[0].imageChange.from.kind: Invalid value: \"DockerImage\"",
             json!([{"type": "ImageChange", "imageChange": {"from": {"kind": "DockerImage", "name": "a:b"}}}])),
            ("spec.triggers: Invalid value", json!([{"type": "ImageChange"}, {"type": "imageChange"}])),
            ("spec.triggers[1].imageChange: Invalid value: {}: multiple ImageChange triggers",
             json!([{"type": "ImageChange", "imageChange": {"from": {"kind": "ImageStreamTag", "name": "ruby:3"}}}, {"type": "ImageChange"}])),
        ];
        for (want, triggers) in cases {
            let mut bc = source_bc();
            bc["spec"]["triggers"] = triggers;
            let e = admit_config_create(&mut bc).unwrap_err();
            assert!(e.message.contains(want), "{want}: {}", e.message);
        }
        let mut docker = docker_bc();
        docker["spec"]["triggers"] = json!([{"type": "ImageChange"}]);
        assert!(admit_config_create(&mut docker).unwrap_err().message.contains("a default ImageChange trigger can only be used"));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn webhook_deliveries_as_upstreams_plugins_read_them() {
        let source = json!({"git": {"uri": "https://git.example/web.git", "ref": "main"}});
        let json_ct = ("content-type", "application/json");
        let push = |r: &str| json!({"ref": r, "head_commit": {"id": "abc123", "message": "fix",
            "author": {"name": "A", "email": "a@x"}, "committer": {"name": "C", "email": "c@x"}}}).to_string();
        let gh = headers(&[("x-github-event", "push"), json_ct]);
        match extract_hook("GitHub", &json!({}), &source, &gh, push("refs/heads/main").as_bytes()).unwrap() {
            HookCall::Build { revision, .. } => {
                assert_eq!(revision["git"]["commit"], "abc123");
                assert_eq!(revision["git"]["committer"]["email"], "c@x");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(extract_hook("GitHub", &json!({}), &source, &gh, push("refs/heads/dev").as_bytes()).unwrap(), HookCall::Skip(_)));
        assert!(matches!(extract_hook("GitHub", &json!({}), &source, &headers(&[("x-github-event", "ping")]), b"{}").unwrap(),
                         HookCall::Skip(_)));
        assert!(extract_hook("GitHub", &json!({}), &source, &headers(&[("x-github-event", "issues"), json_ct]), b"{}").is_err());
        assert!(extract_hook("GitHub", &json!({}), &source, &headers(&[("x-github-event", "push")]), b"{}").is_err(), "not JSON");
        // No ref in the config: master.
        assert!(git_ref_matches("refs/heads/master", &json!({"git": {"uri": "u"}})));
        assert!(!git_ref_matches("refs/heads/main", &json!({"git": {"uri": "u"}})));
        // GitLab, Bitbucket.
        let gl = json!({"ref": "refs/heads/main", "checkout_sha": "sha",
                        "commits": [{"id": "c1", "message": "one", "author": {"name": "A", "email": "a@x"}}]}).to_string();
        let call = extract_hook("GitLab", &json!({}), &source, &headers(&[("x-gitlab-event", "Push Hook"), json_ct]), gl.as_bytes()).unwrap();
        assert!(matches!(call, HookCall::Build { ref revision, .. } if revision["git"]["commit"] == "c1"));
        let bb = json!({"push": {"changes": [{"new": {"type": "branch", "name": "main",
            "target": {"hash": "h1", "message": "m", "author": {"raw": "Ann <ann@x>"}}}}]}}).to_string();
        let call = extract_hook("Bitbucket", &json!({}), &source, &headers(&[("x-event-key", "repo:push"), json_ct]), bb.as_bytes()).unwrap();
        assert!(matches!(call, HookCall::Build { ref revision, .. }
                         if revision["git"]["commit"] == "h1" && revision["git"]["author"]["email"] == "ann@x"));
        // Generic: no body is a build; env only when allowed.
        assert!(matches!(extract_hook("Generic", &json!({}), &source, &headers(&[]), b"").unwrap(), HookCall::Build { .. }));
        let gen = json!({"git": {"ref": "main", "commit": "g1"}, "env": [{"name": "A", "value": "1"}]}).to_string();
        let call = extract_hook("Generic", &json!({}), &source, &headers(&[json_ct]), gen.as_bytes()).unwrap();
        assert!(matches!(call, HookCall::Build { ref env, ref revision, .. } if env.is_null() && revision["git"]["commit"] == "g1"));
        let call = extract_hook("Generic", &json!({"allowEnv": true}), &source, &headers(&[json_ct]), gen.as_bytes()).unwrap();
        assert!(matches!(call, HookCall::Build { ref env, .. } if env[0]["name"] == "A"));
        let yaml = "git:\n  ref: dev\n";
        assert!(matches!(extract_hook("Generic", &json!({}), &source, &headers(&[("content-type", "application/yaml")]), yaml.as_bytes()).unwrap(),
                         HookCall::Skip(_)));
    }

    #[tokio::test]
    async fn update_image_triggers_records_each_image() {
        let mut bc = source_bc();
        admit_config_create(&mut bc).unwrap();
        // The first build: the default trigger resolves, and is the strategy's image.
        let image = update_image_triggers(&registry, &mut bc, &json!({})).await.unwrap();
        assert_eq!(image.as_deref(), Some(RUBY));
        assert_eq!(bc["spec"]["triggers"][0]["imageChange"]["lastTriggeredImageID"], RUBY);
        assert_eq!(bc["status"]["imageChangeTriggers"][0]["from"], json!({"name": "ruby:3", "namespace": "demo"}));
        // The trigger controller's request for the same image is refused; a new one is taken.
        let req = |img: &str| json!({"triggeredByImage": {"kind": "DockerImage", "name": img},
                                     "from": {"kind": "ImageStreamTag", "name": "ruby:3"}});
        let e = update_image_triggers(&registry, &mut bc, &req(RUBY)).await.unwrap_err();
        assert!(e.message.contains("has already instantiated a build for imageid"), "{}", e.message);
        let newer = "node1.lo:5000/demo/ruby@sha256:4444";
        assert_eq!(update_image_triggers(&registry, &mut bc, &req(newer)).await.unwrap().as_deref(), Some(newer));
        assert_eq!(bc["status"]["imageChangeTriggers"][0]["lastTriggeredImageID"], newer);
        assert!(bc["status"]["imageChangeTriggers"][0]["lastTriggerTime"].is_string());
        // A default trigger whose stream is missing fails the build.
        let mut gone = source_bc();
        gone["spec"]["strategy"]["sourceStrategy"]["from"]["name"] = json!("ruby:9");
        assert!(update_image_triggers(&registry, &mut gone, &json!({})).await.is_err());
    }

    #[test]
    fn request_options_land_on_the_build() {
        let mut b = json!({"spec": {"strategy": {"dockerStrategy": {"env": [{"name": "A", "value": "1"}],
                                                                  "buildArgs": [{"name": "X", "value": "1"}]}}}});
        let req = json!({"env": [{"name": "A", "value": "2"}, {"name": "B", "value": "3"}],
                         "dockerStrategyOptions": {"buildArgs": [{"name": "X", "value": "2"}], "noCache": true},
                         "triggeredBy": [{"message": "Manually triggered"}]});
        apply_request(&mut b, &req, "web", "demo").unwrap();
        assert_eq!(b["spec"]["strategy"]["dockerStrategy"]["env"], json!([{"name": "A", "value": "2"}, {"name": "B", "value": "3"}]));
        assert_eq!(b["spec"]["strategy"]["dockerStrategy"]["buildArgs"], json!([{"name": "X", "value": "2"}]));
        assert_eq!(b["spec"]["strategy"]["dockerStrategy"]["noCache"], true);
        assert_eq!(b["spec"]["triggeredBy"][0]["message"], "Manually triggered");
        let mut source = json!({"spec": {"strategy": {"sourceStrategy": {}}}});
        assert!(apply_request(&mut source, &json!({"dockerStrategyOptions": {"noCache": true}}), "web", "demo").is_err());
    }

    #[test]
    fn clean_paths() {
        assert_eq!(clean("a/../b"), "b");
        assert_eq!(clean("../x"), "../x");
        assert_eq!(clean("./"), ".");
        assert_eq!(clean("/a/../.."), "/");
    }
}
