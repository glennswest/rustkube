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
        "minor": "32",
        "gitVersion": format!("v1.32.0-rustkube+{}", apimachinery::VERSION),
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
