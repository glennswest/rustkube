//! API discovery endpoints.
//!
//! Implements /api, /apis, /api/v1, /version, /healthz, /livez, /readyz
//! so kubectl can discover available resources and server capabilities.

use crate::handlers::AppState;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

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
pub async fn readyz() -> impl IntoResponse {
    "ok"
}

/// GET /api — list core API versions.
pub async fn api_versions() -> impl IntoResponse {
    Json(json!({
        "kind": "APIVersions",
        "versions": ["v1"],
        "serverAddressByClientCIDRs": [{
            "clientCIDR": "0.0.0.0/0",
            "serverAddress": ""
        }]
    }))
}

/// GET /apis — list API groups (includes dynamic CRD groups).
pub async fn api_groups_dynamic(State(state): State<AppState>) -> impl IntoResponse {
    let mut groups = vec![
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
            "name": "storage.k8s.io",
            "versions": [{"groupVersion": "storage.k8s.io/v1", "version": "v1"}],
            "preferredVersion": {"groupVersion": "storage.k8s.io/v1", "version": "v1"}
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
            "versions": [{"groupVersion": "autoscaling/v2", "version": "v2"}],
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
    ];

    // Add dynamically registered CRD groups
    let crd_groups = state.crd_registry.api_groups().await;
    groups.extend(crd_groups);

    Json(json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": groups
    }))
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "name": "pods/log",
                "singularName": "",
                "namespaced": true,
                "kind": "Pod",
                "verbs": ["get"]
            },
            {
                "name": "services",
                "singularName": "service",
                "namespaced": true,
                "kind": "Service",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ep"]
            },
            {
                "name": "configmaps",
                "singularName": "configmap",
                "namespaced": true,
                "kind": "ConfigMap",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["cm"]
            },
            {
                "name": "secrets",
                "singularName": "secret",
                "namespaced": true,
                "kind": "Secret",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "serviceaccounts",
                "singularName": "serviceaccount",
                "namespaced": true,
                "kind": "ServiceAccount",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["sa"]
            },
            {
                "name": "events",
                "singularName": "event",
                "namespaced": true,
                "kind": "Event",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ev"]
            },
            {
                "name": "persistentvolumeclaims",
                "singularName": "persistentvolumeclaim",
                "namespaced": true,
                "kind": "PersistentVolumeClaim",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["pvc"]
            },
            {
                "name": "persistentvolumes",
                "singularName": "persistentvolume",
                "namespaced": false,
                "kind": "PersistentVolume",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "singularName": "deployment",
                "namespaced": true,
                "kind": "Deployment",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "singularName": "replicaset",
                "namespaced": true,
                "kind": "ReplicaSet",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["rs"]
            },
            {
                "name": "statefulsets",
                "singularName": "statefulset",
                "namespaced": true,
                "kind": "StatefulSet",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "name": "daemonsets",
                "singularName": "daemonset",
                "namespaced": true,
                "kind": "DaemonSet",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "singularName": "job",
                "namespaced": true,
                "kind": "Job",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
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
                "singularName": "cronjob",
                "namespaced": true,
                "kind": "CronJob",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            }
        ]
    }))
}

/// GET /openapi/v2 — Swagger 2.0 document.
///
/// `kubectl apply` downloads this to validate manifests client-side; a 404
/// aborts the apply with "failed to download openapi" before any write happens.
/// We serve a valid document with no per-type definitions: kubectl finds no
/// schema for the GVK and proceeds without client-side validation (the server
/// is still the authority), instead of failing outright.
pub async fn openapi_v2() -> impl IntoResponse {
    Json(json!({
        "swagger": "2.0",
        "info": {
            "title": "Kubernetes",
            "version": format!("v1.36.0-rustkube+{}", apimachinery::VERSION)
        },
        "paths": {},
        "definitions": {}
    }))
}

/// GET /openapi/v3 — the OpenAPI v3 group-version index kubectl fetches first.
pub async fn openapi_v3() -> impl IntoResponse {
    // Each entry points at a per-group-version document below.
    let gv = |p: &str| json!({ "serverRelativeURL": format!("/openapi/v3/{p}") });
    Json(json!({
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
    }))
}

/// GET /openapi/v3/{*path} — per-group-version OpenAPI v3 document.
pub async fn openapi_v3_group(
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

    Json(json!({
        "openapi": "3.0.0",
        "info": {
            "title": "Kubernetes",
            "version": format!("v1.36.0-rustkube+{}", apimachinery::VERSION)
        },
        "paths": paths,
        "components": { "schemas": {} }
    }))
}

/// One OpenAPI operation carrying its GVK and the `fieldValidation` parameter.
fn operation(gvk: &serde_json::Value) -> serde_json::Value {
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
fn resources_for(group: &str, version: &str) -> Vec<(&'static str, &'static str, bool)> {
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
        ("storage.k8s.io", "v1") => vec![
            ("storageclasses", "StorageClass", false),
            ("csidrivers", "CSIDriver", false),
            ("csinodes", "CSINode", false),
            ("volumeattachments", "VolumeAttachment", false),
            ("csistoragecapacities", "CSIStorageCapacity", true),
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
            }
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "clusterrolebindings",
                "singularName": "clusterrolebinding",
                "namespaced": false,
                "kind": "ClusterRoleBinding",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "roles",
                "singularName": "role",
                "namespaced": true,
                "kind": "Role",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "rolebindings",
                "singularName": "rolebinding",
                "namespaced": true,
                "kind": "RoleBinding",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
pub async fn api_autoscaling_v2_resources() -> impl IntoResponse {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "autoscaling/v2",
        "resources": [
            {
                "name": "horizontalpodautoscalers",
                "singularName": "horizontalpodautoscaler",
                "namespaced": true,
                "kind": "HorizontalPodAutoscaler",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["netpol"]
            },
            {
                "name": "ingresses",
                "singularName": "ingress",
                "namespaced": true,
                "kind": "Ingress",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "validatingwebhookconfigurations",
                "singularName": "validatingwebhookconfiguration",
                "namespaced": false,
                "kind": "ValidatingWebhookConfiguration",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            }
        ]
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
            },
            {
                "name": "gateways",
                "singularName": "gateway",
                "namespaced": true,
                "kind": "Gateway",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
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
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"]
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
