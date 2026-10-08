//! API discovery endpoints.
//!
//! Implements /api, /apis, /api/v1, /version, /healthz, /livez, /readyz
//! so kubectl can discover available resources and server capabilities.

use crate::handlers::AppState;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

/// GET /version — server version info (kubectl uses this).
pub async fn version() -> impl IntoResponse {
    Json(json!({
        "major": "1",
        "minor": "36",
        "gitVersion": format!("v1.36.0-rustkube+{}", apimachinery::VERSION),
        "gitCommit": "",
        "gitTreeState": "clean",
        "buildDate": "2026-03-17T00:00:00Z",
        "goVersion": "rustc/1.93.0",
        "compiler": "rustc",
        "platform": std::env::consts::OS.to_owned() + "/" + std::env::consts::ARCH
    }))
}

/// GET /healthz
pub async fn healthz() -> impl IntoResponse {
    "ok"
}

/// GET /livez
pub async fn livez() -> impl IntoResponse {
    "ok"
}

/// GET /readyz
///
/// Not ready until every stored CRD is registered (#185): before that, each
/// custom resource answers 404, and a client that trusts readiness (Cilium's
/// agent listing CiliumNodes) concludes its own resources do not exist.
pub async fn readyz(State(state): State<AppState>) -> axum::response::Response {
    if state.crd_registry.is_synced() {
        "ok".into_response()
    } else {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "[-]crd-registration failed: stored CustomResourceDefinitions not yet registered\nreadyz check failed\n",
        )
            .into_response()
    }
}

/// GET /api — list core API versions.
pub async fn api_versions(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    if let Some(v) = wants_aggregated(&headers) {
        return aggregated(&state, true, v).await;
    }
    Json(json!({
        "kind": "APIVersions",
        "versions": ["v1"],
        "serverAddressByClientCIDRs": [{
            "clientCIDR": "0.0.0.0/0",
            "serverAddress": ""
        }]
    }))
    .into_response()
}

/// GET /apis — list API groups (includes dynamic CRD groups).
/// The built-in API groups, as `/apis` lists them.
///
/// Extracted so it can be tested. A group whose resources are served but which
/// is missing here is invisible: `oc api-resources` never asks for it, so the
/// resources exist and nothing finds them — which is exactly how the Route
/// group shipped serving `/apis/route.openshift.io/v1` while `/apis` did not
/// mention it.
/// The names of the built-in groups, which no APIService may take over (#83).
pub(crate) fn builtin_group_names() -> std::collections::HashSet<String> {
    builtin_groups().iter().filter_map(|g| g["name"].as_str().map(str::to_string)).collect()
}

/// Built-in, CRD and aggregated groups, as `/apis` lists them. An
/// aggregated version of a group a CRD also serves joins that group.
async fn all_groups(state: &AppState) -> Vec<Value> {
    let mut groups = builtin_groups();
    groups.extend(state.crd_registry.api_groups().await);
    for agg in state.aggregator.groups() {
        match groups.iter_mut().find(|g| g["name"] == agg["name"]) {
            Some(g) => {
                for v in agg["versions"].as_array().into_iter().flatten() {
                    if let Some(list) = g["versions"].as_array_mut() {
                        if !list.iter().any(|x| x["groupVersion"] == v["groupVersion"]) {
                            list.push(v.clone());
                        }
                    }
                }
            }
            None => groups.push(agg),
        }
    }
    groups
}

fn builtin_groups() -> Vec<Value> {
    vec![
        json!({
            "name": "apps",
            "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"}
        }),
        json!({
            "name": "batch",
            "versions": [{"groupVersion": "batch/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "batch/v1", "version": "v1"}
        }),
        json!({
            "name": "rbac.authorization.k8s.io",
            "versions": [{"groupVersion": "rbac.authorization.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "rbac.authorization.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "coordination.k8s.io",
            "versions": [{"groupVersion": "coordination.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "coordination.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "certificates.k8s.io",
            "versions": [{"groupVersion": "certificates.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "certificates.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "discovery.k8s.io",
            "versions": [{"groupVersion": "discovery.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "discovery.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "events.k8s.io",
            "versions": [{"groupVersion": "events.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "events.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "storage.k8s.io",
            "versions": [{"groupVersion": "storage.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "storage.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "resource.k8s.io",
            "versions": [{"groupVersion": "resource.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "resource.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "authorization.k8s.io",
            "versions": [{"groupVersion": "authorization.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "authorization.k8s.io/v1", "version": "v1"}
        }),
        json!({
            // The KubeVirt subresource API. `kubevirt.io/v1` itself is not
            // here and is not meant to be: VirtualMachineInstance arrives as
            // an ordinary CRD from stormpump's manifests, and a client
            // discovers it that way. What a CRD cannot carry is a subresource
            // that is not stored, which is what this group is for (#61).
            "name": "subresources.kubevirt.io",
            "versions": [{"groupVersion": "subresources.kubevirt.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "subresources.kubevirt.io/v1", "version": "v1"}
        }),
        json!({
            "name": "metrics.k8s.io",
            "versions": [{"groupVersion": "metrics.k8s.io/v1beta1", "version": "v1beta1"}],
            "preferredVersion": {"groupVersion": "metrics.k8s.io/v1beta1", "version": "v1beta1"}
        }),
        json!({
            "name": "policy",
            "versions": [{"groupVersion": "policy/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "policy/v1", "version": "v1"}
        }),
        json!({
            "name": "apiextensions.k8s.io",
            "versions": [{"groupVersion": "apiextensions.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "apiextensions.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "autoscaling",
            "versions": [{"groupVersion": "autoscaling/v2", "version": "v2"},
                         {"groupVersion": "autoscaling/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "autoscaling/v2", "version": "v2"}
        }),
        json!({
            "name": "networking.k8s.io",
            "versions": [{"groupVersion": "networking.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "networking.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "admissionregistration.k8s.io",
            "versions": [{"groupVersion": "admissionregistration.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "admissionregistration.k8s.io/v1", "version": "v1"}
        }),
        json!({
            // OpenShift's access reviews (#106): what `oc adm policy who-can`
            // and `oc adm new-project` ask.
            "name": "authorization.openshift.io",
            "versions": [{"groupVersion": "authorization.openshift.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "authorization.openshift.io/v1", "version": "v1"}
        }),
        json!({
            "name": "node.k8s.io",
            "versions": [{"groupVersion": "node.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "node.k8s.io/v1", "version": "v1"}
        }),
        json!({
            // API Priority and Fairness objects (#118): served and stored,
            // not enforced — every request is admitted as before.
            "name": "flowcontrol.apiserver.k8s.io",
            "versions": [{"groupVersion": "flowcontrol.apiserver.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "flowcontrol.apiserver.k8s.io/v1", "version": "v1"}
        }),
        json!({
            // OpenShift's Route, under its upstream group name so `oc get
            // route` and manifests written for OpenShift work unchanged.
            "name": "route.openshift.io",
            "versions": [{"groupVersion": "route.openshift.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "route.openshift.io/v1", "version": "v1"}
        }),
        json!({
            // Projects (#97): Namespaces with owners, under OpenShift's group
            // name so `oc new-project` and `oc projects` find them.
            "name": "project.openshift.io",
            "versions": [{"groupVersion": "project.openshift.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "project.openshift.io/v1", "version": "v1"}
        }),
        json!({
            // PriorityClass (#85): routed and resolved at pod admission, but
            // undiscoverable, so kubectl and helm could not find it.
            "name": "scheduling.k8s.io",
            "versions": [{"groupVersion": "scheduling.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "scheduling.k8s.io/v1", "version": "v1"}
        }),
        json!({
            // TokenReview (#85). The conformance Discovery test looks for it.
            "name": "authentication.k8s.io",
            "versions": [{"groupVersion": "authentication.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "authentication.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "gateway.networking.k8s.io",
            "versions": [{"groupVersion": "gateway.networking.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "gateway.networking.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "apiregistration.k8s.io",
            "versions": [{"groupVersion": "apiregistration.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "apiregistration.k8s.io/v1", "version": "v1"}
        }),
        json!({
            "name": "rustkube.io",
            "versions": [{"groupVersion": "rustkube.io/v1alpha1", "version": "v1alpha1"}],
            "preferredVersion": {"groupVersion": "rustkube.io/v1alpha1", "version": "v1alpha1"}
        }),
    ]
}

pub async fn api_groups_dynamic(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    if let Some(v) = wants_aggregated(&headers) {
        return aggregated(&state, false, v).await;
    }
    let groups = all_groups(&state).await;

    Json(json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": groups
    }))
    .into_response()
}

/// Does `Accept` ask for aggregated discovery (#107)? The version
/// (`v2`, or `v2beta1` for older clients) of the first JSON
/// `as=APIGroupDiscoveryList` entry, unless a plain media type comes first.
/// Protobuf entries are passed over: there is no protobuf schema for it
/// here, and every client lists the JSON form after.
pub(crate) fn wants_aggregated(headers: &axum::http::HeaderMap) -> Option<&'static str> {
    let accept = headers.get(axum::http::header::ACCEPT)?.to_str().ok()?;
    for entry in accept.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let media = parts.next().unwrap_or("");
        let params: Vec<&str> = parts.collect();
        let has = |p: &str| params.iter().any(|x| *x == p);
        if media.contains("protobuf") {
            continue;
        }
        if !has("as=APIGroupDiscoveryList") {
            return None;
        }
        if media == "application/json" && has("g=apidiscovery.k8s.io") {
            if has("v=v2") {
                return Some("v2");
            }
            if has("v=v2beta1") {
                return Some("v2beta1");
            }
        }
    }
    None
}

/// One group-version's discovery `resources` list in aggregated form: each
/// resource with its subresources folded in, `responseKind` and `scope`.
pub(crate) fn to_v2_resources(group: &str, version: &str, list: &[Value]) -> Vec<Value> {
    let kind_of = |e: &Value| json!({
        "group": e["group"].as_str().unwrap_or(group),
        "version": e["version"].as_str().unwrap_or(version),
        "kind": e["kind"],
    });
    let mut out: Vec<Value> = Vec::new();
    for e in list.iter().filter(|e| e["name"].as_str().is_some_and(|n| !n.contains('/'))) {
        let mut r = json!({
            "resource": e["name"],
            "responseKind": kind_of(e),
            "scope": if e["namespaced"].as_bool() == Some(true) { "Namespaced" } else { "Cluster" },
            "singularResource": e["singularName"].as_str().unwrap_or(""),
            "verbs": e["verbs"].clone(),
        });
        for k in ["shortNames", "categories"] {
            if e[k].is_array() {
                r[k] = e[k].clone();
            }
        }
        out.push(r);
    }
    for e in list {
        let Some((parent, sub)) = e["name"].as_str().and_then(|n| n.split_once('/')) else { continue };
        let entry = json!({"subresource": sub, "responseKind": kind_of(e), "verbs": e["verbs"].clone()});
        match out.iter_mut().find(|r| r["resource"] == parent) {
            Some(r) => {
                if !r["subresources"].is_array() {
                    r["subresources"] = json!([]);
                }
                r["subresources"].as_array_mut().unwrap().push(entry);
            }
            // A subresource served without its parent (the KubeVirt doors).
            None => out.push(json!({
                "resource": parent,
                "responseKind": kind_of(e),
                "scope": if e["namespaced"].as_bool() == Some(true) { "Namespaced" } else { "Cluster" },
                "singularResource": "",
                "verbs": [],
                "subresources": [entry],
            })),
        }
    }
    out
}

/// The aggregated discovery document (`apidiscovery.k8s.io` `version`,
/// #107): `/api`'s core group, or every group `/apis` lists — built-in,
/// CRD and aggregated — with every version's resources, from the same
/// lists as `/api/v1`, `/apis/<g>/<v>` and the CRD registry. An aggregated
/// API's versions are listed without resources, `freshness: Stale`, so a
/// client asks the backend's own discovery.
async fn aggregated(state: &AppState, core: bool, version: &str) -> axum::response::Response {
    let lists = builtin_lists().await;
    let mut items = Vec::new();
    if core {
        let list = lists.get(&(String::new(), "v1".into())).and_then(Value::as_array).cloned().unwrap_or_default();
        items.push(json!({"metadata": {"creationTimestamp": null}, "versions": [
            {"version": "v1", "resources": to_v2_resources("", "v1", &list), "freshness": "Current"}]}));
    } else {
        let aggregated_groups: Vec<String> =
            state.aggregator.groups().iter().filter_map(|g| g["name"].as_str().map(str::to_string)).collect();
        for g in all_groups(state).await {
            let name = g["name"].as_str().unwrap_or("").to_string();
            let preferred = g["preferredVersion"]["version"].as_str().unwrap_or("").to_string();
            let mut versions: Vec<String> =
                g["versions"].as_array().into_iter().flatten().filter_map(|v| v["version"].as_str().map(str::to_string)).collect();
            versions.sort_by_key(|v| *v != preferred);
            let mut out = Vec::new();
            for v in versions {
                let mut list = lists.get(&(name.clone(), v.clone())).and_then(Value::as_array).cloned().unwrap_or_default();
                let builtin = !list.is_empty();
                for crd in state.crd_registry.api_resources(&name, &v).await {
                    if !list.iter().any(|e| e["name"] == crd["name"]) {
                        list.push(crd);
                    }
                }
                let stale = list.is_empty() && !builtin && aggregated_groups.contains(&name);
                out.push(json!({"version": v, "resources": to_v2_resources(&name, &v, &list),
                                "freshness": if stale { "Stale" } else { "Current" }}));
            }
            items.push(json!({"metadata": {"name": name, "creationTimestamp": null}, "versions": out}));
        }
    }
    let media = format!("application/json;g=apidiscovery.k8s.io;v={version};as=APIGroupDiscoveryList");
    let body = json!({"kind": "APIGroupDiscoveryList", "apiVersion": format!("apidiscovery.k8s.io/{version}"),
                      "metadata": {}, "items": items});
    (
        [(axum::http::header::CONTENT_TYPE, media), (axum::http::header::VARY, "Accept".to_string())],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response()
}

/// GET /apis/{group} — one group's `APIGroup` document.
///
/// Upstream serves it, and clients use it to find a group's versions without
/// the whole list; the conformance suite fetches `/apis/apiextensions.k8s.io`
/// and got a 404 (#67).
pub async fn api_group(
    State(state): State<AppState>,
    axum::extract::Path(group): axum::extract::Path<String>,
) -> axum::response::Response {
    let groups = all_groups(&state).await;
    match groups.into_iter().find(|g| g["name"] == group.as_str()) {
        Some(mut g) => {
            g["kind"] = json!("APIGroup");
            g["apiVersion"] = json!("v1");
            Json(g).into_response()
        }
        None => crate::error::ApiError {
            status: axum::http::StatusCode::NOT_FOUND,
            reason: "NotFound".into(),
            message: "the server could not find the requested resource".into(),
            continue_token: None,
        }
        .into_response(),
    }
}

/// GET /api/v1 — list core/v1 resources.
pub async fn api_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "v1",
        "resources": [
            {
                "name": "namespaces",
                "singularName": "namespace",
                "namespaced": false,
                "kind": "Namespace",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ns"]
            },
            {
                "name": "nodes",
                "singularName": "node",
                "namespaced": false,
                "kind": "Node",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["no"]
            },
            {
                "name": "nodes/status",
                "singularName": "",
                "namespaced": false,
                "kind": "Node",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "pods",
                "categories": ["all"],
                "singularName": "pod",
                "namespaced": true,
                "kind": "Pod",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["po"]
            },
            {
                "name": "pods/status",
                "singularName": "",
                "namespaced": true,
                "kind": "Pod",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "pods/resize",
                "singularName": "",
                "namespaced": true,
                "kind": "Pod",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "pods/log",
                "singularName": "",
                "namespaced": true,
                "kind": "Pod",
                "verbs": ["get"]
            },
            {
                "name": "services",
                "categories": ["all"],
                "singularName": "service",
                "namespaced": true,
                "kind": "Service",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["svc"]
            },
            {
                "name": "services/status",
                "singularName": "",
                "namespaced": true,
                "kind": "Service",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "endpoints",
                "singularName": "endpoint",
                "namespaced": true,
                "kind": "Endpoints",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ep"]
            },
            {
                "name": "podtemplates",
                "singularName": "podtemplate",
                "namespaced": true,
                "kind": "PodTemplate",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "replicationcontrollers",
                "singularName": "replicationcontroller",
                "namespaced": true,
                "kind": "ReplicationController",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["rc"]
            },
            {
                "name": "replicationcontrollers/scale",
                "singularName": "",
                "namespaced": true,
                "kind": "Scale",
                "group": "autoscaling",
                "version": "v1",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "replicationcontrollers/status",
                "singularName": "",
                "namespaced": true,
                "kind": "ReplicationController",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "resourcequotas",
                "singularName": "resourcequota",
                "namespaced": true,
                "kind": "ResourceQuota",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["quota"]
            },
            {
                "name": "resourcequotas/status",
                "singularName": "",
                "namespaced": true,
                "kind": "ResourceQuota",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "limitranges",
                "singularName": "limitrange",
                "namespaced": true,
                "kind": "LimitRange",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["limits"]
            },
            {
                "name": "configmaps",
                "singularName": "configmap",
                "namespaced": true,
                "kind": "ConfigMap",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["cm"]
            },
            {
                "name": "secrets",
                "singularName": "secret",
                "namespaced": true,
                "kind": "Secret",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "serviceaccounts",
                "singularName": "serviceaccount",
                "namespaced": true,
                "kind": "ServiceAccount",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["sa"]
            },
            {
                "name": "events",
                "singularName": "event",
                "namespaced": true,
                "kind": "Event",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ev"]
            },
            {
                "name": "persistentvolumeclaims",
                "singularName": "persistentvolumeclaim",
                "namespaced": true,
                "kind": "PersistentVolumeClaim",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["pvc"]
            },
            {
                "name": "persistentvolumes",
                "singularName": "persistentvolume",
                "namespaced": false,
                "kind": "PersistentVolume",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["pv"]
            }
        ]
    }))
}

/// GET /apis/apps/v1 — list apps/v1 resources.
pub async fn api_apps_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "apps/v1",
        "resources": [
            {
                "name": "deployments",
                "categories": ["all"],
                "singularName": "deployment",
                "namespaced": true,
                "kind": "Deployment",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["deploy"]
            },
            {
                "name": "deployments/status",
                "singularName": "",
                "namespaced": true,
                "kind": "Deployment",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "deployments/scale",
                "singularName": "",
                "namespaced": true,
                "kind": "Scale",
                "group": "autoscaling",
                "version": "v1",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "replicasets",
                "categories": ["all"],
                "singularName": "replicaset",
                "namespaced": true,
                "kind": "ReplicaSet",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["rs"]
            },
            {
                "name": "replicasets/scale",
                "singularName": "",
                "namespaced": true,
                "kind": "Scale",
                "group": "autoscaling",
                "version": "v1",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "statefulsets",
                "categories": ["all"],
                "singularName": "statefulset",
                "namespaced": true,
                "kind": "StatefulSet",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["sts"]
            },
            {
                "name": "statefulsets/status",
                "singularName": "",
                "namespaced": true,
                "kind": "StatefulSet",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "statefulsets/scale",
                "singularName": "",
                "namespaced": true,
                "kind": "Scale",
                "group": "autoscaling",
                "version": "v1",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "daemonsets",
                "categories": ["all"],
                "singularName": "daemonset",
                "namespaced": true,
                "kind": "DaemonSet",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ds"]
            },
            {
                "name": "daemonsets/status",
                "singularName": "",
                "namespaced": true,
                "kind": "DaemonSet",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

/// GET /apis/batch/v1 — list batch/v1 resources.
pub async fn api_batch_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "batch/v1",
        "resources": [
            {
                "name": "jobs",
                "categories": ["all"],
                "singularName": "job",
                "namespaced": true,
                "kind": "Job",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "jobs/status",
                "singularName": "",
                "namespaced": true,
                "kind": "Job",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "cronjobs",
                "categories": ["all"],
                "singularName": "cronjob",
                "namespaced": true,
                "kind": "CronJob",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["cj"]
            },
            {
                "name": "cronjobs/status",
                "singularName": "",
                "namespaced": true,
                "kind": "CronJob",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

/// GET /apis/coordination.k8s.io/v1 — coordination resources.
pub async fn api_coordination_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "coordination.k8s.io/v1",
        "resources": [
            {
                "name": "leases",
                "singularName": "lease",
                "namespaced": true,
                "kind": "Lease",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            }
        ]
    }))
}

/// GET /apis/discovery.k8s.io/v1 — EndpointSlice resources.
pub async fn api_discovery_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "discovery.k8s.io/v1",
        "resources": [
            {
                "name": "endpointslices",
                "singularName": "endpointslice",
                "namespaced": true,
                "kind": "EndpointSlice",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            }
        ]
    }))
}

/// The resources a built-in group-version advertises (the first segment of
/// each discovery entry's name, so `pods/log` is `pods`), read from its
/// discovery handler, once. None for a group-version not served by one of
/// the handlers below (custom resources, aggregated APIs, events, metrics).
/// The served-resource check (#110, `served.rs`) reads it.
pub async fn advertised(group: &str, version: &str) -> Option<&'static std::collections::HashSet<String>> {
    use std::collections::{HashMap, HashSet};
    static NAMES: tokio::sync::OnceCell<HashMap<(String, String), HashSet<String>>> = tokio::sync::OnceCell::const_new();
    let names = NAMES.get_or_init(|| async {
        let mut m = HashMap::new();
        for ((g, v), list) in builtin_lists().await {
            let n: HashSet<String> = list.as_array().into_iter().flatten()
                .filter_map(|r| r["name"].as_str().map(|n| n.split('/').next().unwrap_or("").to_string()))
                .collect();
            m.insert((g.clone(), v.clone()), n);
        }
        m
    }).await;
    names.get(&(group.to_string(), version.to_string()))
}

/// Every built-in group-version's discovery `resources` list, read from its
/// handler once: the source of [`advertised`] and of aggregated discovery
/// (#107), so neither can disagree with `/apis/<g>/<v>`.
pub(crate) async fn builtin_lists() -> &'static std::collections::BTreeMap<(String, String), Value> {
    use std::collections::BTreeMap;
    static TABLE: tokio::sync::OnceCell<BTreeMap<(String, String), Value>> = tokio::sync::OnceCell::const_new();
    async fn names(r: axum::response::Response) -> Value {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap_or_default();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap_or_default();
        v["resources"].clone()
    }
    TABLE.get_or_init(|| async {
        let mut m = BTreeMap::new();
        macro_rules! gv {
            ($g:expr, $v:expr, $f:expr) => {
                m.insert(($g.to_string(), $v.to_string()), names($f.await.into_response()).await);
            };
        }
        gv!("", "v1", api_v1_resources());
        gv!("apps", "v1", api_apps_v1_resources());
        gv!("batch", "v1", api_batch_v1_resources());
        gv!("coordination.k8s.io", "v1", api_coordination_v1_resources());
        gv!("discovery.k8s.io", "v1", api_discovery_v1_resources());
        gv!("policy", "v1", api_policy_v1_resources());
        gv!("authorization.k8s.io", "v1", api_authorization_v1_resources());
        gv!("subresources.kubevirt.io", "v1", api_kubevirt_subresources_v1_resources());
        gv!("storage.k8s.io", "v1", api_storage_v1_resources());
        gv!("resource.k8s.io", "v1", api_resource_v1_resources());
        gv!("certificates.k8s.io", "v1", api_certificates_v1_resources());
        gv!("rustkube.io", "v1alpha1", api_rustkube_v1alpha1_resources());
        gv!("rbac.authorization.k8s.io", "v1", api_rbac_v1_resources());
        gv!("apiextensions.k8s.io", "v1", api_apiextensions_v1_resources());
        gv!("autoscaling", "v1", api_autoscaling_v1_resources());
        gv!("autoscaling", "v2", api_autoscaling_v2_resources());
        gv!("networking.k8s.io", "v1", api_networking_v1_resources());
        gv!("route.openshift.io", "v1", api_route_v1_resources());
        gv!("scheduling.k8s.io", "v1", api_scheduling_v1_resources());
        gv!("authentication.k8s.io", "v1", api_authentication_v1_resources());
        gv!("project.openshift.io", "v1", api_project_v1_resources());
        gv!("admissionregistration.k8s.io", "v1", api_admissionregistration_v1_resources());
        gv!("node.k8s.io", "v1", api_node_v1_resources());
        gv!("authorization.openshift.io", "v1", api_openshift_authorization_v1_resources());
        gv!("events.k8s.io", "v1", crate::events::discovery());
        gv!("metrics.k8s.io", "v1beta1", crate::resource_metrics::resources());
        gv!("flowcontrol.apiserver.k8s.io", "v1", api_flowcontrol_v1_resources());
        gv!("gateway.networking.k8s.io", "v1", api_gateway_v1_resources());
        gv!("apiregistration.k8s.io", "v1", api_apiregistration_v1_resources());
        m
    }).await
}

/// GET /openapi/v2 — Swagger 2.0 document.
///
/// `kubectl apply` downloads this to validate manifests client-side; a 404
/// aborts the apply with "failed to download openapi" before any write happens.
/// We serve a valid document with no per-type definitions: kubectl finds no
/// schema for the GVK and proceeds without client-side validation (the server
/// is still the authority), instead of failing outright.
///
/// Custom resources are published: one definition per served CRD version,
/// from its schema (#120, `openapi_crd.rs`).
pub async fn openapi_v2(
    axum::extract::State(state): axum::extract::State<crate::handlers::AppState>,
) -> impl IntoResponse {
    let defs = state.crd_registry.all_versions().await;
    Json(json!({
        "swagger": "2.0",
        "info": {
            "title": "Kubernetes",
            "version": format!("v1.36.0-rustkube+{}", apimachinery::VERSION)
        },
        "paths": {},
        "definitions": crate::openapi_crd::v2_definitions(&defs)
    }))
}

/// GET /openapi/v3 — the OpenAPI v3 group-version index kubectl fetches first.
pub async fn openapi_v3(
    axum::extract::State(state): axum::extract::State<crate::handlers::AppState>,
) -> impl IntoResponse {
    // Each entry points at a per-group-version document below.
    let gv = |p: &str| json!({ "serverRelativeURL": format!("/openapi/v3/{p}") });
    let mut index = json!({
        "paths": {
            "api/v1": gv("api/v1"),
            "apis/apps/v1": gv("apis/apps/v1"),
            "apis/batch/v1": gv("apis/batch/v1"),
            "apis/discovery.k8s.io/v1": gv("apis/discovery.k8s.io/v1"),
            "apis/storage.k8s.io/v1": gv("apis/storage.k8s.io/v1"),
            "apis/rbac.authorization.k8s.io/v1": gv("apis/rbac.authorization.k8s.io/v1"),
            "apis/coordination.k8s.io/v1": gv("apis/coordination.k8s.io/v1"),
            "apis/certificates.k8s.io/v1": gv("apis/certificates.k8s.io/v1"),
            "apis/apiextensions.k8s.io/v1": gv("apis/apiextensions.k8s.io/v1")
        }
    });
    // Every CRD group-version (#120).
    for p in crate::openapi_crd::v3_group_versions(&state.crd_registry.all_versions().await) {
        index["paths"][&p] = gv(&p);
    }
    Json(index)
}

/// GET /openapi/v3/{*path} — per-group-version OpenAPI v3 document.
pub async fn openapi_v3_group(
    axum::extract::State(state): axum::extract::State<crate::handlers::AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    // `path` is the group-version as it appears in the index: "api/v1" for the
    // core group, "apis/<group>/<version>" otherwise.
    let (group, version, prefix) = match parse_openapi_path(&path) {
        Some(t) => t,
        None => {
            return Json(json!({
                "openapi": "3.0.0",
                "info": { "title": "Kubernetes",
                          "version": format!("v1.36.0-rustkube+{}", apimachinery::VERSION) },
                "paths": {},
                "components": { "schemas": {} }
            }))
        }
    };

    let mut paths = serde_json::Map::new();
    for (plural, kind, namespaced) in resources_for(&group, &version) {
        let gvk = json!({ "group": group, "version": version, "kind": kind });

        // Collection path (POST creates) and item path (PUT/PATCH update). Both
        // advertise `fieldValidation`, which is the whole point: kubectl looks
        // the GVK up here and, finding the parameter, uses SERVER-side field
        // validation. Without it, it falls back to the legacy protobuf-encoded
        // /openapi/v2 document and `kubectl apply` fails outright (#31).
        let (collection, item) = if namespaced {
            (
                format!("{prefix}/namespaces/{{namespace}}/{plural}"),
                format!("{prefix}/namespaces/{{namespace}}/{plural}/{{name}}"),
            )
        } else {
            (
                format!("{prefix}/{plural}"),
                format!("{prefix}/{plural}/{{name}}"),
            )
        };

        paths.insert(collection, json!({ "post": operation(&gvk) }));
        paths.insert(
            item,
            json!({ "put": operation(&gvk), "patch": operation(&gvk) }),
        );
    }

    // A CRD group-version's resources and schemas (#120).
    let (crd_paths, schemas) = crate::openapi_crd::v3_parts(&state.crd_registry.all_versions().await, &group, &version);
    paths.extend(crd_paths);

    Json(json!({
        "openapi": "3.0.0",
        "info": {
            "title": "Kubernetes",
            "version": format!("v1.36.0-rustkube+{}", apimachinery::VERSION)
        },
        "paths": paths,
        "components": { "schemas": schemas }
    }))
}

/// One OpenAPI operation carrying its GVK and the `fieldValidation` parameter.
pub(crate) fn operation(gvk: &serde_json::Value) -> serde_json::Value {
    json!({
        "x-kubernetes-group-version-kind": gvk,
        "parameters": [{
            "name": "fieldValidation",
            "in": "query",
            "description": "Ignore, Warn or Strict handling of unknown/duplicate fields",
            "schema": { "type": "string", "uniqueItems": true }
        }],
        "responses": { "200": { "description": "OK" } }
    })
}

/// Split an OpenAPI v3 index path into `(group, version, url_prefix)`.
/// `api/v1` → `("", "v1", "/api/v1")`; `apis/apps/v1` → `("apps", "v1", "/apis/apps/v1")`.
fn parse_openapi_path(path: &str) -> Option<(String, String, String)> {
    let p = path.trim_matches('/');
    let parts: Vec<&str> = p.split('/').collect();
    match parts.as_slice() {
        ["api", version] => Some((String::new(), version.to_string(), format!("/api/{version}"))),
        ["apis", group, version] => Some((
            group.to_string(),
            version.to_string(),
            format!("/apis/{group}/{version}"),
        )),
        _ => None,
    }
}

/// `(plural, kind, namespaced)` for a served group-version.
///
/// Only what `kubectl apply` needs to resolve a GVK to a path — the schemas
/// themselves stay empty, so validation happens server-side rather than against
/// a client-side copy of the type.
pub(crate) fn resources_for(group: &str, version: &str) -> Vec<(&'static str, &'static str, bool)> {
    match (group, version) {
        ("", "v1") => vec![
            ("namespaces", "Namespace", false),
            ("nodes", "Node", false),
            ("persistentvolumes", "PersistentVolume", false),
            ("pods", "Pod", true),
            ("services", "Service", true),
            ("endpoints", "Endpoints", true),
            ("configmaps", "ConfigMap", true),
            ("secrets", "Secret", true),
            ("serviceaccounts", "ServiceAccount", true),
            ("events", "Event", true),
            ("persistentvolumeclaims", "PersistentVolumeClaim", true),
        ],
        ("apps", "v1") => vec![
            ("deployments", "Deployment", true),
            ("replicasets", "ReplicaSet", true),
            ("statefulsets", "StatefulSet", true),
            ("daemonsets", "DaemonSet", true),
        ],
        ("batch", "v1") => vec![("jobs", "Job", true), ("cronjobs", "CronJob", true)],
        ("discovery.k8s.io", "v1") => vec![("endpointslices", "EndpointSlice", true)],
        ("coordination.k8s.io", "v1") => vec![("leases", "Lease", true)],
        // Mirrors api_gateway_v1_resources below. Absent, the startup
        // manifest applier — whose resolve() reads this table — refused
        // every Gateway API document with "not served by this apiserver"
        // while the runtime handlers served the same objects happily: the
        // two-lists-that-disagree failure this function's own comment
        // warns about, found when 85-routes.yaml silently did not apply
        // and the router booted with an empty table.
        ("admissionregistration.k8s.io", "v1") => vec![
            ("mutatingwebhookconfigurations", "MutatingWebhookConfiguration", false),
            ("validatingwebhookconfigurations", "ValidatingWebhookConfiguration", false),
            ("validatingadmissionpolicies", "ValidatingAdmissionPolicy", false),
            ("validatingadmissionpolicybindings", "ValidatingAdmissionPolicyBinding", false),
            ("mutatingadmissionpolicies", "MutatingAdmissionPolicy", false),
            ("mutatingadmissionpolicybindings", "MutatingAdmissionPolicyBinding", false),
        ],
        ("node.k8s.io", "v1") => vec![("runtimeclasses", "RuntimeClass", false)],
        ("flowcontrol.apiserver.k8s.io", "v1") => vec![
            ("flowschemas", "FlowSchema", false),
            ("prioritylevelconfigurations", "PriorityLevelConfiguration", false),
        ],
        ("autoscaling", "v1") | ("autoscaling", "v2") => vec![("horizontalpodautoscalers", "HorizontalPodAutoscaler", true)],
        ("networking.k8s.io", "v1") => vec![
            ("networkpolicies", "NetworkPolicy", true),
            ("ingresses", "Ingress", true),
            ("ingressclasses", "IngressClass", false),
            ("servicecidrs", "ServiceCIDR", false),
            ("ipaddresses", "IPAddress", false),
        ],
        ("gateway.networking.k8s.io", "v1") => vec![
            ("gatewayclasses", "GatewayClass", false),
            ("gateways", "Gateway", true),
            ("httproutes", "HTTPRoute", true),
        ],
        ("storage.k8s.io", "v1") => vec![
            ("storageclasses", "StorageClass", false),
            ("csidrivers", "CSIDriver", false),
            ("csinodes", "CSINode", false),
            ("volumeattachments", "VolumeAttachment", false),
            ("csistoragecapacities", "CSIStorageCapacity", true),
            ("volumeattributesclasses", "VolumeAttributesClass", false),
        ],
        ("resource.k8s.io", "v1") => vec![
            ("deviceclasses", "DeviceClass", false),
            ("resourceslices", "ResourceSlice", false),
            ("resourceclaims", "ResourceClaim", true),
            ("resourceclaimtemplates", "ResourceClaimTemplate", true),
        ],
        ("rbac.authorization.k8s.io", "v1") => vec![
            ("clusterroles", "ClusterRole", false),
            ("clusterrolebindings", "ClusterRoleBinding", false),
            ("roles", "Role", true),
            ("rolebindings", "RoleBinding", true),
        ],
        ("certificates.k8s.io", "v1") => {
            vec![("certificatesigningrequests", "CertificateSigningRequest", false)]
        }
        ("apiextensions.k8s.io", "v1") => vec![(
            "customresourcedefinitions",
            "CustomResourceDefinition",
            false,
        )],
        _ => Vec::new(),
    }
}

/// GET /apis/policy/v1 — PodDisruptionBudget + the pod Eviction subresource (#7).
pub async fn api_policy_v1_resources() -> impl IntoResponse {
    let verbs = json!(["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]);
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "policy/v1",
        "resources": [
            {
                "name": "poddisruptionbudgets",
                "singularName": "poddisruptionbudget",
                "namespaced": true,
                "kind": "PodDisruptionBudget",
                "shortNames": ["pdb"],
                "verbs": verbs
            },
            {
                "name": "poddisruptionbudgets/status",
                "singularName": "",
                "namespaced": true,
                "kind": "PodDisruptionBudget",
                "verbs": ["get", "patch", "update"]
            },
            {
                // Eviction is posted to core pods/{name}/eviction, but the kind
                // lives in policy/v1 and clients discover it here.
                "name": "pods/eviction",
                "singularName": "",
                "namespaced": true,
                "group": "policy",
                "version": "v1",
                "kind": "Eviction",
                "verbs": ["create"]
            }
        ]
    }))
}

/// GET /apis/authorization.k8s.io/v1 — the self-review virtual resources (#59).
///
/// `create` is the only verb: these are questions, not objects. A client that
/// cannot find them here will not try them at all, which is why the group has
/// to be in discovery and not merely routed.
pub async fn api_authorization_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "authorization.k8s.io/v1",
        "resources": [
            {
                "name": "selfsubjectaccessreviews",
                "singularName": "selfsubjectaccessreview",
                "namespaced": false,
                "kind": "SelfSubjectAccessReview",
                "verbs": ["create"]
            },
            {
                "name": "selfsubjectrulesreviews",
                "singularName": "selfsubjectrulesreview",
                "namespaced": false,
                "kind": "SelfSubjectRulesReview",
                "verbs": ["create"]
            },
            {
                "name": "subjectaccessreviews",
                "singularName": "subjectaccessreview",
                "namespaced": false,
                "kind": "SubjectAccessReview",
                "verbs": ["create"]
            },
            {
                "name": "localsubjectaccessreviews",
                "singularName": "localsubjectaccessreview",
                "namespaced": true,
                "kind": "LocalSubjectAccessReview",
                "verbs": ["create"]
            }
        ]
    }))
}

/// GET /apis/subresources.kubevirt.io/v1 — the VM console doors (#61).
///
/// `virtctl` looks the group up here before it opens anything, so a group that
/// is routed but not discoverable is one the client will not try.
///
/// The two console doors, the three VirtualMachine lifecycle verbs, and
/// `migrate` on a VM (what `virtctl migrate` calls) or a bare instance,
/// which creates a VirtualMachineInstanceMigration (#184).
///
/// And the VMI control verbs `pause`, `unpause`, `softreboot`, `freeze`,
/// `unfreeze` (#141), proxied to the VMI's kubelet and stormvm.
pub async fn api_kubevirt_subresources_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "subresources.kubevirt.io/v1",
        "resources": [
            {
                "name": "virtualmachineinstances/console",
                "singularName": "",
                "namespaced": true,
                "kind": "VirtualMachineInstance",
                "verbs": ["get"]
            },
            {
                "name": "virtualmachineinstances/vnc",
                "singularName": "",
                "namespaced": true,
                "kind": "VirtualMachineInstance",
                "verbs": ["get"]
            },
            {
                "name": "virtualmachines/start",
                "singularName": "",
                "namespaced": true,
                "kind": "VirtualMachine",
                "verbs": ["update"]
            },
            {
                "name": "virtualmachines/stop",
                "singularName": "",
                "namespaced": true,
                "kind": "VirtualMachine",
                "verbs": ["update"]
            },
            {
                "name": "virtualmachines/restart",
                "singularName": "",
                "namespaced": true,
                "kind": "VirtualMachine",
                "verbs": ["update"]
            },
            {
                "name": "virtualmachines/migrate",
                "singularName": "",
                "namespaced": true,
                "kind": "VirtualMachine",
                "verbs": ["update"]
            },
            {
                "name": "virtualmachineinstances/migrate",
                "singularName": "",
                "namespaced": true,
                "kind": "VirtualMachineInstance",
                "verbs": ["update"]
            },
            {"name": "virtualmachineinstances/pause", "singularName": "", "namespaced": true,
             "kind": "VirtualMachineInstance", "verbs": ["update"]},
            {"name": "virtualmachineinstances/unpause", "singularName": "", "namespaced": true,
             "kind": "VirtualMachineInstance", "verbs": ["update"]},
            {"name": "virtualmachineinstances/softreboot", "singularName": "", "namespaced": true,
             "kind": "VirtualMachineInstance", "verbs": ["update"]},
            {"name": "virtualmachineinstances/freeze", "singularName": "", "namespaced": true,
             "kind": "VirtualMachineInstance", "verbs": ["update"]},
            {"name": "virtualmachineinstances/unfreeze", "singularName": "", "namespaced": true,
             "kind": "VirtualMachineInstance", "verbs": ["update"]}
        ]
    }))
}

/// GET /apis/storage.k8s.io/v1 — CSI ecosystem resources (#24).
///
/// These are plain stored resources: the CSI sidecars (provisioner, attacher,
/// resizer, capacity publisher) create and watch them, and the scheduler reads
/// CSIStorageCapacity. No server-side controller logic is required.
pub async fn api_storage_v1_resources() -> impl IntoResponse {
    let verbs = json!(["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]);
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "storage.k8s.io/v1",
        "resources": [
            {
                "name": "storageclasses",
                "singularName": "storageclass",
                "namespaced": false,
                "kind": "StorageClass",
                "shortNames": ["sc"],
                "verbs": verbs
            },
            {
                "name": "csidrivers",
                "singularName": "csidriver",
                "namespaced": false,
                "kind": "CSIDriver",
                "verbs": verbs
            },
            {
                "name": "csinodes",
                "singularName": "csinode",
                "namespaced": false,
                "kind": "CSINode",
                "verbs": verbs
            },
            {
                "name": "volumeattachments",
                "singularName": "volumeattachment",
                "namespaced": false,
                "kind": "VolumeAttachment",
                "verbs": verbs
            },
            {
                "name": "volumeattachments/status",
                "singularName": "",
                "namespaced": false,
                "kind": "VolumeAttachment",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "csistoragecapacities",
                "singularName": "csistoragecapacity",
                "namespaced": true,
                "kind": "CSIStorageCapacity",
                "verbs": verbs
            },
            {
                "name": "volumeattributesclasses",
                "singularName": "volumeattributesclass",
                "namespaced": false,
                "kind": "VolumeAttributesClass",
                "verbs": verbs,
                "shortNames": ["vac"]
            }
        ]
    }))
}

/// GET /apis/resource.k8s.io/v1 — Dynamic Resource Allocation (#137).
///
/// The objects are served and stored like any other: DeviceClasses and
/// ResourceSlices (a driver publishes a node's devices) cluster-wide,
/// ResourceClaims (with `/status`, where an allocation is written) and
/// ResourceClaimTemplates per namespace. Nothing here allocates: no
/// scheduler plugin, claim-template controller or kubelet plugin API exists
/// yet, so a Pod naming a claim is scheduled as if it named none.
pub async fn api_resource_v1_resources() -> impl IntoResponse {
    let verbs = json!(["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]);
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "resource.k8s.io/v1",
        "resources": [
            {"name": "deviceclasses", "singularName": "deviceclass", "namespaced": false,
             "kind": "DeviceClass", "verbs": verbs},
            {"name": "resourceclaims", "singularName": "resourceclaim", "namespaced": true,
             "kind": "ResourceClaim", "verbs": verbs},
            {"name": "resourceclaims/status", "singularName": "", "namespaced": true,
             "kind": "ResourceClaim", "verbs": ["get", "patch", "update"]},
            {"name": "resourceclaimtemplates", "singularName": "resourceclaimtemplate", "namespaced": true,
             "kind": "ResourceClaimTemplate", "verbs": verbs},
            {"name": "resourceslices", "singularName": "resourceslice", "namespaced": false,
             "kind": "ResourceSlice", "verbs": verbs}
        ]
    }))
}

/// GET /apis/certificates.k8s.io/v1 — CertificateSigningRequest resources.
pub async fn api_certificates_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "certificates.k8s.io/v1",
        "resources": [
            {
                "name": "certificatesigningrequests",
                "singularName": "certificatesigningrequest",
                "namespaced": false,
                "kind": "CertificateSigningRequest",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["csr"]
            },
            {
                "name": "certificatesigningrequests/approval",
                "singularName": "",
                "namespaced": false,
                "kind": "CertificateSigningRequest",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "certificatesigningrequests/status",
                "singularName": "",
                "namespaced": false,
                "kind": "CertificateSigningRequest",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

/// GET /apis/rustkube.io/v1alpha1 — RustKube CRD resources.
pub async fn api_rustkube_v1alpha1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "rustkube.io/v1alpha1",
        "resources": [
            {
                "name": "podmigrations",
                "singularName": "podmigration",
                "namespaced": true,
                "kind": "PodMigration",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["pm"]
            }
        ]
    }))
}

/// GET /apis/rbac.authorization.k8s.io/v1 — RBAC resources.
pub async fn api_rbac_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "rbac.authorization.k8s.io/v1",
        "resources": [
            {
                "name": "clusterroles",
                "singularName": "clusterrole",
                "namespaced": false,
                "kind": "ClusterRole",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "clusterrolebindings",
                "singularName": "clusterrolebinding",
                "namespaced": false,
                "kind": "ClusterRoleBinding",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "roles",
                "singularName": "role",
                "namespaced": true,
                "kind": "Role",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "rolebindings",
                "singularName": "rolebinding",
                "namespaced": true,
                "kind": "RoleBinding",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            }
        ]
    }))
}

/// GET /apis/apiextensions.k8s.io/v1 — CRD management resources.
pub async fn api_apiextensions_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "apiextensions.k8s.io/v1",
        "resources": [
            {
                "name": "customresourcedefinitions",
                "singularName": "customresourcedefinition",
                "namespaced": false,
                "kind": "CustomResourceDefinition",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["crd", "crds"]
            },
            {
                "name": "customresourcedefinitions/status",
                "singularName": "",
                "namespaced": false,
                "kind": "CustomResourceDefinition",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

/// GET /apis/autoscaling/v2 — autoscaling resources.
/// GET /apis/autoscaling/v1 — HorizontalPodAutoscaler v1, a view of v2 (#123).
pub async fn api_autoscaling_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "autoscaling/v1",
        "resources": [
            {
                "name": "horizontalpodautoscalers",
                "singularName": "horizontalpodautoscaler",
                "namespaced": true,
                "kind": "HorizontalPodAutoscaler",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["hpa"],
                "categories": ["all"]
            },
            {
                "name": "horizontalpodautoscalers/status",
                "singularName": "",
                "namespaced": true,
                "kind": "HorizontalPodAutoscaler",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

pub async fn api_autoscaling_v2_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "autoscaling/v2",
        "resources": [
            {
                "name": "horizontalpodautoscalers",
                "categories": ["all"],
                "singularName": "horizontalpodautoscaler",
                "namespaced": true,
                "kind": "HorizontalPodAutoscaler",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["hpa"]
            },
            {
                "name": "horizontalpodautoscalers/status",
                "singularName": "",
                "namespaced": true,
                "kind": "HorizontalPodAutoscaler",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

/// GET /apis/networking.k8s.io/v1 — networking resources.
pub async fn api_networking_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "networking.k8s.io/v1",
        "resources": [
            {
                "name": "networkpolicies",
                "singularName": "networkpolicy",
                "namespaced": true,
                "kind": "NetworkPolicy",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["netpol"]
            },
            {
                "name": "ingresses",
                "singularName": "ingress",
                "namespaced": true,
                "kind": "Ingress",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ing"]
            },
            {
                "name": "ingresses/status",
                "singularName": "",
                "namespaced": true,
                "kind": "Ingress",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "ingressclasses",
                "singularName": "ingressclass",
                "namespaced": false,
                "kind": "IngressClass",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "servicecidrs",
                "singularName": "servicecidr",
                "namespaced": false,
                "kind": "ServiceCIDR",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "servicecidrs/status",
                "singularName": "",
                "namespaced": false,
                "kind": "ServiceCIDR",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "ipaddresses",
                "singularName": "ipaddress",
                "namespaced": false,
                "kind": "IPAddress",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ip"]
            }
        ]
    }))
}

/// GET /apis/route.openshift.io/v1 — OpenShift Routes.
///
/// A Route is "this hostname reaches this service", which is the object the
/// console and every other web service on the node needs in order to be
/// reachable by name rather than by port. See stormpump docs/routing.md for
/// the wildcard domain and VIP it resolves against.
pub async fn api_route_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "route.openshift.io/v1",
        "resources": [
            {
                "name": "routes",
                "singularName": "route",
                "namespaced": true,
                "kind": "Route",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "routes/status",
                "singularName": "",
                "namespaced": true,
                "kind": "Route",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

/// GET /apis/scheduling.k8s.io/v1 — PriorityClass (#85).
pub async fn api_scheduling_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "scheduling.k8s.io/v1",
        "resources": [{
            "name": "priorityclasses",
            "singularName": "priorityclass",
            "namespaced": false,
            "kind": "PriorityClass",
            "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
            "shortNames": ["pc"]
        }]
    }))
}

/// GET /apis/authentication.k8s.io/v1 — TokenReview (#85). Create only: it
/// is a question, not a stored object.
pub async fn api_authentication_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "authentication.k8s.io/v1",
        "resources": [{
            "name": "tokenreviews",
            "singularName": "tokenreview",
            "namespaced": false,
            "kind": "TokenReview",
            "verbs": ["create"]
        }]
    }))
}

/// GET /apis/project.openshift.io/v1 — Projects over Namespaces (#97).
///
/// `projects` has no `patch`: a Project's only writable fields are its labels
/// and annotations, taken by `update`. `projectrequests` is create-only; its
/// `list` answers whether the caller may request one, as upstream's does.
pub async fn api_project_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "project.openshift.io/v1",
        "resources": [
            {
                "name": "projects",
                "singularName": "project",
                "namespaced": false,
                "kind": "Project",
                "verbs": ["create", "delete", "get", "list", "update", "watch"]
            },
            {
                "name": "projectrequests",
                "singularName": "projectrequest",
                "namespaced": false,
                "kind": "ProjectRequest",
                "verbs": ["create", "list"]
            }
        ]
    }))
}

/// GET /apis/admissionregistration.k8s.io/v1 — admission webhook resources.
pub async fn api_admissionregistration_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "admissionregistration.k8s.io/v1",
        "resources": [
            {
                "name": "mutatingwebhookconfigurations",
                "singularName": "mutatingwebhookconfiguration",
                "namespaced": false,
                "kind": "MutatingWebhookConfiguration",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "validatingwebhookconfigurations",
                "singularName": "validatingwebhookconfiguration",
                "namespaced": false,
                "kind": "ValidatingWebhookConfiguration",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "validatingadmissionpolicies",
                "singularName": "validatingadmissionpolicy",
                "namespaced": false,
                "kind": "ValidatingAdmissionPolicy",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "validatingadmissionpolicies/status",
                "singularName": "",
                "namespaced": false,
                "kind": "ValidatingAdmissionPolicy",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "validatingadmissionpolicybindings",
                "singularName": "validatingadmissionpolicybinding",
                "namespaced": false,
                "kind": "ValidatingAdmissionPolicyBinding",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "mutatingadmissionpolicies",
                "singularName": "mutatingadmissionpolicy",
                "namespaced": false,
                "kind": "MutatingAdmissionPolicy",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "mutatingadmissionpolicybindings",
                "singularName": "mutatingadmissionpolicybinding",
                "namespaced": false,
                "kind": "MutatingAdmissionPolicyBinding",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            }
        ]
    }))
}

/// GET /apis/authorization.openshift.io/v1 — OpenShift's access reviews (#106).
pub async fn api_openshift_authorization_v1_resources() -> impl IntoResponse {
    let r = |name: &str, kind: &str, namespaced: bool| json!({
        "name": name, "singularName": "", "namespaced": namespaced, "kind": kind, "verbs": ["create"]
    });
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "authorization.openshift.io/v1",
        "resources": [
            r("subjectaccessreviews", "SubjectAccessReview", false),
            r("localsubjectaccessreviews", "LocalSubjectAccessReview", true),
            r("resourceaccessreviews", "ResourceAccessReview", false),
            r("localresourceaccessreviews", "LocalResourceAccessReview", true),
        ]
    }))
}

/// GET /apis/node.k8s.io/v1 — RuntimeClass (#135).
pub async fn api_node_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "node.k8s.io/v1",
        "resources": [{
            "name": "runtimeclasses", "singularName": "runtimeclass", "namespaced": false, "kind": "RuntimeClass",
            "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
        }]
    }))
}

/// GET /apis/flowcontrol.apiserver.k8s.io/v1 — API Priority and Fairness
/// objects (#118). Stored and served; nothing enforces them.
pub async fn api_flowcontrol_v1_resources() -> impl IntoResponse {
    let mut resources = Vec::new();
    for (name, singular, kind) in [
        ("flowschemas", "flowschema", "FlowSchema"),
        ("prioritylevelconfigurations", "prioritylevelconfiguration", "PriorityLevelConfiguration"),
    ] {
        resources.push(json!({
            "name": name, "singularName": singular, "namespaced": false, "kind": kind,
            "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
        }));
        resources.push(json!({
            "name": format!("{name}/status"), "singularName": "", "namespaced": false, "kind": kind,
            "verbs": ["get", "patch", "update"]
        }));
    }
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "flowcontrol.apiserver.k8s.io/v1",
        "resources": resources
    }))
}

/// GET /apis/gateway.networking.k8s.io/v1 — Gateway API resources.
pub async fn api_gateway_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "gateway.networking.k8s.io/v1",
        "resources": [
            {
                "name": "gatewayclasses",
                "singularName": "gatewayclass",
                "namespaced": false,
                "kind": "GatewayClass",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "gateways",
                "singularName": "gateway",
                "namespaced": true,
                "kind": "Gateway",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "gateways/status",
                "singularName": "",
                "namespaced": true,
                "kind": "Gateway",
                "verbs": ["get", "patch", "update"]
            },
            {
                "name": "httproutes",
                "singularName": "httproute",
                "namespaced": true,
                "kind": "HTTPRoute",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "httproutes/status",
                "singularName": "",
                "namespaced": true,
                "kind": "HTTPRoute",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

/// GET /apis/apiregistration.k8s.io/v1 — API aggregation resources.
pub async fn api_apiregistration_v1_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "apiregistration.k8s.io/v1",
        "resources": [
            {
                "name": "apiservices",
                "singularName": "apiservice",
                "namespaced": false,
                "kind": "APIService",
                "verbs": ["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "apiservices/status",
                "singularName": "",
                "namespaced": false,
                "kind": "APIService",
                "verbs": ["get", "patch", "update"]
            }
        ]
    }))
}

#[cfg(test)]
mod openapi_tests {
    use super::*;

    #[test]
    fn parses_core_and_group_paths() {
        assert_eq!(
            parse_openapi_path("api/v1"),
            Some((String::new(), "v1".into(), "/api/v1".into()))
        );
        assert_eq!(
            parse_openapi_path("apis/apps/v1"),
            Some(("apps".into(), "v1".into(), "/apis/apps/v1".into()))
        );
        // Leading/trailing slashes are tolerated; nonsense is not.
        assert!(parse_openapi_path("/api/v1/").is_some());
        assert!(parse_openapi_path("openapi/v3").is_none());
        assert!(parse_openapi_path("").is_none());
    }

    #[test]
    fn operations_carry_gvk_and_field_validation() {
        // kubectl resolves a GVK to a path via x-kubernetes-group-version-kind,
        // then checks for the fieldValidation parameter. Missing either sends it
        // back to the legacy protobuf /openapi/v2 document (#31).
        let gvk = json!({"group": "", "version": "v1", "kind": "Namespace"});
        let op = operation(&gvk);
        assert_eq!(op["x-kubernetes-group-version-kind"], gvk);
        let params = op["parameters"].as_array().unwrap();
        assert!(params.iter().any(|p| p["name"] == "fieldValidation"
            && p["in"] == "query"));
    }

    #[test]
    fn core_group_covers_namespaced_and_cluster_scoped() {
        let core = resources_for("", "v1");
        assert!(core.contains(&("namespaces", "Namespace", false)));
        assert!(core.contains(&("configmaps", "ConfigMap", true)));
        // Groups we serve resolve; ones we don't stay empty rather than lying.
        assert!(!resources_for("apps", "v1").is_empty());
        assert!(!resources_for("storage.k8s.io", "v1").is_empty());
        assert!(resources_for("nope.example.com", "v1").is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every group whose resources this apiserver serves must appear in
    /// `/apis`. A group that is missing here is invisible to `oc`: discovery
    /// never asks for its resource list, so the resources exist and nothing
    /// finds them. That is exactly how the Route group first shipped —
    /// `/apis/route.openshift.io/v1` answered and `/apis` did not mention it.
    /// What the generic handlers serve has a list kind of its own, never the
    /// `{plural}List` fallback (#110), and the apply table names nothing
    /// discovery does not advertise.
    #[tokio::test]
    async fn every_advertised_generic_resource_has_its_list_kind() {
        let generic = [("", "v1"), ("apps", "v1"), ("batch", "v1"), ("coordination.k8s.io", "v1"),
            ("discovery.k8s.io", "v1"), ("policy", "v1"), ("storage.k8s.io", "v1"), ("resource.k8s.io", "v1"),
            ("certificates.k8s.io", "v1"), ("rbac.authorization.k8s.io", "v1"), ("autoscaling", "v2"),
            ("networking.k8s.io", "v1"), ("scheduling.k8s.io", "v1"), ("admissionregistration.k8s.io", "v1"),
            ("node.k8s.io", "v1"), ("flowcontrol.apiserver.k8s.io", "v1"), ("gateway.networking.k8s.io", "v1")];
        for (g, v) in generic {
            let served = advertised(g, v).await.unwrap_or_else(|| panic!("{g}/{v} not in the table"));
            for r in served {
                let kind = crate::handlers::resource::resource_to_kind(r);
                assert!(kind.chars().next().is_some_and(|c| c.is_ascii_uppercase()), "{g}/{v} {r}: list kind {kind}List");
            }
            for (plural, _, _) in resources_for(g, v) {
                assert!(served.contains(plural), "{g}/{v}: {plural} is in the apply table but not advertised");
            }
        }
        assert!(advertised("", "v1").await.unwrap().contains("replicationcontrollers"));
        assert!(!advertised("", "v1").await.unwrap().contains("replicationcontrollerz"));
        assert!(advertised("example.com", "v1").await.is_none(), "custom groups are not checked here");
    }

    #[test]
    fn aggregated_discovery_is_negotiated_as_kubectl_asks() {
        let h = |a: &str| {
            let mut m = axum::http::HeaderMap::new();
            m.insert(axum::http::header::ACCEPT, a.parse().unwrap());
            wants_aggregated(&m)
        };
        let kubectl = "application/vnd.kubernetes.protobuf;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList,\
                       application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList,\
                       application/json;g=apidiscovery.k8s.io;v=v2beta1;as=APIGroupDiscoveryList,application/json";
        assert_eq!(h(kubectl), Some("v2"));
        assert_eq!(h("application/json;g=apidiscovery.k8s.io;v=v2beta1;as=APIGroupDiscoveryList,application/json"), Some("v2beta1"));
        assert_eq!(h("application/json"), None);
        assert_eq!(h("application/json, application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList"), None, "legacy first");
        assert!(wants_aggregated(&axum::http::HeaderMap::new()).is_none());
    }

    #[test]
    fn a_resource_list_folds_into_the_v2_shape() {
        let list = vec![
            json!({"name": "deployments", "singularName": "deployment", "namespaced": true, "kind": "Deployment",
                   "verbs": ["get", "list"], "shortNames": ["deploy"], "categories": ["all"]}),
            json!({"name": "deployments/scale", "singularName": "", "namespaced": true, "kind": "Scale",
                   "group": "autoscaling", "version": "v1", "verbs": ["get", "update"]}),
            json!({"name": "deployments/status", "singularName": "", "namespaced": true, "kind": "Deployment", "verbs": ["get"]}),
            json!({"name": "virtualmachineinstances/console", "singularName": "", "namespaced": true,
                   "kind": "VirtualMachineInstance", "verbs": ["get"]}),
        ];
        let v2 = to_v2_resources("apps", "v1", &list);
        assert_eq!(v2.len(), 2);
        let d = &v2[0];
        assert_eq!(d["resource"], "deployments");
        assert_eq!(d["responseKind"], json!({"group": "apps", "version": "v1", "kind": "Deployment"}));
        assert_eq!(d["scope"], "Namespaced");
        assert_eq!(d["shortNames"], json!(["deploy"]));
        assert_eq!(d["subresources"][0]["subresource"], "scale");
        assert_eq!(d["subresources"][0]["responseKind"], json!({"group": "autoscaling", "version": "v1", "kind": "Scale"}));
        assert_eq!(d["subresources"][1]["subresource"], "status");
        assert_eq!(v2[1]["resource"], "virtualmachineinstances", "a parent made for a lone subresource");
        assert_eq!(v2[1]["subresources"][0]["subresource"], "console");
    }

    #[test]
    fn every_served_group_is_discoverable() {
        let names: Vec<String> = builtin_groups()
            .iter()
            .map(|g| g["name"].as_str().unwrap_or("").to_string())
            .collect();
        for want in [
            "apps",
            "batch",
            "rbac.authorization.k8s.io",
            "coordination.k8s.io",
            "certificates.k8s.io",
            "discovery.k8s.io",
            "events.k8s.io",
            "storage.k8s.io",
            "resource.k8s.io",
            "apiextensions.k8s.io",
            "networking.k8s.io",
            "admissionregistration.k8s.io",
            "flowcontrol.apiserver.k8s.io",
            "node.k8s.io",
            "authorization.openshift.io",
            "gateway.networking.k8s.io",
            "route.openshift.io",
            "project.openshift.io",
            "scheduling.k8s.io",
            "authentication.k8s.io",
            "apiregistration.k8s.io",
        ] {
            assert!(
                names.contains(&want.to_string()),
                "{want} is served but not in /apis: {names:?}"
            );
        }
    }

    /// `oc get all` expands the `all` category from discovery; with no
    /// resource in it, it fails with `the server doesn't have a resource type
    /// "all"` (#97). The members are upstream's.
    #[tokio::test]
    async fn get_all_has_a_category_to_expand() {
        async fn members(r: impl IntoResponse) -> Vec<String> {
            let body = axum::body::to_bytes(r.into_response().into_body(), usize::MAX).await.unwrap();
            let v: Value = serde_json::from_slice(&body).unwrap();
            v["resources"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["categories"].as_array().is_some_and(|c| c.iter().any(|c| c == "all")))
                .map(|r| r["name"].as_str().unwrap().to_string())
                .collect()
        }
        assert_eq!(members(api_v1_resources().await).await, ["pods", "services"]);
        assert_eq!(
            members(api_apps_v1_resources().await).await,
            ["deployments", "replicasets", "statefulsets", "daemonsets"]
        );
        assert_eq!(members(api_batch_v1_resources().await).await, ["jobs", "cronjobs"]);
        assert_eq!(
            members(api_autoscaling_v2_resources().await).await,
            ["horizontalpodautoscalers"]
        );
    }

    /// A group whose preferredVersion is not among its versions makes clients
    /// pick a version that 404s.
    #[test]
    fn group_entries_are_well_formed() {
        for g in builtin_groups() {
            let name = g["name"].as_str().expect("group has a name");
            let preferred = g["preferredVersion"]["groupVersion"]
                .as_str()
                .unwrap_or_else(|| panic!("{name} has no preferredVersion"));
            let versions: Vec<&str> = g["versions"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} has no versions"))
                .iter()
                .filter_map(|v| v["groupVersion"].as_str())
                .collect();
            assert!(
                versions.contains(&preferred),
                "{name} prefers {preferred}, which is not among {versions:?}"
            );
        }
    }
}
