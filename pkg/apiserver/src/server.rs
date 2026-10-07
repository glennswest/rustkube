//! API server setup and startup.
//!
//! Builds the axum router with all K8s API routes and starts
//! the HTTPS listener.

use crate::auth::{self, SigningKeys};
use crate::config::ApiServerConfig;
use crate::crd::{self, CrdRegistry};
use crate::discovery;
use crate::handlers::resource;
use crate::handlers::AppState;
use crate::rbac_engine::{self, RbacEngine};
use crate::storage::ResourceStorage;
use axum::middleware;
use axum::routing::{get, patch, post, put};
use axum::Router;
use apimachinery::store::KvStore;
use storage::{EtcdStore, EtcdTls};
use serde_json::json;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};

/// Build the complete K8s API router.
fn build_router(
    state: AppState,
    signing_keys: SigningKeys,
    static_tokens: crate::token_file::StaticTokens,
    rbac: Arc<RbacEngine>,
    anonymous_auth: bool,
    prom: metrics_exporter_prometheus::PrometheusHandle,
) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let h = prom.clone();
                async move { apimachinery::metrics::render(&h) }
            }),
        )
        // Discovery & health
        .route("/version", get(discovery::version))
        .route("/healthz", get(discovery::healthz))
        .route("/livez", get(discovery::livez))
        .route("/readyz", get(discovery::readyz))
        .route("/api", get(discovery::api_versions))
        .route("/api/", get(discovery::api_versions))
        // OpenAPI — kubectl apply downloads these to validate manifests;
        // a 404 aborts the apply before any write (#25 follow-up).
        .route("/openapi/v2", get(discovery::openapi_v2))
        .route("/openapi/v3", get(discovery::openapi_v3))
        .route("/openapi/v3/{*path}", get(discovery::openapi_v3_group))
        .route("/apis", get(discovery::api_groups_dynamic))
        .route("/apis/", get(discovery::api_groups_dynamic))
        // metrics.k8s.io from each node's cadvisor (#89), before the CRD
        // catch-alls.
        .route("/apis/metrics.k8s.io/v1beta1", get(crate::resource_metrics::resources))
        .route("/apis/metrics.k8s.io/v1beta1/nodes", get(crate::resource_metrics::list_nodes))
        .route("/apis/metrics.k8s.io/v1beta1/nodes/{name}", get(crate::resource_metrics::get_node))
        .route("/apis/metrics.k8s.io/v1beta1/pods", get(crate::resource_metrics::list_pods_all))
        .route("/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods", get(crate::resource_metrics::list_pods))
        .route("/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods/{name}", get(crate::resource_metrics::get_pod))
        .route("/apis/{group}", get(discovery::api_group))
        .route("/apis/{group}/", get(discovery::api_group))
        .route("/api/v1", get(discovery::api_v1_resources))
        .route("/apis/apps/v1", get(discovery::api_apps_v1_resources))
        .route("/apis/batch/v1", get(discovery::api_batch_v1_resources))
        .route(
            "/apis/coordination.k8s.io/v1",
            get(discovery::api_coordination_v1_resources),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1",
            get(discovery::api_rbac_v1_resources),
        )
        .route(
            "/apis/apiextensions.k8s.io/v1",
            get(discovery::api_apiextensions_v1_resources),
        )
        // Core v1 — cluster-scoped resources
        .route(
            "/api/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/api/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        // Namespace /finalize subresource (graceful deletion, #28). Must be
        // registered before the generic namespaced routes; the static `finalize`
        // segment takes precedence over `{resource}`.
        .route(
            "/api/v1/namespaces/{name}/finalize",
            put(resource::finalize_namespace),
        )
        // Core v1 — namespace-scoped resources
        .route(
            "/api/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/api/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        // ServiceAccount TokenRequest (mint a bound SA token)
        .route(
            "/api/v1/namespaces/{namespace}/serviceaccounts/{name}/token",
            post(crate::handlers::token::create_serviceaccount_token),
        )
        // Apps v1 — namespace-scoped resources
        .route(
            "/apis/apps/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/apps/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        // Apps v1 — cluster-scoped list (e.g., kubectl get deployments --all-namespaces)
        .route(
            "/apis/apps/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection),
        )
        // Batch v1 — namespace-scoped resources (jobs, cronjobs)
        .route(
            "/apis/batch/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/batch/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/batch/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection),
        )
        // Coordination v1
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/coordination.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection),
        )
        // discovery.k8s.io v1 — EndpointSlices (namespaced). Needed by Cilium /
        // kube-proxy-replacement, which use slices as the modern default (#22).
        .route(
            "/apis/discovery.k8s.io/v1",
            get(discovery::api_discovery_v1_resources),
        )
        .route(
            "/apis/discovery.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection).post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/discovery.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/discovery.k8s.io/v1/{resource}",
            get(resource::list_all_namespaces_resources),
        )
        // events.k8s.io v1 — the modern Event API (#48). Same stored objects as
        // core/v1 Event, translated field names both ways by dedicated handlers.
        .route("/apis/events.k8s.io/v1", get(crate::events::discovery))
        .route(
            "/apis/events.k8s.io/v1/namespaces/{namespace}/events",
            get(crate::events::list_ns)
                .post(crate::events::create)
                .delete(crate::events::delete_collection),
        )
        .route(
            "/apis/events.k8s.io/v1/namespaces/{namespace}/events/{name}",
            get(crate::events::get)
                .put(crate::events::update)
                .delete(crate::events::delete)
                .patch(crate::events::patch),
        )
        .route("/apis/events.k8s.io/v1/events", get(crate::events::list_all))
        // storage.k8s.io v1 — CSI ecosystem (#24): StorageClass, CSIDriver,
        // CSINode, VolumeAttachment (cluster-scoped) + CSIStorageCapacity
        // (namespaced). Plain stored resources driven by the CSI sidecars.
        .route(
            "/apis/storage.k8s.io/v1",
            get(discovery::api_storage_v1_resources),
        )
        .route(
            "/apis/storage.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection).post(resource::create_cluster_resource),
        )
        .route(
            "/apis/storage.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        .route(
            "/apis/storage.k8s.io/v1/{resource}/{name}/status",
            get(resource::get_cluster_status)
                .put(resource::update_cluster_status)
                .merge(patch(resource::patch_cluster_status)),
        )
        .route(
            "/apis/storage.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection).post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/storage.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        // resource.k8s.io v1 — Dynamic Resource Allocation (#137): DeviceClass,
        // ResourceSlice (cluster-scoped), ResourceClaim (+ /status) and
        // ResourceClaimTemplate (namespaced). Stored and served; nothing
        // allocates yet.
        .route("/apis/resource.k8s.io/v1", get(discovery::api_resource_v1_resources))
        .route(
            "/apis/resource.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection).post(resource::create_cluster_resource),
        )
        .route(
            "/apis/resource.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        .route(
            "/apis/resource.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection).post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/resource.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/resource.k8s.io/v1/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        // policy/v1 — PodDisruptionBudget (#7) + the pod Eviction subresource.
        .route(
            "/apis/policy/v1",
            get(discovery::api_policy_v1_resources),
        )
        .route(
            "/apis/authorization.k8s.io/v1",
            get(discovery::api_authorization_v1_resources),
        )
        .route(
            "/apis/subresources.kubevirt.io/v1",
            get(discovery::api_kubevirt_subresources_v1_resources),
        )
        .route(
            "/apis/policy/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection).post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/policy/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/policy/v1/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        .route(
            "/apis/policy/v1/{resource}",
            get(resource::list_all_namespaces_resources),
        )
        // Eviction subresource on core pods — PDB-gated (#7).
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/eviction",
            post(crate::eviction::create_eviction),
        )
        // scheduling.k8s.io v1 — PriorityClass (cluster-scoped)
        .route(
            "/apis/scheduling.k8s.io/v1",
            get(discovery::api_scheduling_v1_resources),
        )
        .route(
            "/apis/authentication.k8s.io/v1",
            get(discovery::api_authentication_v1_resources),
        )
        .route(
            "/apis/scheduling.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection).post(resource::create_cluster_resource),
        )
        .route(
            "/apis/scheduling.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        // RBAC v1
        .route(
            "/apis/rbac.authorization.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        // certificates.k8s.io/v1 — CertificateSigningRequests (cluster-scoped)
        // with approval/status subresources, for node-join CSR bootstrapping.
        .route(
            "/apis/certificates.k8s.io/v1",
            get(discovery::api_certificates_v1_resources),
        )
        .route(
            "/apis/certificates.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection).post(resource::create_cluster_resource),
        )
        .route(
            "/apis/certificates.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        .route(
            "/apis/certificates.k8s.io/v1/{resource}/{name}/approval",
            get(resource::get_cluster_status)
                .put(resource::update_cluster_status)
                .patch(resource::patch_cluster_status),
        )
        .route(
            "/apis/certificates.k8s.io/v1/{resource}/{name}/status",
            get(resource::get_cluster_status)
                .put(resource::update_cluster_status)
                .patch(resource::patch_cluster_status),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        // RustKube v1alpha1 (PodMigration)
        .route(
            "/apis/rustkube.io/v1alpha1",
            get(discovery::api_rustkube_v1alpha1_resources),
        )
        .route(
            "/apis/rustkube.io/v1alpha1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/rustkube.io/v1alpha1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/rustkube.io/v1alpha1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection),
        )
        // Status subresource routes — core v1 cluster-scoped
        .route(
            "/api/v1/{resource}/{name}/status",
            get(resource::get_cluster_status)
                .put(resource::update_cluster_status)
                .merge(patch(resource::patch_cluster_status)),
        )
        // TokenReview — how a component that holds a token but not the signing
        // key asks whether it is valid. The kubelet authenticates every inbound
        // request this way.
        .route(
            "/apis/authentication.k8s.io/v1/tokenreviews",
            axum::routing::post(crate::handlers::token::create_token_review),
        )
        // authorization.k8s.io — "may I?" and "what may I?", answered by the
        // same engine that decides the real request (#59). Write-only virtual
        // resources: nothing is stored.
        .route(
            "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews",
            axum::routing::post(crate::handlers::authorization::create_self_subject_access_review),
        )
        .route(
            "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews",
            axum::routing::post(crate::handlers::authorization::create_self_subject_rules_review),
        )
        // The privileged siblings: asking about *another* identity. Ordinary
        // RBAC governs them — no bootstrap role grants them but cluster-admin
        // — which is what `oc adm policy who-can` needs (#69).
        .route(
            "/apis/authorization.k8s.io/v1/subjectaccessreviews",
            axum::routing::post(crate::handlers::authorization::create_subject_access_review),
        )
        .route(
            "/apis/authorization.k8s.io/v1/namespaces/{namespace}/localsubjectaccessreviews",
            axum::routing::post(
                crate::handlers::authorization::create_local_subject_access_review,
            ),
        )
        // subresources.kubevirt.io — the console doors `virtctl` resolves
        // through (#61). Not CRD subresources: a CRD gets `/status` and
        // `/scale`, and these are a WebSocket proxied to the node running the
        // guest. GET only, because a WebSocket handshake is a GET.
        .route(
            "/apis/subresources.kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances/{name}/console",
            get(crate::handlers::kubevirt::vmi_console),
        )
        .route(
            "/apis/subresources.kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances/{name}/vnc",
            get(crate::handlers::kubevirt::vmi_vnc),
        )
        // virtualmachines/start|stop|restart (#62). Writes to a stored
        // object rather than a proxy: the verbs state intent and the
        // VirtualMachine controller reconciles it. PUT is what virtctl sends.
        .route(
            "/apis/subresources.kubevirt.io/v1/namespaces/{namespace}/virtualmachines/{name}/start",
            axum::routing::put(crate::handlers::kubevirt::vm_start),
        )
        .route(
            "/apis/subresources.kubevirt.io/v1/namespaces/{namespace}/virtualmachines/{name}/stop",
            axum::routing::put(crate::handlers::kubevirt::vm_stop),
        )
        .route(
            "/apis/subresources.kubevirt.io/v1/namespaces/{namespace}/virtualmachines/{name}/restart",
            axum::routing::put(crate::handlers::kubevirt::vm_restart),
        )
        .route(
            "/apis/subresources.kubevirt.io/v1/namespaces/{namespace}/virtualmachines/{name}/migrate",
            axum::routing::put(crate::handlers::kubevirt::vm_migrate),
        )
        .route(
            "/apis/subresources.kubevirt.io/v1/namespaces/{namespace}/virtualmachineinstances/{name}/migrate",
            axum::routing::put(crate::handlers::kubevirt::vmi_migrate),
        )
        // pods/log — what `kubectl logs` actually calls. Registered before the
        // generic {resource}/{name}/status route so the more specific path
        // wins, and separate from it because a log is proxied to the node
        // rather than read from the datastore (#54).
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/log",
            get(crate::handlers::logs::pod_logs),
        )
        // exec / attach / portforward — the streaming subresources, proxied to
        // the kubelet as a transparent connection upgrade (#42). GET and POST
        // both: SPDY clients POST, WebSocket clients GET.
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/exec",
            get(crate::handlers::streaming::pod_exec)
                .post(crate::handlers::streaming::pod_exec),
        )
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/attach",
            get(crate::handlers::streaming::pod_attach)
                .post(crate::handlers::streaming::pod_attach),
        )
        .route(
            "/api/v1/namespaces/{namespace}/pods/{name}/portforward",
            get(crate::handlers::streaming::pod_portforward)
                .post(crate::handlers::streaming::pod_portforward),
        )
        // Status subresource routes — core v1 namespace-scoped
        .route(
            "/api/v1/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        // Scale subresource, autoscaling/v1 Scale (#86)
        .route(
            "/apis/apps/v1/namespaces/{namespace}/{resource}/{name}/scale",
            get(crate::handlers::scale::get_apps)
                .put(crate::handlers::scale::put_apps)
                .merge(patch(crate::handlers::scale::patch_apps)),
        )
        // Status subresource routes — apps/v1
        .route(
            "/apis/apps/v1/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        // Status subresource routes — batch/v1
        .route(
            "/apis/batch/v1/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        // autoscaling/v2
        .route(
            "/apis/autoscaling/v2",
            get(discovery::api_autoscaling_v2_resources),
        )
        .route(
            "/apis/autoscaling/v2/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/autoscaling/v2/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/autoscaling/v2/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        .route(
            "/apis/autoscaling/v2/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection),
        )
        // project.openshift.io/v1 — Projects are Namespaces with owners (#97).
        // Static paths, so they win over the CRD catch-all below.
        .route(
            "/apis/project.openshift.io/v1",
            get(discovery::api_project_v1_resources),
        )
        .route(
            "/apis/project.openshift.io/v1/projects",
            get(crate::handlers::project::list_projects)
                .post(crate::handlers::project::create_project),
        )
        .route(
            "/apis/project.openshift.io/v1/projects/{name}",
            get(crate::handlers::project::get_project)
                .put(crate::handlers::project::update_project)
                .delete(crate::handlers::project::delete_project),
        )
        .route(
            "/apis/project.openshift.io/v1/projectrequests",
            get(crate::handlers::project::list_project_requests)
                .post(crate::handlers::project::create_project_request),
        )
        // route.openshift.io/v1 — OpenShift Routes
        .route(
            "/apis/route.openshift.io/v1",
            get(discovery::api_route_v1_resources),
        )
        .route(
            "/apis/route.openshift.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/route.openshift.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/route.openshift.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection),
        )
        // networking.k8s.io/v1
        .route(
            "/apis/networking.k8s.io/v1",
            get(discovery::api_networking_v1_resources),
        )
        .route(
            "/apis/networking.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/networking.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        // Ingress `/status` — without it the path fell through to the CRD
        // catch-all and answered `resource "ingresses" not found` (#67).
        .route(
            "/apis/networking.k8s.io/v1/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        .route(
            "/apis/networking.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/networking.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        // ServiceCIDR's /status (#134)
        .route(
            "/apis/networking.k8s.io/v1/{resource}/{name}/status",
            get(resource::get_cluster_status)
                .put(resource::update_cluster_status)
                .merge(patch(resource::patch_cluster_status)),
        )
        // admissionregistration.k8s.io/v1
        .route(
            "/apis/admissionregistration.k8s.io/v1",
            get(discovery::api_admissionregistration_v1_resources),
        )
        .route(
            "/apis/admissionregistration.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/admissionregistration.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        // gateway.networking.k8s.io/v1
        .route(
            "/apis/gateway.networking.k8s.io/v1",
            get(discovery::api_gateway_v1_resources),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .delete(resource::delete_namespaced_collection)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        // apiregistration.k8s.io/v1
        .route(
            "/apis/apiregistration.k8s.io/v1",
            get(discovery::api_apiregistration_v1_resources),
        )
        .route(
            "/apis/apiregistration.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .delete(resource::delete_cluster_collection)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/apiregistration.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
        )
        // apiextensions.k8s.io/v1 CRD management (customresourcedefinitions) is
        // served by the generic /apis/{group}/{version}/{resource} catch-all
        // below — the 3-arg handlers extract group=apiextensions.k8s.io,
        // version=v1, resource=customresourcedefinitions correctly, and
        // validate_crd allow-lists it. (A dedicated 1-arg route here caused a
        // Path-arity 500 that blocked all CRDs / Cilium — rustkube#21.)
        //
        // CRD catch-all routes for dynamic custom resources
        .route(
            "/apis/{group}/{version}/{resource}",
            get(crd::crd_list_cluster)
                .post(crd::crd_create_cluster)
                .delete(crd::crd_delete_collection_cluster),
        )
        .route(
            "/apis/{group}/{version}/{resource}/{name}",
            get(crd::crd_get_cluster)
                .put(crd::crd_update_cluster)
                .delete(crd::crd_delete_cluster)
                .patch(crd::crd_patch_cluster),
        )
        // CR /status subresource (CRDs declaring subresources.status) — #23
        .route(
            "/apis/{group}/{version}/{resource}/{name}/status",
            get(crd::crd_get_status_cluster)
                .put(crd::crd_update_status_cluster)
                .patch(crd::crd_patch_status_cluster),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}",
            get(crd::crd_list_ns)
                .post(crd::crd_create_ns)
                .delete(crd::crd_delete_collection_ns),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}",
            get(crd::crd_get_ns)
                .put(crd::crd_update_ns)
                .delete(crd::crd_delete_ns)
                .patch(crd::crd_patch_ns),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}/scale",
            get(crate::handlers::scale::get_cr_ns)
                .put(crate::handlers::scale::put_cr_ns)
                .merge(patch(crate::handlers::scale::patch_cr_ns)),
        )
        .route(
            "/apis/{group}/{version}/{resource}/{name}/scale",
            get(crate::handlers::scale::get_cr_cluster)
                .put(crate::handlers::scale::put_cr_cluster)
                .merge(patch(crate::handlers::scale::patch_cr_cluster)),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}/status",
            get(crd::crd_get_status_ns)
                .put(crd::crd_update_status_ns)
                .patch(crd::crd_patch_status_ns),
        )
        // Dynamic CRD discovery
        .route("/apis/{group}/{version}", get(crd::crd_api_resources))
        // Inside authentication and RBAC: an aggregated API's requests go to
        // its backend once this apiserver has authorized them (#83).
        .layer(middleware::from_fn_with_state(state.clone(), crate::aggregation::proxy))
        .layer(middleware::from_fn(move |req, next| {
            let rbac = rbac.clone();
            async move {
                let mut req: axum::extract::Request = req;
                req.extensions_mut().insert(rbac);
                rbac_engine::rbac_middleware(req, next).await
            }
        }))
        .layer(middleware::from_fn(move |req, next| {
            let keys = signing_keys.clone();
            let static_tokens = static_tokens.clone();
            async move {
                let mut req: axum::extract::Request = req;
                req.extensions_mut().insert(keys);
                req.extensions_mut().insert(static_tokens);
                req.extensions_mut()
                    .insert(auth::AnonymousAuth(anonymous_auth));
                auth::auth_middleware(req, next).await
            }
        }))
        .with_state(state)
}

/// Start the API server.
pub async fn run(config: ApiServerConfig) -> anyhow::Result<()> {
    // Connect to the external etcd/fastetcd datastore (kube architecture).
    if config.etcd_servers.is_empty() {
        anyhow::bail!(
            "no --etcd-servers configured: RustKube requires an external etcd/fastetcd datastore"
        );
    }
    let etcd_tls = if config.etcd_cacert.is_some()
        || config.etcd_cert.is_some()
        || config.etcd_key.is_some()
    {
        Some(EtcdTls {
            ca: config.etcd_cacert.clone(),
            cert: config.etcd_cert.clone(),
            key: config.etcd_key.clone(),
        })
    } else {
        None
    };
    tracing::info!("connecting to datastore: {:?}", config.etcd_servers);
    let store = EtcdStore::connect(&config.etcd_servers, etcd_tls)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to etcd/fastetcd {:?}: {e}", config.etcd_servers))?;
    let kv: Arc<dyn KvStore> = Arc::new(store);
    let storage = Arc::new(ResourceStorage::new(kv.clone()));

    // One gate for the whole of bootstrap. `Client::connect` above succeeds
    // against a datastore that is not up — the connection is lazy — so without
    // this the next dozen writes go into a hole and the cluster comes up with
    // no namespaces and no RBAC (#52).
    wait_for_datastore(&storage).await;
    crate::compactor::spawn(kv, config.etcd_compaction_interval);

    // Bootstrap default namespaces
    bootstrap_namespace(&storage, "default").await;
    bootstrap_namespace(&storage, "kube-system").await;
    bootstrap_namespace(&storage, "kube-public").await;
    bootstrap_namespace(&storage, "kube-node-lease").await;
    backfill_namespace_defaults(&storage).await;
    backfill_secret_string_data(&storage).await;
    bootstrap_service_cidr(&storage, &config.service_cidr).await;

    // Bootstrap RBAC resources
    bootstrap_rbac(&storage, config.anonymous_auth, config.dev_anonymous_admin).await;

    // default/kubernetes Service + Endpoints, so in-cluster client-go can reach
    // the apiserver via KUBERNETES_SERVICE_HOST (#30). Re-run periodically so a
    // restarted/replaced replica re-registers itself.
    {
        let cluster_ip = first_service_ip(&config.service_cidr)
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "10.96.0.1".to_string());
        let advertise = config.advertise_address.clone().or_else(|| {
            // Fall back to the bind address when it names a concrete interface.
            match config.bind_addr.as_str() {
                "0.0.0.0" | "::" | "" => None,
                addr => Some(addr.to_string()),
            }
        });
        if advertise.is_none() {
            tracing::warn!(
                "no --advertise-address (and --bind-addr is a wildcard): this apiserver \
                 will not register itself in the default/kubernetes Endpoints"
            );
        }
        let storage_ep = storage.clone();
        let port = config.secure_port;
        reconcile_kubernetes_service(&storage_ep, &cluster_ip, advertise.as_deref(), port).await;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.tick().await; // consume the immediate tick
            loop {
                tick.tick().await;
                reconcile_kubernetes_service(
                    &storage_ep,
                    &cluster_ip,
                    advertise.as_deref(),
                    port,
                )
                .await;
            }
        });
    }

    // Initialize CRD registry and load existing CRDs
    let crd_registry = Arc::new(CrdRegistry::new());
    crd::load_existing_crds(&storage, &crd_registry).await;
    // And keep following them: a CRD written through another replica, or one
    // the boot read could not see, is registered here too (#185).
    tokio::spawn(crd::follow_stored_crds(storage.clone(), crd_registry.clone()));

    // Manifests that ship with the node, applied once. After the CRD registry
    // is loaded, because a manifest may be a custom resource of a CRD that is
    // already established; before the HTTP server starts, so a controller or a
    // kubelet never observes a half-applied cluster.
    if let Some(dir) = &config.manifest_dir {
        crate::manifests::apply_dir(&storage, &crd_registry, dir).await;
    }

    // Claim the addresses Services already hold.
    //
    // Bootstrap objects and everything the manifest applier writes go straight
    // to storage rather than through admission, so nothing recorded a claim
    // for them. The allocator would then scan from the bottom of the range and
    // hand out an address already in use — a new Service was given 10.96.0.1,
    // the apiserver's own, and every connection to it reached the apiserver.
    //
    // After the manifests and before the server accepts anything, so the first
    // Service a client creates is allocated against a complete picture.
    let claimed = crate::service_ip::reconcile(&storage).await;
    if claimed > 0 {
        info!("service-ip: claimed {claimed} address(es) already in use");
    }

    // And then check, rather than assume.
    //
    // Claims, the reconcile above and claiming on the direct write paths are
    // all guards, and a guard nobody checks is a guess. A duplicate ClusterIP
    // is silent, intermittent and blames the network, so it is worth one list
    // at startup to be able to say it did not happen — and to say so loudly
    // when it did, rather than leaving it to be found from the far end months
    // later.
    for (ip, holders) in crate::service_ip::duplicates(&storage).await {
        error!(
            "service-ip: {ip} is held by {} Services ({}) — something wrote a \
             Service by a path that does not claim its address",
            holders.len(),
            holders.join(", ")
        );
    }

    // ServiceAccount token signing keys. A real cluster supplies the RSA
    // keypair (--service-account-signing-key-file / --service-account-key-file)
    // so every replica signs and verifies with the same key — tokens then work
    // across apiservers and survive restarts (#11). Without it we fall back to
    // an ephemeral per-process HMAC key, which only works single-replica.
    let signing_keys = match (
        &config.service_account_signing_key,
        config.service_account_key.is_empty(),
    ) {
        (Some(priv_path), false) => {
            let priv_pem = std::fs::read(priv_path).map_err(|e| {
                anyhow::anyhow!("reading --service-account-signing-key-file {priv_path:?}: {e}")
            })?;
            let mut pub_pems = Vec::new();
            for pub_path in &config.service_account_key {
                pub_pems.push(std::fs::read(pub_path).map_err(|e| {
                    anyhow::anyhow!("reading --service-account-key-file {pub_path:?}: {e}")
                })?);
            }
            let refs: Vec<&[u8]> = pub_pems.iter().map(Vec::as_slice).collect();
            let keys = SigningKeys::from_pem(&priv_pem, &refs)
                .map_err(|e| anyhow::anyhow!("loading ServiceAccount keys: {e}"))?;
            tracing::info!(
                "ServiceAccount tokens: signed with {priv_path:?}, verified against {} key(s) in {:?}",
                keys.verifying_keys(),
                config.service_account_key
            );
            keys
        }
        _ => {
            tracing::warn!(
                "no ServiceAccount keypair configured (--service-account-signing-key-file \
                 and --service-account-key-file); using an EPHEMERAL key — tokens will not \
                 survive restart and will be rejected by other apiserver replicas"
            );
            SigningKeys::generate()
        }
    };

    // Issuer, audiences and bound-object checks for ServiceAccount tokens
    // (#182).
    let signing_keys = signing_keys
        .with_token_config(
            &config.service_account_issuer,
            &config.api_audiences,
            config.service_account_extend_token_expiration,
        )
        .with_bound_objects(storage.clone());
    tracing::info!(
        "ServiceAccount tokens: issuer {}, API audiences {:?}",
        signing_keys.issuer(),
        signing_keys.api_audiences()
    );

    // Static bearer tokens (--token-auth-file, #188): install-config's
    // apiToken, written by stormpump on first boot. Followed, not read once.
    let static_tokens = match &config.token_auth_file {
        Some(path) => crate::token_file::StaticTokens::follow(path.clone()),
        None => crate::token_file::StaticTokens::none(),
    };

    // Initialize RBAC engine
    let rbac = Arc::new(
        RbacEngine::new(storage.clone())
            .with_anonymous_admin(config.anonymous_auth && config.dev_anonymous_admin),
    );

    // API aggregation (#83): the APIService table, followed from storage,
    // the backends' availability, and what aggregated servers are told about
    // this apiserver's front-proxy identity.
    let proxy_identity = match (&config.proxy_client_cert, &config.proxy_client_key) {
        (Some(cert), Some(key)) => {
            let pair = apimachinery::tls_reload::ReloadingKey::from_pem(&std::fs::read(cert)?, &std::fs::read(key)?)
                .map_err(|e| anyhow::anyhow!("--proxy-client-cert-file: {e}"))?;
            pair.watch("proxy client certificate", cert.clone(), key.clone(), |_| {});
            Some(pair)
        }
        (None, None) => None,
        _ => anyhow::bail!("--proxy-client-cert-file and --proxy-client-key-file go together"),
    };
    let aggregator = Arc::new(crate::aggregation::Aggregator::new(proxy_identity));
    tokio::spawn(crate::aggregation::follow(storage.clone(), aggregator.clone()));
    tokio::spawn(crate::aggregation::keep_available(storage.clone(), aggregator.clone()));
    publish_extension_authentication(storage.clone(), &config);

    let state = AppState {
        storage,
        crd_registry,
        service_cidr: config.service_cidr.clone(),
        admission: Default::default(),
        aggregator: aggregator.clone(),
        resource_metrics: Arc::new(crate::resource_metrics::ResourceMetrics::new(
            &config.cadvisor_scheme,
            config.cadvisor_port,
            config.cadvisor_ca.as_deref(),
            config.cadvisor_token_file.clone(),
        )?),
    };
    // Prometheus recorder + /metrics, shared with the other components
    // (apimachinery::metrics) so the `process_*` family and the build-info
    // gauge are the same everywhere. Unlike the scheduler and the controller
    // manager, the apiserver serves /metrics on its own API listener rather
    // than a second port — that is where upstream serves it, and where a
    // scrape config expects it.
    let prom = apimachinery::metrics::install("kube-apiserver")
        .ok_or_else(|| anyhow::anyhow!("prometheus recorder could not be installed"))?;

    // /metrics is inside authentication and RBAC (#90): a principal allowed
    // `get` on the non-resource URL, as upstream (`system:monitoring`).
    let app = build_router(state, signing_keys, static_tokens, rbac, config.anonymous_auth, prom)
        // Protobuf content negotiation: decode application/vnd.kubernetes.protobuf
        // requests to JSON and re-encode JSON responses when the client asked
        // for protobuf (client-go's default for built-in types) — #32.
        // An aggregated API's bodies are its own (#83): passed as they are.
        .layer(middleware::from_fn(move |req: axum::extract::Request, next: middleware::Next| {
            let aggregator = aggregator.clone();
            async move {
                if aggregator.claims(req.uri().path()).is_some() {
                    next.run(req).await
                } else {
                    crate::protobuf_mw::transcode(req, next).await
                }
            }
        }))
        .layer(middleware::from_fn(metrics_middleware))
        // Outermost: no single request may panic the process. Any panic in a
        // handler/middleware is caught and turned into a 500 (rustkube#9).
        .layer(tower_http::catch_panic::CatchPanicLayer::new());

    let addr = format!("{}:{}", config.bind_addr, config.secure_port);

    // Resolve TLS material: explicit cert/key files, else an auto self-signed
    // cert (dev), else plain HTTP.
    let tls_pem: Option<(Vec<u8>, Vec<u8>)> =
        if let (Some(cert), Some(key)) = (&config.tls_cert, &config.tls_key) {
            Some((std::fs::read(cert)?, std::fs::read(key)?))
        } else if config.tls_auto {
            let sans = vec![
                "kubernetes".to_string(),
                "kubernetes.default".to_string(),
                "kubernetes.default.svc".to_string(),
                "kubernetes.default.svc.cluster.local".to_string(),
                "localhost".to_string(),
            ];
            let sc = apimachinery::certs::generate_server_cert("kube-apiserver", &sans)?;
            Some((sc.cert_pem.into_bytes(), sc.key_pem.into_bytes()))
        } else {
            None
        };

    let listener = TcpListener::bind(&addr).await?;
    match tls_pem {
        Some((cert, key)) => {
            // Install the ring crypto provider once (rustls 0.23 requires one).
            let _ = rustls::crypto::ring::default_provider().install_default();
            let client_ca = config.client_ca.as_ref().map(std::fs::read).transpose()?;
            if client_ca.is_some() {
                info!("kube-apiserver serving HTTPS on {addr} (x509 client-cert auth enabled)");
            } else {
                info!("kube-apiserver serving HTTPS on {addr}");
            }
            // Cert-lifecycle monitoring (#20): expose expiry as a metric and warn
            // as certs approach it, so a long-lived static PKI can't silently
            // lapse. The serving cert and the client-auth CA are the ones whose
            // expiry takes the apiserver (or client auth) down.
            report_cert_expiry("serving", &cert);
            if let Some(ca) = &client_ca {
                report_cert_expiry("client-ca", ca);
            }
            let (cfg, resolver) =
                crate::tls::server_config(&cert, &key, client_ca.as_deref())?;
            let cfg: crate::tls::CurrentConfig = Arc::new(std::sync::RwLock::new(Arc::new(cfg)));
            // A rotated client CA applies to new connections without a
            // restart (#105).
            if let Some(ca_path) = &config.client_ca {
                crate::tls::watch_client_ca(cfg.clone(), resolver.clone(), ca_path.clone());
            }
            // Renewal takes effect without a restart (#20). Only for a cert
            // that came from a file: an auto-generated one has nowhere to be
            // renewed from, and watching a path nobody writes is a task that
            // does nothing forever.
            if let (Some(cert_path), Some(key_path)) = (&config.tls_cert, &config.tls_key) {
                crate::tls::watch_cert_files(resolver, cert_path.clone(), key_path.clone());
            }
            crate::tls::serve(listener, app, cfg).await?;
        }
        None => {
            // Never drop TLS silently (#16). Serving the API — bearer tokens,
            // client certs, all traffic — in cleartext must be an explicit
            // choice, not the fallback when certs are missing/misconfigured.
            if !config.insecure {
                anyhow::bail!(
                    "refusing to serve plain HTTP: no TLS configured (need --tls-cert-file \
                     + --tls-private-key-file, or --tls for a self-signed cert). Pass \
                     --insecure to serve cleartext anyway (dev/bring-up only)."
                );
            }
            tracing::warn!(
                "SECURITY: serving plain HTTP on {addr} (--insecure) — credentials travel \
                 in cleartext; do not use in production"
            );
            // TCP_NODELAY, as for TLS (crate::tls::serve, #190).
            use axum::serve::ListenerExt;
            let listener = listener.tap_io(|tcp| {
                if let Err(e) = tcp.set_nodelay(true) {
                    tracing::debug!("TCP_NODELAY: {e}");
                }
            });
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}

/// Days before expiry at which a certificate is considered near-expiry.
const CERT_EXPIRY_WARN_DAYS: i64 = 30;

/// Publish a cert's expiry as `apiserver_certificate_expiration_seconds{name}`
/// (a unix timestamp — alerting rules compute `value - time()`) and log a
/// warning if it expires within `CERT_EXPIRY_WARN_DAYS` (#20).
pub(crate) fn report_cert_expiry(name: &'static str, cert_pem: &[u8]) {
    let Some(not_after) = apimachinery::certs::cert_not_after_unix(cert_pem) else {
        tracing::warn!("could not parse {name} certificate to determine expiry");
        return;
    };
    metrics::gauge!("apiserver_certificate_expiration_seconds", "name" => name)
        .set(not_after as f64);

    let now = chrono::Utc::now().timestamp();
    let days_left = (not_after - now) / 86_400;
    if days_left <= CERT_EXPIRY_WARN_DAYS {
        tracing::warn!(
            "certificate '{name}' expires in {days_left} day(s) — rotate it (control-plane \
             cert rotation is #20)"
        );
    } else {
        info!("certificate '{name}' valid for {days_left} more day(s)");
    }
}

/// The `data` of `kube-system/extension-apiserver-authentication`: how an
/// aggregated API server authenticates this apiserver's proxied requests
/// (the front-proxy CA, allowed names and the X-Remote-* headers), and the
/// client CA, as upstream publishes them (#83).
pub(crate) fn extension_authentication_data(client_ca: Option<&str>, requestheader_ca: Option<&str>, allowed_names: &[String]) -> serde_json::Value {
    let mut data = serde_json::Map::new();
    if let Some(ca) = client_ca {
        data.insert("client-ca-file".into(), json!(ca));
    }
    if let Some(ca) = requestheader_ca {
        data.insert("requestheader-client-ca-file".into(), json!(ca));
        data.insert("requestheader-allowed-names".into(), json!(serde_json::to_string(allowed_names).unwrap_or_default()));
        data.insert("requestheader-username-headers".into(), json!(r#"["X-Remote-User"]"#));
        data.insert("requestheader-group-headers".into(), json!(r#"["X-Remote-Group"]"#));
        data.insert("requestheader-extra-headers-prefix".into(), json!(r#"["X-Remote-Extra-"]"#));
    }
    serde_json::Value::Object(data)
}

/// Keep `kube-system/extension-apiserver-authentication` current: written
/// at start and whenever a CA file's content changes (checked every 30 s,
/// so a renewed CA reaches the aggregated servers).
fn publish_extension_authentication(storage: Arc<ResourceStorage>, config: &ApiServerConfig) {
    if config.client_ca.is_none() && config.requestheader_client_ca.is_none() {
        return;
    }
    let (client_ca, requestheader_ca, names) =
        (config.client_ca.clone(), config.requestheader_client_ca.clone(), config.requestheader_allowed_names.clone());
    tokio::spawn(async move {
        const NAME: &str = "extension-apiserver-authentication";
        let key = ResourceStorage::namespaced_key("configmaps", "kube-system", NAME);
        let read = |p: &Option<std::path::PathBuf>| p.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        loop {
            let data = extension_authentication_data(read(&client_ca).as_deref(), read(&requestheader_ca).as_deref(), &names);
            match storage.get(&key).await {
                Ok(mut cm) if cm["data"] != data => {
                    let rev = cm["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
                    cm["data"] = data;
                    if let Err(e) = storage.update(&key, cm, rev).await {
                        if e.reason != "Conflict" {
                            tracing::warn!("{NAME}: not updated: {}", e.message);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let mut cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "data": data});
                    crate::handlers::resource::ensure_metadata_pub(&mut cm, NAME, Some("kube-system"));
                    create_bootstrap(&storage, &key, cm, &format!("configmaps kube-system/{NAME}")).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

/// Create a namespace if it doesn't already exist.
/// Block until the datastore answers, then return.
///
/// One gate for the whole bootstrap, rather than a retry loop around each of
/// the dozen objects it writes. This is startup-only and costs nothing on the
/// happy path: a store that is already up answers the first probe and this
/// returns immediately.
///
/// It exists because `Client::connect` succeeds against a datastore that is not
/// up — the connection is established lazily, so the apiserver believed it had
/// a store and wrote a dozen objects into a hole. Every one of those writes was
/// `let _ = storage.create(...)`, so every failure was discarded, and the
/// cluster ran permanently with no namespaces and no RBAC. It denied every
/// request, including the kubelet registering its own Node, and presented as an
/// authorization bug — the authorizer was correct throughout.
///
/// Backoff is exponential and capped, not a fixed-interval poll: a store that
/// comes up in 200 ms is not made to wait five seconds, and one that is broken
/// is not hammered.
async fn wait_for_datastore(storage: &ResourceStorage) -> bool {
    // ~60s total. Longer than any datastore takes to come up, shorter than
    // anyone's patience for a control plane that is silently useless.
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
    let start = std::time::Instant::now();
    let mut delay = std::time::Duration::from_millis(50);
    let mut complained = false;

    loop {
        // A read of a key that does not exist. NotFound means the store
        // answered, which is the whole question — a transport error does not.
        match storage
            .get(&ResourceStorage::cluster_key("namespaces", "kube-system"))
            .await
        {
            Ok(_) => return true,
            Err(e) if e.reason == "NotFound" => return true,
            Err(e) => {
                if start.elapsed() >= DEADLINE {
                    tracing::error!(
                        "datastore did not answer within {DEADLINE:?}: {}. Bootstrap will \
                         be attempted anyway and will almost certainly fail; this cluster \
                         will have no namespaces and no RBAC.",
                        e.message
                    );
                    return false;
                }
                if !complained {
                    tracing::info!("waiting for the datastore to answer: {}", e.message);
                    complained = true;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(2));
            }
        }
    }
}

/// Write a bootstrap object, and say so when it does not land.
///
/// One attempt: `wait_for_datastore` has already established that the store
/// answers, so a failure here is a real failure and not a race. What must never
/// happen again is the previous `let _ = storage.create(...)`, which discarded
/// it in silence.
///
/// `AlreadyExists` and `Conflict` are success — a restart, or another replica
/// won the race to create it.
async fn create_bootstrap(
    storage: &ResourceStorage,
    key: &str,
    obj: serde_json::Value,
    what: &str,
) {
    match storage.create(key, obj).await {
        Ok(_) => {}
        Err(e) if e.reason == "AlreadyExists" || e.reason == "Conflict" => {}
        Err(e) => tracing::error!(
            "bootstrap: {what} was not created: {}. Requests needing it will be denied.",
            e.message
        ),
    }
}

/// The `kubernetes` ServiceCIDR (#134): the range ClusterIPs come from, as
/// upstream bootstraps it from `--service-cluster-ip-range`, Ready. Created
/// once; a stored one is left as it is (its `spec.cidrs` is immutable
/// upstream).
async fn bootstrap_service_cidr(storage: &ResourceStorage, cidr: &str) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut obj = json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "ServiceCIDR",
        "spec": {"cidrs": [cidr]},
        "status": {"conditions": [{"type": "Ready", "status": "True", "reason": "",
                                   "message": "Kubernetes Service CIDR is ready", "lastTransitionTime": now}]},
    });
    crate::handlers::resource::ensure_metadata_pub(&mut obj, "kubernetes", None);
    create_bootstrap(storage, &ResourceStorage::cluster_key("servicecidrs", "kubernetes"), obj, "servicecidrs kubernetes").await;
}

async fn bootstrap_namespace(storage: &ResourceStorage, name: &str) {
    let key = ResourceStorage::cluster_key("namespaces", name);
    let ns = json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": name,
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "spec": {
            "finalizers": ["kubernetes"]
        },
        "status": {
            "phase": "Active"
        }
    });
    create_bootstrap(storage, &key, ns, &format!("namespace {name}")).await;
}

/// Give namespaces stored before #75 the phase and finalizer they were
/// created without.
///
/// Idempotent and safe under HA: each write is a CAS on the revision it read,
/// so two replicas booting together write once and the loser's Conflict is the
/// state it wanted. A namespace already terminating is left alone.
async fn backfill_namespace_defaults(storage: &ResourceStorage) {
    let prefix = ResourceStorage::cluster_prefix("namespaces");
    let mut token: Option<String> = None;
    let mut fixed = 0usize;
    loop {
        let (items, next, _) = match storage.list(&prefix, 500, token.as_deref()).await {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!("namespace backfill: list failed: {}", e.message);
                return;
            }
        };
        for mut ns in items {
            if !crate::builtin_admission::namespace_defaults(&mut ns) {
                continue;
            }
            let name = ns["metadata"]["name"].as_str().unwrap_or_default().to_string();
            let rev = ns["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
            let key = ResourceStorage::cluster_key("namespaces", &name);
            match storage.update(&key, ns, rev).await {
                Ok(_) => fixed += 1,
                Err(e) if e.reason == "Conflict" => {}
                Err(e) => tracing::warn!("namespace backfill: {name}: {}", e.message),
            }
        }
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    if fixed > 0 {
        tracing::info!("namespace backfill: {fixed} namespace(s) given phase Active and the kubernetes finalizer");
    }
}

/// Secrets stored before #101 kept `stringData` and had no `data` for it:
/// fold them once at boot, as a write now would. Conditional on each
/// Secret's revision, so a concurrent writer wins and is folded on its own
/// write; idempotent, so every apiserver of a multi-master cluster may run it.
async fn backfill_secret_string_data(storage: &ResourceStorage) {
    let prefix = ResourceStorage::all_namespaces_prefix("secrets");
    let mut token: Option<String> = None;
    let mut fixed = 0usize;
    loop {
        let (items, next, _) = match storage.list(&prefix, 500, token.as_deref()).await {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!("secret stringData backfill: list failed: {}", e.message);
                return;
            }
        };
        for mut secret in items {
            if !crate::builtin_admission::fold_string_data(&mut secret) {
                continue;
            }
            let ns = secret["metadata"]["namespace"].as_str().unwrap_or_default().to_string();
            let name = secret["metadata"]["name"].as_str().unwrap_or_default().to_string();
            let rev = secret["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
            let key = ResourceStorage::namespaced_key("secrets", &ns, &name);
            match storage.update(&key, secret, rev).await {
                Ok(_) => fixed += 1,
                Err(e) if e.reason == "Conflict" => {}
                Err(e) => tracing::warn!("secret stringData backfill: {ns}/{name}: {}", e.message),
            }
        }
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    if fixed > 0 {
        tracing::info!("secret stringData backfill: {fixed} Secret(s) folded into data (#101)");
    }
}

/// First usable address of a service CIDR (`10.96.0.0/12` → `10.96.0.1`), which
/// upstream assigns to the `default/kubernetes` Service.
fn first_service_ip(cidr: &str) -> Option<std::net::Ipv4Addr> {
    let (addr, _prefix) = cidr.split_once('/')?;
    let base: std::net::Ipv4Addr = addr.parse().ok()?;
    Some(std::net::Ipv4Addr::from(u32::from(base).checked_add(1)?))
}

/// Ensure the `default/kubernetes` Service exists and that this apiserver is
/// registered among its Endpoints/EndpointSlice (#30).
///
/// In-cluster client-go builds `https://$KUBERNETES_SERVICE_HOST:$PORT` — which
/// only resolves if this Service exists and its endpoints point at the live
/// apiservers. Each replica registers its own advertise address, so the set
/// converges to all running apiservers. Runs periodically so a restarted or
/// replaced apiserver re-registers itself.
async fn reconcile_kubernetes_service(
    storage: &ResourceStorage,
    cluster_ip: &str,
    advertise: Option<&str>,
    secure_port: u16,
) {
    // The Service itself: no selector, endpoints are managed here (as upstream).
    let svc_key = ResourceStorage::namespaced_key("services", "default", "kubernetes");
    if storage.get(&svc_key).await.is_err() {
        let svc = json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {
                "name": "kubernetes",
                "namespace": "default",
                "uid": uuid::Uuid::new_v4().to_string(),
                "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                "labels": { "component": "apiserver", "provider": "kubernetes" }
            },
            "spec": {
                "clusterIP": cluster_ip,
                "clusterIPs": [cluster_ip],
                "type": "ClusterIP",
                "sessionAffinity": "None",
                "ipFamilies": ["IPv4"],
                "ports": [{
                    "name": "https",
                    "protocol": "TCP",
                    "port": 443,
                    "targetPort": secure_port
                }]
            },
            "status": { "loadBalancer": {} }
        });
        let _ = storage.create(&svc_key, svc).await;
    }

    // Endpoints: add our advertise address if it isn't already listed.
    let Some(advertise) = advertise else {
        return;
    };
    let ep_key = ResourceStorage::namespaced_key("endpoints", "default", "kubernetes");
    let ports = json!([{ "name": "https", "port": secure_port, "protocol": "TCP" }]);

    for (key, slice) in [(&ep_key, false),
        (&ResourceStorage::namespaced_key("endpointslices", "default", "kubernetes"), true)] {
        // Preserve identity and merge against each freshly read revision. A
        // second master must neither replace the UID nor erase the first IP.
        for _ in 0..8 {
            let current = match storage.get(key).await {
                Ok(value) => Some(value),
                Err(e) if e.reason == "NotFound" => None,
                Err(e) => { tracing::warn!(error=%e.message, "cannot read bootstrap endpoint"); break; }
            };
            let desired = bootstrap_endpoint(current.as_ref(), slice, advertise, &ports);
            if current.as_ref() == Some(&desired) { break; }
            let result = if let Some(current) = &current {
                let rv = current["metadata"]["resourceVersion"].as_str().and_then(|s| s.parse().ok());
                storage.update(key, desired, rv).await
            } else { storage.create(key, desired).await };
            match result {
                Ok(_) => break,
                Err(e) if e.reason == "Conflict" || e.reason == "AlreadyExists" => continue,
                Err(e) => { tracing::warn!(error=%e.message, "cannot publish bootstrap endpoint"); break; }
            }
        }
    }
}

fn bootstrap_endpoint(current: Option<&serde_json::Value>, slice: bool, advertise: &str, ports: &serde_json::Value) -> serde_json::Value {
    let mut object = current.cloned().unwrap_or_else(|| json!({}));
    object["apiVersion"] = json!(if slice { "discovery.k8s.io/v1" } else { "v1" });
    object["kind"] = json!(if slice { "EndpointSlice" } else { "Endpoints" });
    crate::handlers::resource::ensure_metadata_pub(&mut object, "kubernetes", Some("default"));
    if slice {
        object["metadata"]["labels"]["kubernetes.io/service-name"] = json!("kubernetes");
        object["addressType"] = json!("IPv4");
        let mut endpoints = object["endpoints"].as_array().cloned().unwrap_or_default();
        if !endpoints.iter().any(|ep| ep["addresses"].as_array().is_some_and(|ips| ips.iter().any(|ip| ip == advertise))) {
            endpoints.push(json!({"addresses":[advertise],"conditions":{"ready":true}}));
        }
        object["endpoints"] = json!(endpoints);
        object["ports"] = ports.clone();
    } else {
        let mut addresses = object["subsets"][0]["addresses"].as_array().cloned().unwrap_or_default();
        if !addresses.iter().any(|a| a["ip"] == advertise) { addresses.push(json!({"ip":advertise})); }
        addresses.sort_by(|a,b| a["ip"].as_str().cmp(&b["ip"].as_str()));
        object["subsets"] = json!([{"addresses":addresses,"ports":ports}]);
    }
    object

}

/// Name of the ServiceAccount a node's ssh login authenticates as, and of the
/// ClusterRoleBinding that makes it cluster-admin (#79).
const NODE_ADMIN: &str = "node-admin";

/// The `kube-system/node-admin` ServiceAccount and its `node-admin` binding to
/// cluster-admin (#79).
///
/// The token is not minted here. stormcert signs one per node with the
/// ServiceAccount signing key and leaves it where only that node's login
/// container can read it, so the credential never leaves the node; this only
/// gives the name it carries something to be.
///
/// Tokens are verified by signature alone — the ServiceAccount is not looked
/// up — so deleting it revokes nothing. Deleting the binding does, until the
/// next boot re-creates it; revocation that must stick rotates the signing key.
fn node_admin_objects() -> (serde_json::Value, serde_json::Value) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let sa = json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {
            "name": NODE_ADMIN,
            "namespace": "kube-system",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": now
        }
    });
    let binding = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {
            "name": NODE_ADMIN,
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": now
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "cluster-admin"
        },
        "subjects": [{
            "kind": "ServiceAccount",
            "name": NODE_ADMIN,
            "namespace": "kube-system"
        }]
    });
    (sa, binding)
}

/// Create a bootstrap ClusterRole, or bring a stored one's rules up to date.
///
/// Create-only would freeze a role at whatever this binary first wrote: a
/// rule added in a later release would never reach a cluster that already
/// had the role. Upstream reconciles its bootstrap roles at every start for
/// that reason, and honours the same opt-out — a role annotated
/// `rbac.authorization.kubernetes.io/autoupdate: "false"` is the operator's,
/// and is left alone.
async fn reconcile_bootstrap_role(storage: &ResourceStorage, role: serde_json::Value) {
    let name = role["metadata"]["name"].as_str().unwrap_or_default().to_string();
    let key = ResourceStorage::cluster_key("clusterroles", &name);
    match storage.get(&key).await {
        Ok(mut stored) => {
            let pinned = stored["metadata"]["annotations"]
                ["rbac.authorization.kubernetes.io/autoupdate"]
                .as_str()
                == Some("false");
            if pinned || stored["rules"] == role["rules"] {
                return;
            }
            stored["rules"] = role["rules"].clone();
            let rev = stored["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
            match storage.update(&key, stored, rev).await {
                Ok(_) => tracing::info!("bootstrap: clusterroles {name}: rules updated"),
                // Another replica reconciled it first.
                Err(e) if e.reason == "Conflict" => {}
                Err(e) => tracing::error!("bootstrap: clusterroles {name} not updated: {}", e.message),
            }
        }
        Err(_) => {
            let mut role = role;
            crate::handlers::resource::ensure_metadata_pub(&mut role, &name, None);
            create_bootstrap(storage, &key, role, &format!("clusterroles {name}")).await;
        }
    }
}

/// Create a bootstrap ClusterRoleBinding, or bring a stored one's roleRef and
/// subjects up to date (#176: the control-plane bindings moved from
/// cluster-admin to their own roles). The same opt-out as the roles: one
/// annotated `rbac.authorization.kubernetes.io/autoupdate: "false"` is left.
async fn reconcile_bootstrap_binding(storage: &ResourceStorage, binding: serde_json::Value) {
    let name = binding["metadata"]["name"].as_str().unwrap_or_default().to_string();
    let key = ResourceStorage::cluster_key("clusterrolebindings", &name);
    match storage.get(&key).await {
        Ok(mut stored) => {
            let pinned = stored["metadata"]["annotations"]["rbac.authorization.kubernetes.io/autoupdate"]
                .as_str()
                == Some("false");
            if pinned || (stored["roleRef"] == binding["roleRef"] && stored["subjects"] == binding["subjects"]) {
                return;
            }
            let was = stored["roleRef"]["name"].as_str().unwrap_or("").to_string();
            stored["roleRef"] = binding["roleRef"].clone();
            stored["subjects"] = binding["subjects"].clone();
            let rev = stored["metadata"]["resourceVersion"].as_str().and_then(|r| r.parse().ok());
            match storage.update(&key, stored, rev).await {
                Ok(_) => tracing::info!("bootstrap: clusterrolebindings {name}: {was} → {}", binding["roleRef"]["name"]),
                Err(e) if e.reason == "Conflict" => {}
                Err(e) => tracing::error!("bootstrap: clusterrolebindings {name} not updated: {}", e.message),
            }
        }
        Err(_) => {
            let mut binding = binding;
            crate::handlers::resource::ensure_metadata_pub(&mut binding, &name, None);
            create_bootstrap(storage, &key, binding, &format!("clusterrolebindings {name}")).await;
        }
    }
}

/// Bootstrap RBAC resources for initial cluster access.
async fn bootstrap_rbac(
    storage: &ResourceStorage,
    anonymous_auth: bool,
    dev_anonymous_admin: bool,
) {
    // ClusterRole: cluster-admin — all verbs, all resources, all groups
    let cluster_admin_role = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": "cluster-admin",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "rules": [{
            "apiGroups": ["*"],
            "resources": ["*"],
            "verbs": ["*"]
        }]
    });
    create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterroles", "cluster-admin"),
            cluster_admin_role,
            &format!("clusterroles {}", "cluster-admin"),
        )
        .await;

    // ClusterRoleBinding: system:masters → cluster-admin
    let masters_binding = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {
            "name": "system:masters",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "cluster-admin"
        },
        "subjects": [{
            "kind": "Group",
            "name": "system:masters",
            "apiGroup": "rbac.authorization.k8s.io"
        }]
    });
    create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterrolebindings", "system:masters"),
            masters_binding,
            &format!("clusterrolebindings {}", "system:masters"),
        )
        .await;

    // Who may read /metrics and the health endpoints (#90): upstream's
    // `system:monitoring` role, bound to the group of the same name.
    reconcile_bootstrap_role(storage, json!({
        "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
        "metadata": {"name": "system:monitoring"},
        "rules": [{"nonResourceURLs": ["/healthz", "/healthz/*", "/livez", "/livez/*", "/metrics", "/metrics/slis", "/readyz", "/readyz/*"],
                   "verbs": ["get"]}],
    }))
    .await;
    reconcile_bootstrap_binding(storage, json!({
        "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
        "metadata": {"name": "system:monitoring"},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "system:monitoring"},
        "subjects": [{"kind": "Group", "name": "system:monitoring", "apiGroup": "rbac.authorization.k8s.io"}],
    }))
    .await;

    // What an aggregated API server binds itself to (#83), as upstream
    // bootstraps them: delegated authentication/authorization, and reading
    // the front-proxy contract in kube-system.
    reconcile_bootstrap_role(storage, json!({
        "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
        "metadata": {"name": "system:auth-delegator"},
        "rules": [
            {"apiGroups": ["authentication.k8s.io"], "resources": ["tokenreviews"], "verbs": ["create"]},
            {"apiGroups": ["authorization.k8s.io"], "resources": ["subjectaccessreviews"], "verbs": ["create"]},
        ],
    }))
    .await;
    let mut reader = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
        "metadata": {"name": "extension-apiserver-authentication-reader", "namespace": "kube-system"},
        "rules": [{"apiGroups": [""], "resources": ["configmaps"],
                   "resourceNames": ["extension-apiserver-authentication"], "verbs": ["get", "list", "watch"]}],
    });
    crate::handlers::resource::ensure_metadata_pub(&mut reader, "extension-apiserver-authentication-reader", Some("kube-system"));
    create_bootstrap(
        storage,
        &ResourceStorage::namespaced_key("roles", "kube-system", "extension-apiserver-authentication-reader"),
        reader,
        "roles kube-system/extension-apiserver-authentication-reader",
    )
    .await;

    // The control-plane components (they authenticate via their client certs
    // as these users) get their own least-privilege roles, not cluster-admin
    // (#176). Reconciled every boot, and a binding left pointing at
    // cluster-admin by an earlier release is repointed.
    for role in [
        crate::control_plane_rbac::controller_manager_role(),
        crate::control_plane_rbac::scheduler_role(),
    ] {
        reconcile_bootstrap_role(storage, role).await;
    }
    for user in [crate::control_plane_rbac::CONTROLLER_MANAGER, crate::control_plane_rbac::SCHEDULER] {
        reconcile_bootstrap_binding(storage, crate::control_plane_rbac::binding(user)).await;
    }

    // kube-system/node-admin: the identity of a node's ssh login (#79).
    let (node_admin_sa, node_admin_binding) = node_admin_objects();
    create_bootstrap(
        storage,
        &ResourceStorage::namespaced_key("serviceaccounts", "kube-system", NODE_ADMIN),
        node_admin_sa,
        "serviceaccounts kube-system/node-admin",
    )
    .await;
    create_bootstrap(
        storage,
        &ResourceStorage::cluster_key("clusterrolebindings", NODE_ADMIN),
        node_admin_binding,
        "clusterrolebindings node-admin",
    )
    .await;

    // Node-join bootstrap: bootstrappers may create CSRs; joined nodes (the
    // system:nodes group) get broad access (tighten to a node role later).
    let bootstrapper_role = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": "system:node-bootstrapper",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "rules": [{
            "apiGroups": ["certificates.k8s.io"],
            "resources": ["certificatesigningrequests"],
            "verbs": ["create", "get", "list", "watch"]
        }]
    });
    create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterroles", "system:node-bootstrapper"),
            bootstrapper_role,
            &format!("clusterroles {}", "system:node-bootstrapper"),
        )
        .await;
    for (name, group, role) in [
        ("system:node-bootstrapper", "system:bootstrappers", "system:node-bootstrapper"),
        ("system:nodes", "system:nodes", "cluster-admin"),
    ] {
        let binding = json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "ClusterRoleBinding",
            "metadata": {
                "name": name,
                "uid": uuid::Uuid::new_v4().to_string(),
                "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
            },
            "roleRef": {
                "apiGroup": "rbac.authorization.k8s.io",
                "kind": "ClusterRole",
                "name": role
            },
            "subjects": [{
                "kind": "Group",
                "name": group,
                "apiGroup": "rbac.authorization.k8s.io"
            }]
        });
        create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterrolebindings", name),
            binding,
            &format!("clusterrolebindings {}", name),
        )
        .await;
    }

    // ClusterRole: system:basic-user — what any authenticated user may do
    // about *themselves* (#59).
    //
    // Asking "may I?" reveals nothing the asker could not learn by trying the
    // action, so upstream grants it to everyone who authenticated at all, and
    // a console depends on it: without this binding every viewer's first
    // question is answered 403 and the console falls back to probing.
    let basic_user_role = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": "system:basic-user",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "rules": [{
            "apiGroups": ["authorization.k8s.io"],
            "resources": ["selfsubjectaccessreviews", "selfsubjectrulesreviews"],
            "verbs": ["create"]
        }]
    });
    create_bootstrap(
        storage,
        &ResourceStorage::cluster_key("clusterroles", "system:basic-user"),
        basic_user_role,
        "clusterroles system:basic-user",
    )
    .await;
    let basic_user_binding = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {
            "name": "system:basic-user",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "system:basic-user"
        },
        "subjects": [{
            "kind": "Group",
            "name": "system:authenticated",
            "apiGroup": "rbac.authorization.k8s.io"
        }]
    });
    create_bootstrap(
        storage,
        &ResourceStorage::cluster_key("clusterrolebindings", "system:basic-user"),
        basic_user_binding,
        "clusterrolebindings system:basic-user",
    )
    .await;

    // Projects (#97): the roles a project is shared with, and the grants that
    // let any authenticated user list their projects and request one.
    for role in crate::handlers::project::bootstrap_cluster_roles() {
        reconcile_bootstrap_role(storage, role).await;
    }
    for binding in crate::handlers::project::bootstrap_cluster_role_bindings() {
        let name = binding["metadata"]["name"].as_str().unwrap_or_default().to_string();
        let mut binding = binding;
        crate::handlers::resource::ensure_metadata_pub(&mut binding, &name, None);
        create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterrolebindings", &name),
            binding,
            &format!("clusterrolebindings {name}"),
        )
        .await;
    }

    // ClusterRole: system:discovery — GET on discovery endpoints
    let discovery_role = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": {
            "name": "system:discovery",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "rules": [{
            "nonResourceURLs": ["/api", "/apis", "/api/*", "/apis/*", "/healthz", "/version"],
            "verbs": ["get"]
        }]
    });
    create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterroles", "system:discovery"),
            discovery_role,
            &format!("clusterroles {}", "system:discovery"),
        )
        .await;

    // ClusterRoleBinding: anonymous → discovery
    let anon_discovery_binding = json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": {
            "name": "system:anonymous-discovery",
            "uid": uuid::Uuid::new_v4().to_string(),
            "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "ClusterRole",
            "name": "system:discovery"
        },
        "subjects": [{
            "kind": "User",
            "name": "system:anonymous",
            "apiGroup": "rbac.authorization.k8s.io"
        }]
    });
    create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterrolebindings", "system:anonymous-discovery"),
            anon_discovery_binding,
            &format!("clusterrolebindings {}", "system:anonymous-discovery"),
        )
        .await;

    // Dev only, explicit opt-in: anonymous gets cluster-admin (so kubectl works
    // without certs). Gated on --dev-anonymous-admin, NOT on --anonymous-auth
    // (#16) — so the common `--anonymous-auth=true` case grants anonymous only
    // discovery/health, and a secured cluster never grants standing access.
    if anonymous_auth && dev_anonymous_admin {
        let anon_admin_binding = json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "ClusterRoleBinding",
            "metadata": {
                "name": "system:anonymous-admin",
                "uid": uuid::Uuid::new_v4().to_string(),
                "creationTimestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
            },
            "roleRef": {
                "apiGroup": "rbac.authorization.k8s.io",
                "kind": "ClusterRole",
                "name": "cluster-admin"
            },
            "subjects": [{
                "kind": "User",
                "name": "system:anonymous",
                "apiGroup": "rbac.authorization.k8s.io"
            }]
        });
        create_bootstrap(
            storage,
            &ResourceStorage::cluster_key("clusterrolebindings", "system:anonymous-admin"),
            anon_admin_binding,
            &format!("clusterrolebindings {}", "system:anonymous-admin"),
        )
        .await;
    } else {
        // The grant is not in effect, so the binding must not survive from a
        // boot when it was (#60).
        //
        // Not creating it is not enough: the binding is a stored object, the
        // authorizer reads stored bindings, and the in-memory flag has no say
        // over one that is already there. An apiserver brought up once with
        // `--dev-anonymous-admin true` and later restarted without it kept
        // answering every anonymous request as cluster-admin — which is how a
        // dev rig gets promoted to something real while the flag that was
        // removed reads as though it did something.
        //
        // Deleting is safe because the object is the server's own: it is
        // written at bootstrap and named by the server, so removing it cannot
        // discard anything an operator wrote.
        let key = ResourceStorage::cluster_key("clusterrolebindings", "system:anonymous-admin");
        if storage.get(&key).await.is_ok() {
            match storage.delete(&key, None).await {
                Ok(()) => tracing::warn!(
                    "bootstrap: removed clusterrolebindings/system:anonymous-admin left by an \
                     earlier --dev-anonymous-admin boot — anonymous no longer has cluster-admin"
                ),
                Err(e) => tracing::error!(
                    "bootstrap: clusterrolebindings/system:anonymous-admin is present and could \
                     not be removed ({}) — ANONYMOUS STILL HAS CLUSTER-ADMIN",
                    e.message
                ),
            }
        }
    }
}

/// Records the metrics real dashboards/alerts need (#13): request rate by
/// verb/resource/code, request latency histogram, and in-flight requests —
/// bringing the apiserver exporter to the bar the CM/scheduler exporters set.
async fn metrics_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let method = req.method().clone();
    let attrs = RequestAttributes::of(&path, &query, &method);

    // Upstream splits in-flight by mutating vs read-only, because they are
    // limited separately and a burst of one says something different from a
    // burst of the other.
    let kind = if attrs.mutating { "mutating" } else { "readOnly" };
    metrics::gauge!("apiserver_current_inflight_requests", "request_kind" => kind).increment(1.0);
    let started = std::time::Instant::now();

    let response = next.run(req).await;

    let elapsed = started.elapsed().as_secs_f64();
    let code = response.status().as_u16().to_string();
    metrics::gauge!("apiserver_current_inflight_requests", "request_kind" => kind).decrement(1.0);

    // The upstream label set, exactly: verb, group, version, resource, scope,
    // code. A dashboard written for Kubernetes reads these names and no
    // others, which is the entire reason for matching them.
    metrics::counter!(
        "apiserver_request_total",
        "verb" => attrs.verb,
        "group" => attrs.group.clone(),
        "version" => attrs.version.clone(),
        "resource" => attrs.resource.clone(),
        "scope" => attrs.scope,
        "code" => code,
    )
    .increment(1);
    metrics::histogram!(
        "apiserver_request_duration_seconds",
        "verb" => attrs.verb,
        "group" => attrs.group,
        "version" => attrs.version,
        "resource" => attrs.resource,
        "scope" => attrs.scope,
    )
    .record(elapsed);

    response
}

/// What upstream labels a request with.
struct RequestAttributes {
    /// The **Kubernetes** verb, not the HTTP method: a GET of a collection is
    /// a `list`, a GET with `?watch=true` is a `watch`, and a dashboard that
    /// asks "how many lists are we serving" is asking about the first.
    verb: &'static str,
    group: String,
    version: String,
    resource: String,
    /// `cluster`, `namespace` or `resource` — how much the request covers.
    scope: &'static str,
    mutating: bool,
}

impl RequestAttributes {
    fn of(path: &str, query: &str, method: &axum::http::Method) -> Self {
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let watching = query.split('&').any(|p| p == "watch=true" || p == "watch=1");

        let (group, version, rest): (String, String, &[&str]) = match segments.as_slice() {
            ["api", version, rest @ ..] => ("".into(), (*version).to_string(), rest),
            ["apis", group, version, rest @ ..] => {
                ((*group).to_string(), (*version).to_string(), rest)
            }
            _ => ("".into(), "".into(), &[][..]),
        };

        // Inside the group/version: either `{resource}[/{name}[/{sub}]]` or
        // `namespaces/{ns}/{resource}[/{name}[/{sub}]]`.
        let (namespaced, tail): (bool, &[&str]) = match rest {
            ["namespaces", _ns, tail @ ..] if !tail.is_empty() => (true, tail),
            other => (false, other),
        };
        let resource = tail.first().copied().unwrap_or("").to_string();
        let named = tail.len() >= 2;
        let subresource = tail.get(2).copied();

        let scope = if resource.is_empty() {
            "cluster"
        } else if named {
            "resource"
        } else if namespaced {
            "namespace"
        } else {
            "cluster"
        };

        let verb = match method.as_str() {
            "GET" | "HEAD" => {
                if watching {
                    "watch"
                } else if named {
                    "get"
                } else {
                    "list"
                }
            }
            "POST" => "create",
            "PUT" => "update",
            "PATCH" => "patch",
            "DELETE" => {
                if named {
                    "delete"
                } else {
                    "deletecollection"
                }
            }
            _ => "other",
        };

        let resource = match (resource.as_str(), subresource) {
            ("", _) => non_resource_label(path).to_string(),
            (r, Some(sub)) => format!("{r}/{sub}"),
            (r, None) => r.to_string(),
        };

        Self {
            verb,
            group,
            version,
            resource,
            scope,
            mutating: matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE"),
        }
    }
}

/// A label for the paths that are not resources at all.
fn non_resource_label(path: &str) -> &'static str {
    if path.starts_with("/openapi") {
        "openapi"
    } else if path == "/api" || path.starts_with("/apis") || path == "/version" {
        "discovery"
    } else if path.starts_with("/healthz")
        || path.starts_with("/livez")
        || path.starts_with("/readyz")
    {
        "health"
    } else if path == "/metrics" {
        "metrics"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::first_service_ip;

    #[test]
    fn bootstrap_endpoints_have_stable_uid_and_repair_legacy_objects() {
        for slice in [false,true] {
            let ports = serde_json::json!([{"name":"https","port":6443,"protocol":"TCP"}]);
            let first = super::bootstrap_endpoint(None,slice,"127.0.0.1",&ports);
            assert!(!first["metadata"]["uid"].as_str().unwrap().is_empty());
            let second = super::bootstrap_endpoint(Some(&first),slice,"127.0.0.2",&ports);
            assert_eq!(first["metadata"]["uid"],second["metadata"]["uid"]);
            let mut legacy = second.clone(); legacy["metadata"]["uid"] = serde_json::Value::Null;
            let repaired = super::bootstrap_endpoint(Some(&legacy),slice,"127.0.0.1",&ports);
            assert!(!repaired["metadata"]["uid"].as_str().unwrap().is_empty());
            assert_eq!(super::bootstrap_endpoint(Some(&second),slice,"127.0.0.2",&ports),second);
            assert_eq!(if slice { second["endpoints"].as_array().unwrap().len() }
                else { second["subsets"][0]["addresses"].as_array().unwrap().len() },2);
        }
    }

    #[test]
    fn service_cidr_yields_dot_one() {
        // Upstream assigns the first usable address of the service CIDR to the
        // default/kubernetes Service (#30).
        assert_eq!(
            first_service_ip("10.96.0.0/12").map(|i| i.to_string()),
            Some("10.96.0.1".to_string())
        );
        assert_eq!(
            first_service_ip("172.20.0.0/16").map(|i| i.to_string()),
            Some("172.20.0.1".to_string())
        );
        assert!(first_service_ip("not-a-cidr").is_none());
        assert!(first_service_ip("10.96.0.0").is_none());
    }
}

#[cfg(test)]
mod node_admin_tests {
    use super::node_admin_objects;
    use crate::auth::tests::{sign, test_keys};
    use crate::auth::UserInfo;
    use crate::rbac_engine::subjects_match;

    fn authenticate(sub: &str) -> UserInfo {
        let token = sign(serde_json::json!({
            "sub": sub,
            "exp": chrono::Utc::now().timestamp() + 3600,
        }));
        let (username, groups) = test_keys().validate_token(&token).unwrap().claims.identity();
        UserInfo { username, groups }
    }

    #[test]
    fn the_node_admin_token_is_cluster_admin() {
        // #79: the token stormcert mints, the ServiceAccount it names and the
        // binding that gives it standing all meet.
        let (sa, binding) = node_admin_objects();
        assert_eq!(sa["kind"], "ServiceAccount");
        assert_eq!(sa["metadata"]["namespace"], "kube-system");
        assert_eq!(sa["metadata"]["name"], "node-admin");
        assert_eq!(binding["roleRef"]["kind"], "ClusterRole");
        assert_eq!(binding["roleRef"]["name"], "cluster-admin");

        let user = authenticate("system:serviceaccount:kube-system:node-admin");
        assert!(subjects_match(&binding, &user));
    }

    #[test]
    fn the_binding_names_only_node_admin() {
        let (_, binding) = node_admin_objects();
        for sub in [
            "system:serviceaccount:default:node-admin",
            "system:serviceaccount:kube-system:default",
            "node-admin",
        ] {
            assert!(!subjects_match(&binding, &authenticate(sub)), "{sub} matched");
        }
    }
}

#[cfg(test)]
mod metrics_label_tests {
    use super::RequestAttributes;
    use axum::http::Method;

    fn attrs(path: &str, query: &str, method: Method) -> RequestAttributes {
        RequestAttributes::of(path, query, &method)
    }

    #[test]
    fn a_get_of_a_collection_is_a_list_not_a_get() {
        // The distinction upstream draws, and the one a dashboard asks about:
        // "how many lists are we serving" is a question about load.
        let a = attrs("/api/v1/namespaces/default/pods", "", Method::GET);
        assert_eq!(a.verb, "list");
        assert_eq!(a.resource, "pods");
        assert_eq!(a.scope, "namespace");
        assert_eq!(a.group, "");
        assert_eq!(a.version, "v1");

        let b = attrs("/api/v1/namespaces/default/pods/web", "", Method::GET);
        assert_eq!(b.verb, "get");
        assert_eq!(b.scope, "resource");
    }

    #[test]
    fn a_watch_is_its_own_verb() {
        let a = attrs("/api/v1/pods", "watch=true", Method::GET);
        assert_eq!(a.verb, "watch");
        assert_eq!(a.scope, "cluster");
    }

    #[test]
    fn grouped_resources_carry_their_group_and_version() {
        let a = attrs(
            "/apis/apps/v1/namespaces/kube-system/deployments/coredns",
            "",
            Method::PATCH,
        );
        assert_eq!(a.group, "apps");
        assert_eq!(a.version, "v1");
        assert_eq!(a.resource, "deployments");
        assert_eq!(a.verb, "patch");
        assert!(a.mutating);
    }

    #[test]
    fn a_subresource_is_labelled_as_one() {
        let a = attrs("/api/v1/namespaces/default/pods/web/exec", "", Method::POST);
        assert_eq!(a.resource, "pods/exec");
        assert_eq!(a.verb, "create");

        let b = attrs("/api/v1/nodes/node-a/status", "", Method::PUT);
        assert_eq!(b.resource, "nodes/status");
        assert_eq!(b.scope, "resource");
    }

    #[test]
    fn a_collection_delete_is_deletecollection() {
        let a = attrs("/api/v1/namespaces/default/pods", "", Method::DELETE);
        assert_eq!(a.verb, "deletecollection");
    }

    #[test]
    fn custom_resources_need_no_table_of_known_names() {
        // The old label function matched a hardcoded list and reported "other"
        // for everything else, so every CRD request was invisible.
        let a = attrs(
            "/apis/cilium.io/v2/namespaces/kube-system/ciliumnetworkpolicies",
            "",
            Method::GET,
        );
        assert_eq!(a.group, "cilium.io");
        assert_eq!(a.resource, "ciliumnetworkpolicies");
        assert_eq!(a.verb, "list");
    }

    #[test]
    fn non_resource_paths_are_labelled_not_blank() {
        assert_eq!(attrs("/healthz", "", Method::GET).resource, "health");
        assert_eq!(attrs("/metrics", "", Method::GET).resource, "metrics");
        assert_eq!(attrs("/openapi/v3", "", Method::GET).resource, "openapi");
    }
}

#[cfg(test)]
mod extension_authentication_tests {
    #[test]
    fn the_front_proxy_contract_is_published_as_upstream_spells_it() {
        let d = super::extension_authentication_data(Some("CLIENT"), Some("PROXY"), &["front-proxy-client".into()]);
        assert_eq!(d["client-ca-file"], "CLIENT");
        assert_eq!(d["requestheader-client-ca-file"], "PROXY");
        assert_eq!(d["requestheader-allowed-names"], r#"["front-proxy-client"]"#);
        assert_eq!(d["requestheader-username-headers"], r#"["X-Remote-User"]"#);
        assert_eq!(d["requestheader-group-headers"], r#"["X-Remote-Group"]"#);
        assert_eq!(d["requestheader-extra-headers-prefix"], r#"["X-Remote-Extra-"]"#);
        let only_client = super::extension_authentication_data(Some("CLIENT"), None, &[]);
        assert_eq!(only_client.as_object().unwrap().len(), 1);
    }
}
