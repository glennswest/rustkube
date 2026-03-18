//! API discovery endpoints.
//!
//! Implements /api, /apis, /api/v1, /version, /healthz, /livez, /readyz
//! so kubectl can discover available resources and server capabilities.

use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

/// GET /version — server version info (kubectl uses this).
pub async fn version() -> impl IntoResponse {
    Json(json!({
        "major": "1",
        "minor": "32",
        "gitVersion": format!("v1.32.0-rustkube+{}", rk_core::VERSION),
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

/// GET /apis — list API groups.
pub async fn api_groups() -> impl IntoResponse {
    Json(json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": [
            {
                "name": "apps",
                "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
                "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"}
            },
            {
                "name": "rbac.authorization.k8s.io",
                "versions": [{"groupVersion": "rbac.authorization.k8s.io/v1", "version": "v1"}],
                "preferredVersion": {"groupVersion": "rbac.authorization.k8s.io/v1", "version": "v1"}
            },
            {
                "name": "coordination.k8s.io",
                "versions": [{"groupVersion": "coordination.k8s.io/v1", "version": "v1"}],
                "preferredVersion": {"groupVersion": "coordination.k8s.io/v1", "version": "v1"}
            },
            {
                "name": "rustkube.io",
                "versions": [{"groupVersion": "rustkube.io/v1alpha1", "version": "v1alpha1"}],
                "preferredVersion": {"groupVersion": "rustkube.io/v1alpha1", "version": "v1alpha1"}
            }
        ]
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
                "name": "daemonsets",
                "singularName": "daemonset",
                "namespaced": true,
                "kind": "DaemonSet",
                "verbs": ["create", "delete", "get", "list", "patch", "update", "watch"],
                "shortNames": ["ds"]
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
