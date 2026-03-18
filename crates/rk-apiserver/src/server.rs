//! API server setup and startup.
//!
//! Builds the axum router with all K8s API routes and starts
//! the HTTPS listener.

use crate::config::ApiServerConfig;
use crate::discovery;
use crate::handlers::resource;
use crate::handlers::AppState;
use crate::storage::ResourceStorage;
use axum::routing::get;
use axum::Router;
use rk_core::store::KvStore;
use rk_store::StormforceStore;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

/// Build the complete K8s API router.
fn build_router(state: AppState) -> Router {
    Router::new()
        // Discovery & health
        .route("/version", get(discovery::version))
        .route("/healthz", get(discovery::healthz))
        .route("/livez", get(discovery::livez))
        .route("/readyz", get(discovery::readyz))
        .route("/api", get(discovery::api_versions))
        .route("/apis", get(discovery::api_groups))
        .route("/api/v1", get(discovery::api_v1_resources))
        .route("/apis/apps/v1", get(discovery::api_apps_v1_resources))
        .route(
            "/apis/coordination.k8s.io/v1",
            get(discovery::api_coordination_v1_resources),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1",
            get(discovery::api_rbac_v1_resources),
        )
        // Core v1 — cluster-scoped resources
        .route(
            "/api/v1/{resource}",
            get(resource::list_cluster_resources)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/api/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource),
        )
        // Core v1 — namespace-scoped resources
        .route(
            "/api/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/api/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        // Apps v1 — namespace-scoped resources
        .route(
            "/apis/apps/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/apps/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        // Apps v1 — cluster-scoped list (e.g., kubectl get deployments --all-namespaces)
        .route(
            "/apis/apps/v1/{resource}",
            get(resource::list_cluster_resources),
        )
        // Coordination v1
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/coordination.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        .route(
            "/apis/coordination.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources),
        )
        // RBAC v1
        // RustKube v1alpha1 (PodMigration)
        .route(
            "/apis/rustkube.io/v1alpha1",
            get(discovery::api_rustkube_v1alpha1_resources),
        )
        .route(
            "/apis/rustkube.io/v1alpha1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/rustkube.io/v1alpha1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        .route(
            "/apis/rustkube.io/v1alpha1/{resource}",
            get(resource::list_cluster_resources),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        .with_state(state)
}

/// Start the API server.
pub async fn run(config: ApiServerConfig) -> anyhow::Result<()> {
    // Open embedded store
    let store = StormforceStore::open(&config.data_dir)?;
    let kv: Arc<dyn KvStore> = Arc::new(store);
    let storage = Arc::new(ResourceStorage::new(kv));

    // Bootstrap default namespace
    bootstrap_namespace(&storage, "default").await;
    bootstrap_namespace(&storage, "kube-system").await;
    bootstrap_namespace(&storage, "kube-public").await;
    bootstrap_namespace(&storage, "kube-node-lease").await;

    let state = AppState { storage };
    let app = build_router(state);

    let addr = format!("{}:{}", config.bind_addr, config.secure_port);
    info!("RustKube API server listening on {addr}");

    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Create a namespace if it doesn't already exist.
async fn bootstrap_namespace(storage: &ResourceStorage, name: &str) {
    let key = ResourceStorage::cluster_key("namespaces", name);
    let ns = serde_json::json!({
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
    let _ = storage.create(&key, ns).await;
}
