//! `security.openshift.io/v1` SecurityContextConstraints as objects (#373,
//! sc#151): what an SCC is, how it is defaulted and validated, how SCCs are
//! ordered, and the provider an SCC becomes for a namespace — the engine
//! [`crate::scc`] admission and the three PodSecurityPolicy reviews
//! (`handlers::scc_review`) run.
//!
//! Ported from openshift/apiserver-library-go (`sccmatching`, `sccdefaults`,
//! `util/sort`, the strategy packages) and openshift-apiserver's validation,
//! so a pod is admitted, mutated and refused with OpenShift's messages and
//! field paths — the doubled `.containers[0].containers[0].hostPort` included.
//!
//! - **Defaulting** ([`defaults`], `SetDefaults_SCC`): strategy types
//!   `RunAsAny` when unset, `users`/`groups` `[]`, `allowPrivilegeEscalation`
//!   `true`, and the volume list resolved (`*` without
//!   `allowHostDirVolumePlugin` expands to every type but `hostPath`).
//! - **Order** ([`sort`]): higher `priority` first (null is 0), then the
//!   more restrictive by OpenShift's points, then by name.
//! - **Preallocated values**: `MustRunAsRange` without a range takes the
//!   namespace's `openshift.io/sa.scc.uid-range`; `fsGroup` `MustRunAs`
//!   without ranges takes the first group of `…supplemental-groups` (that one
//!   group only, as OpenShift); `supplementalGroups` `MustRunAs`, every block.
//! - **Who may use one** ([`applies_to`]): the SCC lists the user, or one of
//!   its groups, or RBAC grants `use` on it.
//!
//! One deviation, kept from #321: rustkube assigns no SELinux MCS levels
//! (stormcos nodes do not run SELinux), so a `MustRunAs` SELinux strategy is
//! enforced only when the SCC names a level itself; otherwise it sets and
//! checks nothing.

use crate::auth::UserInfo;
use crate::error::ApiError;
use crate::rbac_engine::{AuthorizationRequest, RbacEngine};
use crate::storage::ResourceStorage;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// The stored plural.
pub const RESOURCE: &str = "securitycontextconstraints";
pub const GROUP: &str = "security.openshift.io";
/// A pod asking for one SCC by name.
pub const REQUIRED_SCC: &str = "openshift.io/required-scc";
/// Whether the SCC was usable by the pod's ServiceAccount or its creator.
pub const SUBJECT_TYPE: &str = "security.openshift.io/validated-scc-subject-type";
pub const POD_SECCOMP: &str = "seccomp.security.alpha.kubernetes.io/pod";
pub const CONTAINER_SECCOMP: &str = "container.seccomp.security.alpha.kubernetes.io/";
/// The SCCs rustkube ships; admission refuses pods while one is missing,
/// as OpenShift's does for its standard set.
pub const BUILTIN: [&str; 3] = ["anyuid", "privileged", "restricted-v2"];

/// The volume types `anyuid` and `restricted-v2` allow: OpenShift's list,
/// plus `image` (#321).
const SAFE_VOLUMES: [&str; 9] = [
    "configMap", "csi", "downwardAPI", "emptyDir", "ephemeral", "image", "persistentVolumeClaim", "projected", "secret",
];

/// Every volume type (`GetAllFSTypesAsSet`).
const ALL_VOLUMES: [&str; 30] = [
    "hostPath", "azureFile", "flocker", "flexVolume", "emptyDir", "gcePersistentDisk", "awsElasticBlockStore", "gitRepo",
    "secret", "nfs", "iscsi", "glusterfs", "persistentVolumeClaim", "rbd", "cinder", "cephFS", "downwardAPI", "fc",
    "configMap", "vsphere", "quobyte", "azureDisk", "photonPersistentDisk", "projected", "portworxVolume", "scaleIO",
    "storageOS", "csi", "ephemeral", "image",
];

/// A volume's JSON field → its SCC volume type (`GetVolumeFSType`).
const VOLUME_FIELDS: [(&str, &str); 30] = [
    ("hostPath", "hostPath"), ("emptyDir", "emptyDir"), ("gcePersistentDisk", "gcePersistentDisk"),
    ("awsElasticBlockStore", "awsElasticBlockStore"), ("gitRepo", "gitRepo"), ("secret", "secret"), ("nfs", "nfs"),
    ("iscsi", "iscsi"), ("glusterfs", "glusterfs"), ("persistentVolumeClaim", "persistentVolumeClaim"), ("rbd", "rbd"),
    ("flexVolume", "flexVolume"), ("cinder", "cinder"), ("cephfs", "cephFS"), ("flocker", "flocker"),
    ("downwardAPI", "downwardAPI"), ("fc", "fc"), ("azureFile", "azureFile"), ("configMap", "configMap"),
    ("vsphereVolume", "vsphere"), ("quobyte", "quobyte"), ("azureDisk", "azureDisk"),
    ("photonPersistentDisk", "photonPersistentDisk"), ("projected", "projected"), ("portworxVolume", "portworxVolume"),
    ("scaleIO", "scaleIO"), ("storageos", "storageOS"), ("csi", "csi"), ("ephemeral", "ephemeral"), ("image", "image"),
];

/// Upstream's safe sysctls (kernel ≥ 5.7 for the last three).
const SAFE_SYSCTLS: [&str; 12] = [
    "kernel.shm_rmid_forced", "net.ipv4.ip_local_port_range", "net.ipv4.tcp_syncookies", "net.ipv4.ping_group_range",
    "net.ipv4.ip_unprivileged_port_start", "net.ipv4.tcp_keepalive_time", "net.ipv4.tcp_fin_timeout",
    "net.ipv4.tcp_keepalive_intvl", "net.ipv4.tcp_keepalive_probes", "net.ipv4.ip_local_reserved_ports",
    "net.ipv4.tcp_rmem", "net.ipv4.tcp_wmem",
];

fn builtin(name: &str, description: &str, priority: Value, fields: Value) -> Value {
    let mut scc = json!({
        "apiVersion": "security.openshift.io/v1",
        "kind": "SecurityContextConstraints",
        "metadata": {"name": name, "annotations": {
            "kubernetes.io/description": description,
            "release.openshift.io/create-only": "true",
        }},
        "priority": priority,
        "allowHostDirVolumePlugin": false, "allowHostIPC": false, "allowHostNetwork": false, "allowHostPID": false,
        "allowHostPorts": false, "allowPrivilegedContainer": false, "readOnlyRootFilesystem": false,
        "allowedCapabilities": null, "defaultAddCapabilities": null, "requiredDropCapabilities": null,
        "users": [], "groups": [],
        "volumes": SAFE_VOLUMES,
        "userNamespaceLevel": "AllowHostLevel",
    });
    for (k, v) in fields.as_object().into_iter().flatten() {
        scc[k] = v.clone();
    }
    scc
}

/// `anyuid`, `restricted-v2` and `privileged` as OpenShift ships them.
pub fn builtins() -> Vec<Value> {
    vec![
        builtin(
            "anyuid",
            "anyuid provides all features of the restricted SCC but allows users to run with any UID and any GID.",
            json!(10),
            json!({
                "allowPrivilegeEscalation": true,
                "fsGroup": {"type": "RunAsAny"},
                "groups": ["system:cluster-admins"],
                "requiredDropCapabilities": ["MKNOD"],
                "runAsUser": {"type": "RunAsAny"},
                "seLinuxContext": {"type": "MustRunAs"},
                "supplementalGroups": {"type": "RunAsAny"},
            }),
        ),
        builtin(
            "restricted-v2",
            "restricted-v2 denies access to all host features and requires pods to be run with a UID, and SELinux \
             context that are allocated to the namespace. This is the most restrictive SCC and it is used by default \
             for authenticated users. On top of the legacy 'restricted' SCC, it also requires to drop ALL capabilities \
             and does not allow privilege escalation binaries. It will also default the seccomp profile to \
             runtime/default if unset, otherwise this seccomp profile is required.",
            Value::Null,
            json!({
                "allowPrivilegeEscalation": false,
                "allowedCapabilities": ["NET_BIND_SERVICE"],
                "fsGroup": {"type": "MustRunAs"},
                "requiredDropCapabilities": ["ALL"],
                "runAsUser": {"type": "MustRunAsRange"},
                "seLinuxContext": {"type": "MustRunAs"},
                "seccompProfiles": ["runtime/default"],
                "supplementalGroups": {"type": "RunAsAny"},
            }),
        ),
        builtin(
            "privileged",
            "privileged allows access to all privileged and host features and the ability to run as any user, any \
             group, any fsGroup, and with any SELinux context.  WARNING: this is the most relaxed SCC and should be \
             used only for cluster administration. Grant with caution.",
            Value::Null,
            json!({
                "allowHostDirVolumePlugin": true, "allowHostIPC": true, "allowHostNetwork": true, "allowHostPID": true,
                "allowHostPorts": true, "allowPrivilegeEscalation": true, "allowPrivilegedContainer": true,
                "allowedCapabilities": ["*"],
                "allowedUnsafeSysctls": ["*"],
                "fsGroup": {"type": "RunAsAny"},
                "groups": ["system:cluster-admins", "system:nodes", "system:masters"],
                "runAsUser": {"type": "RunAsAny"},
                "seLinuxContext": {"type": "RunAsAny"},
                "seccompProfiles": ["*"],
                "supplementalGroups": {"type": "RunAsAny"},
                "users": ["system:admin", "system:serviceaccount:openshift-infra:build-controller"],
                "volumes": ["*"],
            }),
        ),
    ]
}

/// Create each built-in SCC that is missing; a stored one is left as it is
/// (`release.openshift.io/create-only`).
pub async fn bootstrap(storage: &ResourceStorage) {
    for scc in builtins() {
        let name = scc["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let key = ResourceStorage::cluster_key(RESOURCE, &name);
        match storage.create(&key, scc).await {
            Ok(_) => {}
            Err(e) if e.reason == "AlreadyExists" || e.reason == "Conflict" => {}
            Err(e) => tracing::error!("bootstrap: SCC {name} was not created: {}. Pods will be refused.", e.message),
        }
    }
}

// --- field errors, as apimachinery's field.Error prints them ---------------

fn invalid(path: &str, value: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("{path}: Invalid value: {value}")
    } else {
        format!("{path}: Invalid value: {value}: {detail}")
    }
}

fn required(path: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("{path}: Required value")
    } else {
        format!("{path}: Required value: {detail}")
    }
}

fn forbidden(path: &str, detail: &str) -> String {
    format!("{path}: Forbidden: {detail}")
}

fn not_supported(path: &str, value: &str, valid: &[&str]) -> String {
    let quoted: Vec<String> = valid.iter().map(|v| format!("{v:?}")).collect();
    format!("{path}: Unsupported value: {value}: supported values: {}", quoted.join(", "))
}

/// Go's `%q` of a string.
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A nil pointer's value, as field.Error prints it.
const NULL: &str = "\"null\"";

fn int64s(v: &[i64]) -> String {
    let parts: Vec<String> = v.iter().map(i64::to_string).collect();
    format!("[]int64{{{}}}", parts.join(", "))
}

fn capabilities_repr(v: &[String]) -> String {
    let parts: Vec<String> = v.iter().map(|c| q(c)).collect();
    format!("[]core.Capability{{{}}}", parts.join(", "))
}

/// Go's aggregate error text: the distinct messages, bare when one.
pub fn aggregate(errs: &[String]) -> String {
    let mut seen = BTreeSet::new();
    let distinct: Vec<&String> = errs.iter().filter(|e| seen.insert(e.as_str())).collect();
    match distinct.as_slice() {
        [one] => (*one).clone(),
        many => format!("[{}]", many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

// --- defaulting and validation of the object --------------------------------

/// `SetDefaults_SCC`, and the fields OpenShift always serializes.
pub fn defaults(scc: &mut Value) {
    if !scc.is_object() {
        return;
    }
    for (k, t) in [("fsGroup", true), ("supplementalGroups", true), ("runAsUser", false), ("seLinuxContext", false)] {
        if !scc[k].is_object() {
            scc[k] = json!({});
        }
        if t && scc[k]["type"].as_str().unwrap_or("").is_empty() {
            scc[k]["type"] = json!("RunAsAny");
        }
    }
    for k in ["users", "groups"] {
        if scc[k].is_null() {
            scc[k] = json!([]);
        }
    }
    let map = scc.as_object_mut().unwrap();
    for k in ["priority", "defaultAddCapabilities", "requiredDropCapabilities", "allowedCapabilities"] {
        map.entry(k).or_insert(Value::Null);
    }
    for k in ["allowPrivilegedContainer", "allowHostDirVolumePlugin", "allowHostNetwork", "allowHostPorts",
              "allowHostPID", "allowHostIPC", "readOnlyRootFilesystem"] {
        map.entry(k).or_insert(json!(false));
    }
    let host_dir = scc["allowHostDirVolumePlugin"].as_bool() == Some(true);
    let mut volumes: BTreeSet<String> = match &scc["volumes"] {
        Value::Array(list) if list.is_empty() && host_dir => ["hostPath".to_string()].into(),
        Value::Array(list) if list.is_empty() => ["none".to_string()].into(),
        Value::Array(list) => list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => ["*".to_string()].into(),
    };
    if host_dir {
        if !volumes.contains("*") {
            volumes.insert("hostPath".into());
        }
    } else {
        let all = volumes.remove("*");
        volumes.remove("hostPath");
        if all {
            volumes.extend(ALL_VOLUMES.iter().filter(|v| **v != "hostPath").map(|v| v.to_string()));
        }
    }
    scc["volumes"] = json!(volumes.into_iter().collect::<Vec<_>>());
    if scc["allowPrivilegeEscalation"].is_null() {
        scc["allowPrivilegeEscalation"] = json!(true);
    }
}

fn sysctl_pattern_ok(s: &str) -> bool {
    // `(SysctlSegmentFmt\.)*([a-z0-9][-_a-z0-9]*)?[a-z0-9*]`, at most 253.
    if s.len() > 253 {
        return false;
    }
    let segs: Vec<&str> = s.split(['.', '/']).collect();
    let (last, rest) = segs.split_last().unwrap();
    let seg_ok = |p: &str| {
        let b = p.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && (b[b.len() - 1].is_ascii_lowercase() || b[b.len() - 1].is_ascii_digit())
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
    };
    let last_ok = {
        let b = last.as_bytes();
        !b.is_empty()
            && (b[b.len() - 1].is_ascii_lowercase() || b[b.len() - 1].is_ascii_digit() || b[b.len() - 1] == b'*')
            && (b.len() == 1 || b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && b[..b.len() - 1].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
    };
    rest.iter().all(|p| seg_ok(p)) && last_ok
}

fn validate_sysctls(path: &str, list: &[String], errs: &mut Vec<String>) {
    let mut covers_all = false;
    for (i, s) in list.iter().enumerate() {
        if s.is_empty() {
            errs.push(invalid(&format!("{path}[{i}]"), &q(s), "empty sysctl not allowed"));
        } else if !sysctl_pattern_ok(s) {
            errs.push(invalid(
                &format!("{path}[{i}]"),
                &q(s),
                "must have at most 253 characters and match regex ([a-z0-9]([-_a-z0-9]*[a-z0-9])?[\\./])*([a-z0-9][-_a-z0-9]*)?[a-z0-9*]",
            ));
        } else if s.starts_with('*') {
            covers_all = true;
        }
    }
    if covers_all && list.len() > 1 {
        errs.push(forbidden(&format!("{path}.items"), "if '*' is present, must not specify other sysctls"));
    }
}

/// OpenShift's `ValidateSecurityContextConstraints`, after [`defaults`]: the
/// 422 for an SCC it would refuse.
pub fn validate(scc: &Value) -> Result<(), ApiError> {
    let name = scc["metadata"]["name"].as_str().unwrap_or("");
    let mut errs = Vec::new();
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
    if scc["metadata"]["namespace"].as_str().is_some_and(|n| !n.is_empty()) {
        errs.push(forbidden("metadata.namespace", "not allowed on this type"));
    }
    if let Some(p) = scc["priority"].as_i64().filter(|p| *p < 0) {
        errs.push(invalid("priority", &p.to_string(), "priority cannot be negative"));
    }
    let ty = |k: &str| scc[k]["type"].as_str().unwrap_or("").to_string();
    let run_as = ty("runAsUser");
    if !["MustRunAs", "MustRunAsNonRoot", "RunAsAny", "MustRunAsRange"].contains(&run_as.as_str()) {
        errs.push(invalid(
            "runAsUser.type",
            &q(&run_as),
            "invalid strategy type.  Valid values are MustRunAs, MustRunAsNonRoot, MustRunAsRange, RunAsAny",
        ));
    }
    if let Some(uid) = scc["runAsUser"]["uid"].as_i64().filter(|u| *u < 0) {
        errs.push(invalid("runAsUser.uid", &uid.to_string(), "uid cannot be negative"));
    }
    let selinux = ty("seLinuxContext");
    if !["MustRunAs", "RunAsAny"].contains(&selinux.as_str()) {
        errs.push(invalid("seLinuxContext.type", &q(&selinux), "invalid strategy type.  Valid values are MustRunAs, RunAsAny"));
    }
    for k in ["fsGroup", "supplementalGroups"] {
        let t = ty(k);
        if !["MustRunAs", "RunAsAny"].contains(&t.as_str()) {
            errs.push(not_supported(&format!("{k}.type"), &q(&t), &["MustRunAs", "RunAsAny"]));
        }
        for (i, r) in scc[k]["ranges"].as_array().into_iter().flatten().enumerate() {
            let (min, max) = (r["min"].as_i64().unwrap_or(0), r["max"].as_i64().unwrap_or(0));
            let path = format!("{k}.ranges[{i}]");
            if min < 0 {
                errs.push(invalid(&format!("{path}.min"), &min.to_string(), "min cannot be negative"));
            }
            if max < 0 {
                errs.push(invalid(&format!("{path}.max"), &max.to_string(), "max cannot be negative"));
            }
            if min > max {
                errs.push(invalid(
                    &format!("{path}.min"),
                    &format!("security.IDRange{{Min:{min}, Max:{max}}}"),
                    "min cannot be greater than max",
                ));
            }
        }
    }
    let drops = strings(&scc["requiredDropCapabilities"]);
    if !scc["requiredDropCapabilities"].is_null() {
        for k in ["defaultAddCapabilities", "allowedCapabilities"] {
            for cap in strings(&scc[k]) {
                if drops.contains(&cap) {
                    errs.push(invalid(k, &q(&cap), &format!("capability is listed in {k} and requiredDropCapabilities")));
                }
            }
        }
    }
    if strings(&scc["allowedCapabilities"]).iter().any(|c| c == "*") && !drops.is_empty() {
        errs.push(invalid(
            "requiredDropCapabilities",
            &capabilities_repr(&drops),
            "required capabilities must be empty when all capabilities are allowed by a wildcard",
        ));
    }
    if scc["defaultAllowPrivilegeEscalation"].as_bool() == Some(true)
        && scc["allowPrivilegeEscalation"].as_bool() == Some(false)
    {
        errs.push(invalid(
            "defaultAllowPrivilegeEscalation",
            "true",
            "Cannot set DefaultAllowPrivilegeEscalation to true without also setting AllowPrivilegeEscalation to true",
        ));
    }
    let volumes = strings(&scc["volumes"]);
    if volumes.iter().any(|v| v == "none") && volumes.len() > 1 {
        errs.push(invalid("volumes", &format!("{volumes:?}"), "if 'none' is specified, no other values are allowed"));
    }
    let flex = volumes.iter().any(|v| v == "flexVolume" || v == "*");
    if let Some(drivers) = scc["allowedFlexVolumes"].as_array().filter(|d| !d.is_empty()) {
        if flex {
            for (i, d) in drivers.iter().enumerate() {
                if d["driver"].as_str().unwrap_or("").is_empty() {
                    errs.push(required(&format!("allowedFlexVolumes[{i}].driver"), "must specify a driver"));
                }
            }
        } else {
            errs.push(invalid(
                "allowedFlexVolumes",
                &serde_json::to_string(drivers).unwrap_or_default(),
                "volumes does not include 'flexVolume' or '*', so no flex volumes are allowed",
            ));
        }
    }
    let allowed = strings(&scc["allowedUnsafeSysctls"]);
    let forbidden_list = strings(&scc["forbiddenSysctls"]);
    validate_sysctls("allowedUnsafeSysctls", &allowed, &mut errs);
    validate_sysctls("forbiddenSysctls", &forbidden_list, &mut errs);
    for (i, a) in allowed.iter().enumerate() {
        for (j, f) in forbidden_list.iter().enumerate() {
            let (ap, fp) = (a.strip_suffix('*'), f.strip_suffix('*'));
            let (path, value, other) = match (ap, fp) {
                (Some(ap), Some(fp)) if ap.starts_with(fp) => (format!("allowedUnsafeSysctls[{i}]"), a, f),
                (Some(ap), Some(fp)) if fp.starts_with(ap) => (format!("forbiddenSysctls[{j}]"), f, a),
                (Some(ap), None) if f.starts_with(ap) => (format!("forbiddenSysctls[{j}]"), f, a),
                (None, Some(fp)) if a.starts_with(fp) => (format!("allowedUnsafeSysctls[{i}]"), a, f),
                (None, None) if a == f => (format!("allowedUnsafeSysctls[{i}]"), a, f),
                _ => continue,
            };
            errs.push(invalid(&path, &q(value), &format!("sysctl overlaps with {other}")));
        }
    }
    if errs.is_empty() {
        return Ok(());
    }
    let what = if errs.len() == 1 { errs.remove(0) } else { format!("[{}]", errs.join(", ")) };
    Err(ApiError::invalid(&format!("SecurityContextConstraints.security.openshift.io {name:?} is invalid: {what}")))
}

/// Defaulting and validation of an SCC being written.
pub fn admit(scc: &mut Value) -> Result<(), ApiError> {
    defaults(scc);
    validate(scc)
}

// --- order ------------------------------------------------------------------

/// OpenShift's restrictiveness points: lower is more restrictive.
pub fn points(scc: &Value) -> i64 {
    let mut total = 0;
    if scc["allowPrivilegedContainer"].as_bool() == Some(true) {
        total += 1_600_000;
    }
    let mut host_volume = false;
    let mut non_trivial = false;
    for v in strings(&scc["volumes"]) {
        match v.as_str() {
            "hostPath" | "*" => host_volume = true,
            "secret" | "configMap" | "emptyDir" | "downwardAPI" | "projected" | "none" => {}
            _ => non_trivial = true,
        }
    }
    total += if host_volume {
        200_000
    } else if non_trivial {
        100_000
    } else {
        0
    };
    if scc["allowHostNetwork"].as_bool() == Some(true) {
        total += 400_000;
    }
    if scc["allowHostPorts"].as_bool() == Some(true) {
        total += 800_000;
    }
    // Capabilities, clamped to [0, 9999].
    let upper = |k: &str| strings(&scc[k]).into_iter().map(|c| c.to_uppercase()).collect::<Vec<_>>();
    let mut caps: i64 = 5000 + 300 * strings(&scc["defaultAddCapabilities"]).len() as i64;
    let allowed = upper("allowedCapabilities");
    if allowed.iter().any(|c| c == "*" || c == "ALL") {
        caps += 4000;
    } else {
        caps += 10 * strings(&scc["allowedCapabilities"]).len() as i64;
    }
    if upper("requiredDropCapabilities").iter().any(|c| c == "ALL") {
        caps -= 3000;
    } else {
        caps -= 50 * strings(&scc["requiredDropCapabilities"]).len() as i64;
    }
    total += caps.clamp(0, 9999);
    let strategy = |t: &str| match t {
        "RunAsAny" => 40_000,
        "MustRunAsNonRoot" => 30_000,
        "MustRunAsRange" => 20_000,
        "MustRunAs" => 10_000,
        _ => 0,
    };
    total += strategy(scc["seLinuxContext"]["type"].as_str().unwrap_or(""));
    total += strategy(scc["runAsUser"]["type"].as_str().unwrap_or(""));
    total
}

/// `sort.Sort(ByPriority(...))`: priority (null is 0) descending, then
/// points ascending, then name.
pub fn sort(sccs: &mut [Value]) {
    sccs.sort_by(|a, b| {
        let pa = a["priority"].as_i64().unwrap_or(0);
        let pb = b["priority"].as_i64().unwrap_or(0);
        pb.cmp(&pa)
            .then(points(a).cmp(&points(b)))
            .then(a["metadata"]["name"].as_str().unwrap_or("").cmp(b["metadata"]["name"].as_str().unwrap_or("")))
    });
}

/// Every stored SCC, sorted. `fresh` reads the datastore (the reviews, which
/// report each SCC's resourceVersion); otherwise the watch cache.
pub async fn list(storage: &ResourceStorage, fresh: bool) -> Result<Vec<Value>, ApiError> {
    let prefix = ResourceStorage::cluster_prefix(RESOURCE);
    let mut out = Vec::new();
    let cached = if fresh { None } else { storage.watch_cache().snapshot(&prefix).await.ok() };
    if let Some((_, items)) = cached {
        out.extend(items.iter().filter_map(|(_, b)| serde_json::from_slice::<Value>(b).ok()));
    } else {
        let mut token: Option<String> = None;
        loop {
            let (items, next, _) = storage.list(&prefix, 1000, token.as_deref()).await?;
            out.extend(items);
            match next {
                Some(t) => token = Some(t),
                None => break,
            }
        }
    }
    sort(&mut out);
    Ok(out)
}

// --- who may use one --------------------------------------------------------

/// `ConstraintAppliesTo`: the SCC lists the user or one of its groups, or
/// RBAC grants `use` of it in `ns`. Without an authorizer (tests only) the
/// bootstrapped grants stand in: `system:masters` may use every SCC,
/// `system:authenticated` `restricted-v2`.
pub async fn applies_to(scc: &Value, user: &UserInfo, ns: &str, rbac: Option<&RbacEngine>) -> bool {
    if strings(&scc["users"]).iter().any(|u| *u == user.username) {
        return true;
    }
    let groups = strings(&scc["groups"]);
    if user.groups.iter().any(|g| groups.contains(g)) {
        return true;
    }
    let name = scc["metadata"]["name"].as_str().unwrap_or_default();
    match rbac {
        Some(rbac) => {
            rbac.authorize(
                user,
                &AuthorizationRequest {
                    verb: "use".into(),
                    resource: RESOURCE.into(),
                    subresource: None,
                    api_group: GROUP.into(),
                    namespace: Some(ns.into()),
                    name: Some(name.into()),
                },
            )
            .await
        }
        None => user.groups.iter().any(|g| g == "system:masters" || (g == "system:authenticated" && name == "restricted-v2")),
    }
}

/// A ServiceAccount as the authenticator names it (no
/// `system:authenticated`: `serviceaccount.UserInfo`).
pub fn service_account_user(ns: &str, sa: &str) -> UserInfo {
    UserInfo {
        username: format!("system:serviceaccount:{ns}:{sa}"),
        groups: vec!["system:serviceaccounts".into(), format!("system:serviceaccounts:{ns}")],
    }
}

// --- the provider -----------------------------------------------------------

#[derive(Debug, Clone)]
enum UserStrategy {
    MustRunAs(i64),
    Range(i64, i64),
    NonRoot,
    Any,
}

#[derive(Debug, Clone)]
enum GroupStrategy {
    MustRunAs(Vec<(i64, i64)>),
    Any,
}

impl GroupStrategy {
    fn generate(&self) -> Option<i64> {
        match self {
            GroupStrategy::MustRunAs(r) => Some(r[0].0),
            GroupStrategy::Any => None,
        }
    }

    fn validate(&self, path: &str, field: &str, groups: &[i64], errs: &mut Vec<String>) {
        let GroupStrategy::MustRunAs(ranges) = self else { return };
        let p = format!("{path}.{field}");
        if groups.is_empty() {
            errs.push(invalid(&p, &int64s(groups), "unable to validate empty groups against required ranges"));
        }
        for g in groups {
            if !ranges.iter().any(|(min, max)| g >= min && g <= max) {
                errs.push(invalid(&p, &int64s(groups), &format!("{g} is not an allowed group")));
            }
        }
    }
}

/// An SCC made ready for one namespace (`CreateProviderFromConstraint`).
#[derive(Debug, Clone)]
pub struct Provider {
    /// The SCC with its preallocated values filled in.
    pub scc: Value,
    pub name: String,
    run_as_user: UserStrategy,
    fs_group: GroupStrategy,
    supplemental: GroupStrategy,
    /// `MustRunAs` with a level named by the SCC (see the module doc).
    selinux: Option<Value>,
    seccomp_any: bool,
    seccomp_allowed: Vec<String>,
    seccomp_runtime_default: bool,
}

fn first_blocks(ns: &Value, key: &str) -> Option<Vec<(i64, i64)>> {
    let text = ns["metadata"]["annotations"][key].as_str().filter(|t| !t.is_empty())?;
    let ranges = crate::scc::parse_ranges(text)?;
    Some(ranges.iter().map(|r| (r.min as i64, r.max as i64)).collect())
}

impl Provider {
    pub fn new(ns: &Value, scc: &Value) -> Result<Provider, String> {
        let mut scc = scc.clone();
        let name = scc["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let ns_name = ns["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let ty = |s: &Value, k: &str| s[k]["type"].as_str().unwrap_or("").to_string();
        let annotation_missing = |what: &str, key: &str| {
            format!(
                "unable to find pre-allocated {what} annotation for namespace {ns_name} while trying to configure SCC \
                 {name}: unable to find annotation {key}"
            )
        };
        if ty(&scc, "runAsUser") == "MustRunAsRange"
            && scc["runAsUser"]["uidRangeMin"].is_null()
            && scc["runAsUser"]["uidRangeMax"].is_null()
        {
            let blocks = first_blocks(ns, crate::scc::UID_RANGE)
                .ok_or_else(|| annotation_missing("uid", crate::scc::UID_RANGE))?;
            scc["runAsUser"]["uidRangeMin"] = json!(blocks[0].0);
            scc["runAsUser"]["uidRangeMax"] = json!(blocks[0].1);
        }
        let group_blocks = || {
            first_blocks(ns, crate::scc::SUPPLEMENTAL_GROUPS)
                .or_else(|| first_blocks(ns, crate::scc::UID_RANGE))
                .ok_or_else(|| {
                    format!(
                        "unable to find pre-allocated group annotation for namespace {ns_name} while trying to \
                         configure SCC {name}: unable to find supplemental group or uid annotation for namespace \
                         {ns_name}"
                    )
                })
        };
        if ty(&scc, "fsGroup") == "MustRunAs" && scc["fsGroup"]["ranges"].as_array().is_none_or(|r| r.is_empty()) {
            let first = group_blocks()?[0].0;
            scc["fsGroup"]["ranges"] = json!([{"min": first, "max": first}]);
        }
        if ty(&scc, "supplementalGroups") == "MustRunAs"
            && scc["supplementalGroups"]["ranges"].as_array().is_none_or(|r| r.is_empty())
        {
            let blocks: Vec<Value> = group_blocks()?.iter().map(|(a, b)| json!({"min": a, "max": b})).collect();
            scc["supplementalGroups"]["ranges"] = json!(blocks);
        }
        let creating = |e: &str| format!("error creating provider for SCC {name} in namespace {ns_name}: {e}");
        let run_as_user = match ty(&scc, "runAsUser").as_str() {
            "MustRunAs" => UserStrategy::MustRunAs(
                scc["runAsUser"]["uid"].as_i64().ok_or_else(|| creating("MustRunAs requires a UID"))?,
            ),
            "MustRunAsRange" => UserStrategy::Range(
                scc["runAsUser"]["uidRangeMin"].as_i64().ok_or_else(|| creating("MustRunAsRange requires a UIDRangeMin"))?,
                scc["runAsUser"]["uidRangeMax"].as_i64().ok_or_else(|| creating("MustRunAsRange requires a UIDRangeMax"))?,
            ),
            "MustRunAsNonRoot" => UserStrategy::NonRoot,
            "RunAsAny" => UserStrategy::Any,
            other => return Err(creating(&format!("Unrecognized RunAsUser strategy type {other}"))),
        };
        let selinux = match ty(&scc, "seLinuxContext").as_str() {
            "MustRunAs" => scc["seLinuxContext"]["seLinuxOptions"]
                .as_object()
                .filter(|o| o.get("level").and_then(Value::as_str).is_some_and(|l| !l.is_empty()))
                .map(|o| Value::Object(o.clone())),
            "RunAsAny" => None,
            other => return Err(creating(&format!("Unrecognized SELinuxContext strategy type {other}"))),
        };
        let group = |k: &str, what: &str| -> Result<GroupStrategy, String> {
            match ty(&scc, k).as_str() {
                "RunAsAny" => Ok(GroupStrategy::Any),
                "MustRunAs" => {
                    let ranges: Vec<(i64, i64)> = scc[k]["ranges"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|r| (r["min"].as_i64().unwrap_or(0), r["max"].as_i64().unwrap_or(0)))
                        .collect();
                    if ranges.is_empty() {
                        return Err(creating("ranges must be supplied for MustRunAs"));
                    }
                    Ok(GroupStrategy::MustRunAs(ranges))
                }
                other => Err(creating(&format!("Unrecognized {what} strategy type {other}"))),
            }
        };
        let fs_group = group("fsGroup", "FSGroup")?;
        let supplemental = group("supplementalGroups", "SupplementalGroups")?;
        let mut seccomp_any = false;
        let mut seccomp_allowed = Vec::new();
        let mut seccomp_runtime_default = false;
        for p in strings(&scc["seccompProfiles"]) {
            if p == "*" {
                seccomp_any = true;
                continue;
            }
            if p == "docker/default" || p == "runtime/default" {
                seccomp_runtime_default = true;
            }
            seccomp_allowed.push(p);
        }
        Ok(Provider {
            scc,
            name,
            run_as_user,
            fs_group,
            supplemental,
            selinux,
            seccomp_any,
            seccomp_allowed,
            seccomp_runtime_default,
        })
    }

    fn flag(&self, k: &str) -> bool {
        self.scc[k].as_bool() == Some(true)
    }

    fn validate_profile(&self, path: &str, profile: &str) -> Option<String> {
        if !self.seccomp_any && self.seccomp_allowed.is_empty() && !profile.is_empty() {
            return Some(forbidden(path, "seccomp may not be set"));
        }
        if (self.seccomp_allowed.is_empty() && profile.is_empty()) || self.seccomp_any {
            return None;
        }
        for p in &self.seccomp_allowed {
            if profile == p.as_str()
                || (self.seccomp_runtime_default && (profile == "docker/default" || profile == "runtime/default"))
            {
                return None;
            }
        }
        Some(forbidden(
            path,
            &format!(
                "{profile} is not an allowed seccomp profile. Valid values are [{}]",
                self.seccomp_allowed.join(" ")
            ),
        ))
    }

    fn validate_selinux(&self, path: &str, options: &Value, errs: &mut Vec<String>) {
        let Some(want) = &self.selinux else { return };
        if !options.is_object() {
            errs.push(required(path, ""));
            return;
        }
        let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
        if !equal_levels(&s(want, "level"), &s(options, "level")) {
            errs.push(invalid(&format!("{path}.level"), &q(&s(options, "level")), &format!("must be {}", s(want, "level"))));
        }
        for k in ["role", "type", "user"] {
            if s(options, k) != s(want, k) {
                errs.push(invalid(&format!("{path}.{k}"), &q(&s(options, k)), &format!("must be {}", s(want, k))));
            }
        }
    }

    /// `AssignSecurityContext`: fill in what `pod` leaves unset and return
    /// every way it falls outside this SCC, prefixed `provider <name>: `.
    /// Mutates `pod` (the caller's copy) either way.
    pub fn assign(&self, pod: &mut Value) -> Vec<String> {
        let prefix = format!("provider {}: ", self.name);
        let mut errs = Vec::new();
        if !pod["spec"].is_object() {
            pod["spec"] = json!({});
        }
        self.create_pod_security_context(pod);
        self.validate_pod_security_context(pod, &format!("{prefix}.spec"), &mut errs);
        for (field, i) in container_slots(pod) {
            let path = format!("{prefix}.{field}[{i}]");
            let psc = pod["spec"]["securityContext"].clone();
            let annotations = pod["metadata"]["annotations"].clone();
            let mut c = pod["spec"][field][i].take();
            self.create_container_security_context(&psc, &annotations, &mut c);
            pod["spec"][field][i] = c;
            self.validate_container_security_context(pod, field, i, &path, &mut errs);
        }
        errs
    }

    fn create_pod_security_context(&self, pod: &mut Value) {
        let existed = pod["spec"]["securityContext"].is_object();
        let mut psc = if existed { pod["spec"]["securityContext"].clone() } else { json!({}) };
        if psc["supplementalGroups"].is_null() {
            if let Some(g) = self.supplemental.generate() {
                psc["supplementalGroups"] = json!([g]);
            }
        }
        if psc["fsGroup"].is_null() {
            if let Some(g) = self.fs_group.generate() {
                psc["fsGroup"] = json!(g);
            }
        }
        if psc["seLinuxOptions"].is_null() {
            if let Some(o) = &self.selinux {
                psc["seLinuxOptions"] = o.clone();
            }
        }
        let annotation = pod["metadata"]["annotations"][POD_SECCOMP].as_str().unwrap_or("").to_string();
        let profile = if !annotation.is_empty() {
            annotation
        } else if psc["seccompProfile"].is_object() {
            annotation_for_field(&psc["seccompProfile"])
        } else {
            self.seccomp_allowed.first().cloned().unwrap_or_default()
        };
        if !profile.is_empty() {
            if !pod["metadata"].is_object() {
                pod["metadata"] = json!({});
            }
            if !pod["metadata"]["annotations"].is_object() {
                pod["metadata"]["annotations"] = json!({});
            }
            pod["metadata"]["annotations"][POD_SECCOMP] = json!(profile);
            match field_for_annotation(&profile) {
                Some(f) => psc["seccompProfile"] = f,
                None => {
                    psc.as_object_mut().unwrap().remove("seccompProfile");
                }
            }
        }
        if existed || psc.as_object().is_some_and(|o| !o.is_empty()) {
            pod["spec"]["securityContext"] = psc;
        }
    }

    fn validate_pod_security_context(&self, pod: &Value, spec_path: &str, errs: &mut Vec<String>) {
        let spec = &pod["spec"];
        let psc = &spec["securityContext"];
        let sc_path = format!("{spec_path}.securityContext");
        let fs: Vec<i64> = psc["fsGroup"].as_i64().into_iter().collect();
        self.fs_group.validate(&sc_path, "fsGroup", &fs, errs);
        let sup: Vec<i64> = psc["supplementalGroups"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
        self.supplemental.validate(&sc_path, "supplementalGroups", &sup, errs);
        // seccomp: the annotation, else the field.
        let mut profile = pod["metadata"]["annotations"][POD_SECCOMP].as_str().unwrap_or("").to_string();
        if profile.is_empty() && psc["seccompProfile"].is_object() {
            profile = annotation_for_field(&psc["seccompProfile"]);
        }
        if let Some(e) = self.validate_profile(&format!("metadata.annotations[{POD_SECCOMP}]"), &profile) {
            errs.push(e);
        }
        self.validate_selinux(&format!("{sc_path}.seLinuxOptions"), &psc["seLinuxOptions"], errs);
        for (k, allow, what) in [
            ("hostNetwork", "allowHostNetwork", "Host network"),
            ("hostPID", "allowHostPID", "Host PID"),
            ("hostIPC", "allowHostIPC", "Host IPC"),
        ] {
            if !self.flag(allow) && spec[k].as_bool() == Some(true) {
                errs.push(invalid(&format!("{spec_path}.{k}"), "true", &format!("{what} is not allowed to be used")));
            }
        }
        if self.scc["userNamespaceLevel"].as_str() == Some("RequirePodLevel") && spec["hostUsers"].as_bool() != Some(false) {
            let v = if spec["hostUsers"].is_boolean() { "true".to_string() } else { NULL.to_string() };
            errs.push(invalid(&format!("{spec_path}.hostUsers"), &v, "Host Users must be set to false"));
        }
        self.validate_sysctls(psc, errs);
        self.validate_volumes(spec, spec_path, errs);
    }

    fn validate_sysctls(&self, psc: &Value, errs: &mut Vec<String>) {
        let matches = |list: &[String], name: &str| {
            list.iter().any(|p| match p.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == p,
            })
        };
        let forbidden_list = strings(&self.scc["forbiddenSysctls"]);
        let allowed = strings(&self.scc["allowedUnsafeSysctls"]);
        for (i, s) in psc["sysctls"].as_array().into_iter().flatten().enumerate() {
            let name = s["name"].as_str().unwrap_or("");
            let path = format!("spec.securityContext.sysctls[{i}]");
            if matches(&forbidden_list, name) {
                errs.push(forbidden(&path, &format!("sysctl {} is not allowed", q(name))));
            } else if SAFE_SYSCTLS.contains(&name) || matches(&allowed, name) {
                continue;
            } else {
                errs.push(forbidden(&path, &format!("unsafe sysctl {} is not allowed", q(name))));
            }
        }
    }

    fn validate_volumes(&self, spec: &Value, spec_path: &str, errs: &mut Vec<String>) {
        let volumes = spec["volumes"].as_array().cloned().unwrap_or_default();
        let allowed = strings(&self.scc["volumes"]);
        let all = allowed.iter().any(|v| v == "*");
        if !volumes.is_empty() && !all {
            for (i, v) in volumes.iter().enumerate() {
                let path = format!("{spec_path}.volumes[{i}]");
                let Some(fs_type) = volume_type(v) else {
                    errs.push(invalid(&path, "\"\"", &format!("unknown volume type for volume: {v}")));
                    continue;
                };
                let ok = allowed.iter().any(|a| a == fs_type)
                    || (allowed.iter().any(|a| a == "secret")
                        && fs_type == "projected"
                        && only_service_account_token_sources(&v["projected"]));
                if !ok {
                    errs.push(invalid(&path, &q(fs_type), &format!("{fs_type} volumes are not allowed to be used")));
                }
            }
        }
        let drivers: Vec<String> =
            self.scc["allowedFlexVolumes"].as_array().into_iter().flatten().filter_map(|d| d["driver"].as_str().map(str::to_string)).collect();
        if !volumes.is_empty() && !drivers.is_empty() && (all || allowed.iter().any(|a| a == "flexVolume")) {
            for (i, v) in volumes.iter().enumerate() {
                let Some(driver) = v["flexVolume"]["driver"].as_str().or(v["flexVolume"].is_object().then_some("")) else {
                    continue;
                };
                if !drivers.iter().any(|d| d == driver) {
                    errs.push(invalid(
                        &format!("{spec_path}.volumes[{i}].flexVolume.driver"),
                        &q(driver),
                        "Flexvolume driver is not allowed to be used",
                    ));
                }
            }
        }
    }

    fn generate_capabilities(&self, current: &Value) -> Value {
        let set = |v: &Value| -> BTreeSet<String> { strings(v).into_iter().collect() };
        let mut default_add = set(&self.scc["defaultAddCapabilities"]);
        let required_drop = set(&self.scc["requiredDropCapabilities"]);
        let container_add = set(&current["add"]);
        let container_drop = set(&current["drop"]);
        default_add.retain(|c| !container_drop.contains(c));
        let add: BTreeSet<String> = default_add.union(&container_add).cloned().collect();
        let drop: BTreeSet<String> = required_drop.union(&container_drop).cloned().collect();
        if add.len() == container_add.len() && drop.len() == container_drop.len() {
            return current.clone();
        }
        let mut caps = json!({});
        if !add.is_empty() {
            caps["add"] = json!(add);
        }
        if !drop.is_empty() {
            caps["drop"] = json!(drop);
        }
        caps
    }

    fn create_container_security_context(&self, psc: &Value, annotations: &Value, c: &mut Value) {
        let existed = c["securityContext"].is_object();
        let mut sc = if existed { c["securityContext"].clone() } else { json!({}) };
        // The effective value: the container's, else the pod's; a set
        // changes the container only when the effective value differs.
        let effective = |sc: &Value, k: &str| if sc[k].is_null() { psc[k].clone() } else { sc[k].clone() };
        let set_effective = |sc: &mut Value, k: &str, v: Value| {
            if effective(sc, k) != v {
                sc[k] = v;
            }
        };
        if effective(&sc, "runAsUser").is_null() {
            let uid = match self.run_as_user {
                UserStrategy::MustRunAs(u) => Some(u),
                UserStrategy::Range(min, _) => Some(min),
                _ => None,
            };
            if let Some(u) = uid {
                set_effective(&mut sc, "runAsUser", json!(u));
            }
        }
        if effective(&sc, "seLinuxOptions").is_null() {
            if let Some(o) = &self.selinux {
                set_effective(&mut sc, "seLinuxOptions", o.clone());
            }
        }
        if effective(&sc, "runAsNonRoot").is_null() {
            let non_root = match effective(&sc, "runAsUser").as_i64() {
                None => matches!(self.run_as_user, UserStrategy::NonRoot),
                Some(u) => u > 0,
            };
            if non_root {
                set_effective(&mut sc, "runAsNonRoot", json!(true));
            }
        }
        let caps = self.generate_capabilities(&sc["capabilities"]);
        if caps.is_null() {
            sc.as_object_mut().unwrap().remove("capabilities");
        } else {
            sc["capabilities"] = caps;
        }
        if self.flag("readOnlyRootFilesystem") && sc["readOnlyRootFilesystem"].is_null() {
            sc["readOnlyRootFilesystem"] = json!(true);
        }
        let privileged = sc["privileged"].as_bool() == Some(true);
        let sys_admin = strings(&sc["capabilities"]["add"]).iter().any(|c| c == "CAP_SYS_ADMIN");
        let name = c["name"].as_str().unwrap_or("");
        if let Some(profile) = annotations[format!("{CONTAINER_SECCOMP}{name}")].as_str() {
            match field_for_annotation(profile) {
                Some(f) => sc["seccompProfile"] = f,
                None => {
                    sc.as_object_mut().unwrap().remove("seccompProfile");
                }
            }
        }
        if let Some(d) = self.scc["defaultAllowPrivilegeEscalation"].as_bool() {
            if sc["allowPrivilegeEscalation"].is_null() && !privileged && !sys_admin {
                sc["allowPrivilegeEscalation"] = json!(d);
            }
        }
        if self.scc["allowPrivilegeEscalation"].as_bool() == Some(false) && sc["allowPrivilegeEscalation"].is_null() {
            sc["allowPrivilegeEscalation"] = json!(false);
        }
        if existed || sc.as_object().is_some_and(|o| !o.is_empty()) {
            c["securityContext"] = sc;
        }
    }

    fn validate_container_security_context(&self, pod: &Value, field: &str, i: usize, path: &str, errs: &mut Vec<String>) {
        let spec = &pod["spec"];
        let psc = &spec["securityContext"];
        let c = &spec[field][i];
        let sc = &c["securityContext"];
        let effective = |k: &str| if sc[k].is_null() { psc[k].clone() } else { sc[k].clone() };
        let uid = effective("runAsUser").as_i64();
        let non_root = effective("runAsNonRoot").as_bool();
        match self.run_as_user {
            UserStrategy::MustRunAs(want) => match uid {
                None => errs.push(required(&format!("{path}.runAsUser"), "")),
                Some(u) if u != want => errs.push(invalid(&format!("{path}.runAsUser"), &u.to_string(), &format!("must be: {want}"))),
                _ => {}
            },
            UserStrategy::Range(min, max) => match uid {
                None => errs.push(required(&format!("{path}.runAsUser"), "")),
                Some(u) if u < min || u > max => errs.push(invalid(
                    &format!("{path}.runAsUser"),
                    &u.to_string(),
                    &format!("must be in the ranges: [{min}, {max}]"),
                )),
                _ => {}
            },
            UserStrategy::NonRoot => {
                if non_root.is_none() && uid.is_none() {
                    errs.push(required(&format!("{path}.runAsNonRoot"), "must be true"));
                } else if non_root == Some(false) {
                    errs.push(invalid(&format!("{path}.runAsNonRoot"), "false", "must be true"));
                } else if uid == Some(0) {
                    errs.push(invalid(&format!("{path}.runAsUser"), "0", "running with the root UID is forbidden"));
                }
            }
            UserStrategy::Any => {}
        }
        self.validate_selinux(&format!("{path}.seLinuxOptions"), &effective("seLinuxOptions"), errs);
        // seccomp: the container's field, its annotation, the pod's field,
        // the pod's annotation.
        let name = c["name"].as_str().unwrap_or("");
        let key = format!("{CONTAINER_SECCOMP}{name}");
        let profile = if sc["seccompProfile"].is_object() {
            annotation_for_field(&sc["seccompProfile"])
        } else if let Some(a) = pod["metadata"]["annotations"][&key].as_str() {
            a.to_string()
        } else if psc["seccompProfile"].is_object() {
            annotation_for_field(&psc["seccompProfile"])
        } else {
            pod["metadata"]["annotations"][POD_SECCOMP].as_str().unwrap_or("").to_string()
        };
        if let Some(e) = self.validate_profile(&format!("metadata.annotations[{key}]"), &profile) {
            errs.push(e);
        }
        if !self.flag("allowPrivilegedContainer") && sc["privileged"].as_bool() == Some(true) {
            errs.push(invalid(&format!("{path}.privileged"), "true", "Privileged containers are not allowed"));
        }
        self.validate_capabilities(path, &sc["capabilities"], errs);
        if !self.flag("allowHostNetwork") && spec["hostNetwork"].as_bool() == Some(true) {
            errs.push(invalid(&format!("{path}.hostNetwork"), "true", "Host network is not allowed to be used"));
        }
        if !self.flag("allowHostPorts") {
            // Every container's ports under this container's path, as
            // upstream visits them.
            for (f, j) in container_slots(pod) {
                for p in spec[f][j]["ports"].as_array().into_iter().flatten() {
                    if let Some(port) = p["hostPort"].as_i64().filter(|p| *p > 0) {
                        errs.push(invalid(
                            &format!("{path}.{f}[{j}].hostPort"),
                            &port.to_string(),
                            "Host ports are not allowed to be used",
                        ));
                    }
                }
            }
        }
        for (k, allow, what) in [("hostPID", "allowHostPID", "Host PID"), ("hostIPC", "allowHostIPC", "Host IPC")] {
            if !self.flag(allow) && spec[k].as_bool() == Some(true) {
                errs.push(invalid(&format!("{path}.{k}"), "true", &format!("{what} is not allowed to be used")));
            }
        }
        if self.flag("readOnlyRootFilesystem") {
            match sc["readOnlyRootFilesystem"].as_bool() {
                None => errs.push(invalid(
                    &format!("{path}.readOnlyRootFilesystem"),
                    NULL,
                    "ReadOnlyRootFilesystem may not be nil and must be set to true",
                )),
                Some(false) => errs.push(invalid(
                    &format!("{path}.readOnlyRootFilesystem"),
                    "false",
                    "ReadOnlyRootFilesystem must be set to true",
                )),
                Some(true) => {}
            }
        }
        if self.scc["allowPrivilegeEscalation"].as_bool() == Some(false) {
            let detail = "Allowing privilege escalation for containers is not allowed";
            match sc["allowPrivilegeEscalation"].as_bool() {
                None => errs.push(invalid(&format!("{path}.allowPrivilegeEscalation"), NULL, detail)),
                Some(true) => errs.push(invalid(&format!("{path}.allowPrivilegeEscalation"), "true", detail)),
                Some(false) => {}
            }
        }
    }

    fn validate_capabilities(&self, path: &str, caps: &Value, errs: &mut Vec<String>) {
        let default_add = strings(&self.scc["defaultAddCapabilities"]);
        let required_drop = strings(&self.scc["requiredDropCapabilities"]);
        if !caps.is_object() {
            if !default_add.is_empty() || !required_drop.is_empty() {
                errs.push(invalid(
                    &format!("{path}.capabilities"),
                    NULL,
                    "required capabilities are not set on the securityContext",
                ));
            }
            return;
        }
        let allowed = strings(&self.scc["allowedCapabilities"]);
        if allowed.iter().any(|c| c == "*") {
            return;
        }
        for cap in strings(&caps["add"]) {
            if !default_add.contains(&cap) && !allowed.contains(&cap) {
                errs.push(invalid(&format!("{path}.capabilities.add"), &q(&cap), "capability may not be added"));
            }
        }
        let drops = strings(&caps["drop"]);
        for d in &required_drop {
            if !drops.contains(d) {
                errs.push(invalid(
                    &format!("{path}.capabilities.drop"),
                    &capabilities_repr(&drops),
                    &format!("{d} is required to be dropped but was not found"),
                ));
            }
        }
    }
}

/// Init containers, containers, ephemeral containers, as upstream visits them.
fn container_slots(pod: &Value) -> Vec<(&'static str, usize)> {
    let mut out = Vec::new();
    for field in ["initContainers", "containers", "ephemeralContainers"] {
        let n = pod["spec"][field].as_array().map_or(0, Vec::len);
        out.extend((0..n).map(|i| (field, i)));
    }
    out
}

fn volume_type(v: &Value) -> Option<&'static str> {
    VOLUME_FIELDS.iter().find(|(field, _)| v[*field].is_object()).map(|(_, t)| *t)
}

/// `IsOnlyServiceAccountTokenSources`: the projected volume every pod gets
/// for its token (#102), which a `secret`-only SCC admits.
fn only_service_account_token_sources(projected: &Value) -> bool {
    for s in projected["sources"].as_array().into_iter().flatten() {
        let (token, cm, down) = (&s["serviceAccountToken"], &s["configMap"], &s["downwardAPI"]);
        if token.is_null() && cm.is_null() && down.is_null() {
            return false;
        }
        if token.is_object()
            && (token["path"].as_str() != Some("token") || token["audience"].as_str().is_some_and(|a| !a.is_empty()))
        {
            return false;
        }
        if cm.is_object() {
            let items = cm["items"].as_array().cloned().unwrap_or_default();
            let want = match cm["name"].as_str() {
                Some("kube-root-ca.crt") => "ca.crt",
                Some("openshift-service-ca.crt") => "service-ca.crt",
                _ => return false,
            };
            if items.len() != 1 || items[0]["key"].as_str() != Some(want) || items[0]["path"].as_str() != Some(want) {
                return false;
            }
        }
        for d in down["items"].as_array().into_iter().flatten() {
            if d["path"].as_str() != Some("namespace")
                || d["fieldRef"]["apiVersion"].as_str() != Some("v1")
                || d["fieldRef"]["fieldPath"].as_str() != Some("metadata.namespace")
            {
                return false;
            }
        }
    }
    true
}

fn annotation_for_field(profile: &Value) -> String {
    match profile["type"].as_str() {
        Some("Unconfined") => "unconfined".into(),
        Some("RuntimeDefault") => "runtime/default".into(),
        Some("Localhost") => profile["localhostProfile"].as_str().map(|p| format!("localhost/{p}")).unwrap_or_default(),
        _ => String::new(),
    }
}

fn field_for_annotation(annotation: &str) -> Option<Value> {
    match annotation {
        "unconfined" => Some(json!({"type": "Unconfined"})),
        "runtime/default" | "docker/default" => Some(json!({"type": "RuntimeDefault"})),
        a => a
            .strip_prefix("localhost/")
            .filter(|p| !p.is_empty())
            .map(|p| json!({"type": "Localhost", "localhostProfile": p})),
    }
}

fn equal_levels(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    let (Some((es, ec)), Some((as_, ac))) = (expected.split_once(':'), actual.split_once(':')) else {
        return false;
    };
    let cats = |c: &str| {
        let mut v: Vec<&str> = c.split(',').collect();
        v.sort();
        v
    };
    es == as_ && cats(ec) == cats(ac)
}

/// A reference to an SCC, as `reference.GetReference` makes one.
pub fn reference(scc: &Value) -> Value {
    let mut r = json!({
        "kind": "SecurityContextConstraints",
        "apiVersion": "security.openshift.io/v1",
        "name": scc["metadata"]["name"],
    });
    for k in ["uid", "resourceVersion"] {
        if let Some(v) = scc["metadata"][k].as_str() {
            r[k] = json!(v);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> Value {
        json!({"metadata": {"name": "app", "annotations": {
            crate::scc::UID_RANGE: "1000680000/10000",
            crate::scc::SUPPLEMENTAL_GROUPS: "1000680000/10000",
        }}})
    }

    fn named(name: &str) -> Value {
        builtins().into_iter().find(|s| s["metadata"]["name"] == name).unwrap()
    }

    #[test]
    fn the_builtins_are_valid_and_their_own_defaults() {
        for scc in builtins() {
            let mut d = scc.clone();
            defaults(&mut d);
            assert_eq!(d, scc, "{}", scc["metadata"]["name"]);
            validate(&scc).unwrap();
            Provider::new(&ns(), &scc).unwrap();
        }
    }

    #[test]
    fn sccs_sort_by_priority_then_restrictiveness_then_name() {
        let mut all = builtins();
        all.reverse();
        sort(&mut all);
        let names: Vec<&str> = all.iter().map(|s| s["metadata"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["anyuid", "restricted-v2", "privileged"]);
        // OpenShift's points for the built-ins.
        assert_eq!(points(&named("restricted-v2")), 100_000 + 2010 + 10_000 + 20_000);
        assert_eq!(points(&named("anyuid")), 100_000 + 4950 + 10_000 + 40_000);
        // A custom SCC of the same priority as restricted-v2 that is more
        // restrictive (MustRunAs one UID) sorts before it.
        let mut one = named("restricted-v2");
        one["metadata"]["name"] = json!("aaa-one-uid");
        one["runAsUser"] = json!({"type": "MustRunAs", "uid": 1000680000});
        let mut sccs = vec![named("restricted-v2"), one];
        sort(&mut sccs);
        assert_eq!(sccs[0]["metadata"]["name"], "aaa-one-uid");
    }

    #[test]
    fn defaults_resolve_volumes_as_openshift() {
        let mut s = json!({"metadata": {"name": "x"}, "runAsUser": {"type": "RunAsAny"}, "seLinuxContext": {"type": "RunAsAny"}});
        defaults(&mut s);
        let v = strings(&s["volumes"]);
        assert!(!v.contains(&"hostPath".to_string()) && v.contains(&"nfs".to_string()) && v.len() == 29, "{v:?}");
        assert_eq!(s["fsGroup"]["type"], "RunAsAny");
        assert_eq!(s["allowPrivilegeEscalation"], true);
        assert_eq!(s["users"], json!([]));
        let mut s = json!({"metadata": {"name": "x"}, "volumes": [], "allowHostDirVolumePlugin": true});
        defaults(&mut s);
        assert_eq!(s["volumes"], json!(["hostPath"]));
        let mut s = json!({"metadata": {"name": "x"}, "volumes": []});
        defaults(&mut s);
        assert_eq!(s["volumes"], json!(["none"]));
    }

    #[test]
    fn validation_refuses_what_openshift_refuses() {
        let msg = |f: &dyn Fn(&mut Value)| {
            let mut s = named("restricted-v2");
            f(&mut s);
            validate(&s).unwrap_err().message
        };
        assert!(msg(&|s| s["priority"] = json!(-1)).contains("priority: Invalid value: -1: priority cannot be negative"));
        assert!(msg(&|s| s["runAsUser"]["type"] = json!("Bogus")).contains("runAsUser.type: Invalid value: \"Bogus\""));
        assert!(msg(&|s| s["fsGroup"]["type"] = json!("X")).contains("fsGroup.type: Unsupported value: \"X\""));
        assert!(msg(&|s| s["allowedCapabilities"] = json!(["ALL"]))
            .contains("allowedCapabilities: Invalid value: \"ALL\": capability is listed in allowedCapabilities and requiredDropCapabilities"));
        assert!(msg(&|s| s["volumes"] = json!(["none", "secret"])).contains("if 'none' is specified"));
        let mut ok = named("restricted-v2");
        ok["allowedUnsafeSysctls"] = json!(["kernel.msg*"]);
        validate(&ok).unwrap();
        assert!(msg(&|s| {
            s["allowedUnsafeSysctls"] = json!(["kernel.msg*"]);
            s["forbiddenSysctls"] = json!(["kernel.msgmax"]);
        })
        .contains("forbiddenSysctls[0]: Invalid value: \"kernel.msgmax\": sysctl overlaps with kernel.msg*"));
        assert!(msg(&|s| s["metadata"]["name"] = json!("Bad_Name")).contains("metadata.name: Invalid value: \"Bad_Name\""));
    }

    fn assign(scc: &str, mut pod: Value) -> (Value, Vec<String>) {
        let p = Provider::new(&ns(), &named(scc)).unwrap();
        let errs = p.assign(&mut pod);
        (pod, errs)
    }

    #[test]
    fn restricted_v2_fills_in_an_openshift_manifest() {
        let (p, errs) = assign("restricted-v2", json!({"metadata": {"name": "web"}, "spec": {"containers": [{"name": "app"}]}}));
        assert!(errs.is_empty(), "{errs:?}");
        let c = &p["spec"]["containers"][0]["securityContext"];
        assert_eq!(c["runAsUser"], 1_000_680_000);
        assert_eq!(c["runAsNonRoot"], true);
        assert_eq!(c["allowPrivilegeEscalation"], false);
        assert_eq!(c["capabilities"], json!({"drop": ["ALL"]}));
        assert_eq!(p["spec"]["securityContext"]["fsGroup"], 1_000_680_000);
        assert_eq!(p["spec"]["securityContext"]["seccompProfile"], json!({"type": "RuntimeDefault"}));
        assert_eq!(p["metadata"]["annotations"][POD_SECCOMP], "runtime/default");
        assert!(p["spec"]["securityContext"]["supplementalGroups"].is_null(), "RunAsAny");
    }

    #[test]
    fn only_the_first_group_of_the_range_is_an_fs_group() {
        let (_, errs) = assign("restricted-v2", json!({"spec": {"securityContext": {"fsGroup": 1_000_680_001}, "containers": [{"name": "a"}]}}));
        assert_eq!(
            errs,
            ["provider restricted-v2: .spec.securityContext.fsGroup: Invalid value: []int64{1000680001}: 1000680001 is not an allowed group"]
        );
    }

    #[test]
    fn refusals_carry_openshifts_paths() {
        let (_, errs) = assign("restricted-v2", json!({"spec": {"hostNetwork": true,
            "volumes": [{"name": "h", "hostPath": {"path": "/"}}],
            "containers": [{"name": "a", "ports": [{"containerPort": 80, "hostPort": 80}],
                "securityContext": {"runAsUser": 0, "privileged": true, "allowPrivilegeEscalation": true,
                                    "capabilities": {"add": ["NET_ADMIN", "NET_BIND_SERVICE"]}}}]}}));
        for want in [
            "provider restricted-v2: .spec.hostNetwork: Invalid value: true: Host network is not allowed to be used",
            "provider restricted-v2: .spec.volumes[0]: Invalid value: \"hostPath\": hostPath volumes are not allowed to be used",
            "provider restricted-v2: .containers[0].runAsUser: Invalid value: 0: must be in the ranges: [1000680000, 1000689999]",
            "provider restricted-v2: .containers[0].privileged: Invalid value: true: Privileged containers are not allowed",
            "provider restricted-v2: .containers[0].capabilities.add: Invalid value: \"NET_ADMIN\": capability may not be added",
            "provider restricted-v2: .containers[0].hostNetwork: Invalid value: true: Host network is not allowed to be used",
            "provider restricted-v2: .containers[0].containers[0].hostPort: Invalid value: 80: Host ports are not allowed to be used",
            "provider restricted-v2: .containers[0].allowPrivilegeEscalation: Invalid value: true: Allowing privilege escalation for containers is not allowed",
        ] {
            assert!(errs.iter().any(|e| e == want), "{want}\nnot in {errs:#?}");
        }
        assert!(!errs.iter().any(|e| e.contains("NET_BIND_SERVICE")));
    }

    #[test]
    fn seccomp_is_held_to_the_scc() {
        let (_, errs) = assign("restricted-v2", json!({"spec": {"securityContext": {"seccompProfile": {"type": "Unconfined"}}, "containers": [{"name": "a"}]}}));
        assert!(errs.contains(&format!(
            "metadata.annotations[{POD_SECCOMP}]: Forbidden: unconfined is not an allowed seccomp profile. Valid values are [runtime/default]"
        )), "{errs:?}");
        // anyuid names no profiles: none may be set.
        let (_, errs) = assign("anyuid", json!({"spec": {"securityContext": {"seccompProfile": {"type": "RuntimeDefault"}}, "containers": [{"name": "a"}]}}));
        assert!(errs.contains(&format!("metadata.annotations[{POD_SECCOMP}]: Forbidden: seccomp may not be set")), "{errs:?}");
    }

    #[test]
    fn anyuid_drops_mknod_and_keeps_the_uid() {
        let (p, errs) = assign("anyuid", json!({"spec": {"containers": [{"name": "a", "securityContext": {"runAsUser": 0}}]}}));
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(p["spec"]["containers"][0]["securityContext"], json!({"runAsUser": 0, "capabilities": {"drop": ["MKNOD"]}}));
    }

    #[test]
    fn privileged_admits_anything_and_changes_nothing() {
        let pod = json!({"spec": {"hostNetwork": true, "volumes": [{"name": "h", "hostPath": {"path": "/"}}],
            "securityContext": {"sysctls": [{"name": "kernel.msgmax", "value": "1"}]},
            "containers": [{"name": "a", "securityContext": {"privileged": true, "capabilities": {"add": ["SYS_ADMIN"]}}}]}});
        let (p, errs) = assign("privileged", pod.clone());
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(p["spec"], pod["spec"]);
        // restricted-v2 refuses the unsafe sysctl.
        let (_, errs) = assign("restricted-v2", pod);
        assert!(errs.contains(&"spec.securityContext.sysctls[0]: Forbidden: unsafe sysctl \"kernel.msgmax\" is not allowed".to_string()));
    }

    #[test]
    fn non_root_and_must_run_as_strategies() {
        let mut s = named("restricted-v2");
        s["metadata"]["name"] = json!("nonroot");
        s["runAsUser"] = json!({"type": "MustRunAsNonRoot"});
        let p = Provider::new(&ns(), &s).unwrap();
        let mut pod = json!({"spec": {"containers": [{"name": "a"}]}});
        assert!(p.assign(&mut pod).is_empty());
        assert_eq!(pod["spec"]["containers"][0]["securityContext"]["runAsNonRoot"], true);
        let mut pod = json!({"spec": {"containers": [{"name": "a", "securityContext": {"runAsUser": 0}}]}});
        let errs = p.assign(&mut pod);
        assert!(errs.contains(&"provider nonroot: .containers[0].runAsUser: Invalid value: 0: running with the root UID is forbidden".to_string()), "{errs:?}");
        s["runAsUser"] = json!({"type": "MustRunAs"});
        assert_eq!(Provider::new(&ns(), &s).unwrap_err(), "error creating provider for SCC nonroot in namespace app: MustRunAs requires a UID");
        let no_ranges = json!({"metadata": {"name": "bare"}});
        assert!(Provider::new(&no_ranges, &named("restricted-v2")).unwrap_err().contains("unable to find pre-allocated uid annotation"));
    }

    #[test]
    fn the_token_volume_is_a_secret_volume() {
        let token = json!({"name": "kube-api-access", "projected": {"sources": [
            {"serviceAccountToken": {"path": "token", "expirationSeconds": 3607}},
            {"configMap": {"name": "kube-root-ca.crt", "items": [{"key": "ca.crt", "path": "ca.crt"}]}},
            {"downwardAPI": {"items": [{"path": "namespace", "fieldRef": {"apiVersion": "v1", "fieldPath": "metadata.namespace"}}]}}]}});
        let mut s = named("restricted-v2");
        s["metadata"]["name"] = json!("secrets-only");
        s["volumes"] = json!(["secret"]);
        let p = Provider::new(&ns(), &s).unwrap();
        let mut pod = json!({"spec": {"volumes": [token], "containers": [{"name": "a"}]}});
        assert!(p.assign(&mut pod).is_empty());
        let mut pod = json!({"spec": {"volumes": [{"name": "e", "emptyDir": {}}], "containers": [{"name": "a"}]}});
        assert_eq!(p.assign(&mut pod), ["provider secrets-only: .spec.volumes[0]: Invalid value: \"emptyDir\": emptyDir volumes are not allowed to be used"]);
    }

    #[test]
    fn aggregate_is_bare_for_one_and_distinct() {
        assert_eq!(aggregate(&["a".into()]), "a");
        assert_eq!(aggregate(&["a".into(), "b".into(), "a".into()]), "[a, b]");
    }
}
