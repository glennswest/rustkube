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
use axum::routing::{get, patch};
use axum::Router;
use apimachinery::store::KvStore;
use storage::{EtcdStore, EtcdTls};
use serde_json::json;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

/// Build the complete K8s API router.
fn build_router(state: AppState, signing_keys: SigningKeys, rbac: Arc<RbacEngine>) -> Router {
    Router::new()
        // Discovery & health
        .route("/version", get(discovery::version))
        .route("/healthz", get(discovery::healthz))
        .route("/livez", get(discovery::livez))
        .route("/readyz", get(discovery::readyz))
        .route("/api", get(discovery::api_versions))
        .route("/apis", get(discovery::api_groups_dynamic))
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
        // Batch v1 — namespace-scoped resources (jobs, cronjobs)
        .route(
            "/apis/batch/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/batch/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        .route(
            "/apis/batch/v1/{resource}",
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
        // Status subresource routes — core v1 cluster-scoped
        .route(
            "/api/v1/{resource}/{name}/status",
            get(resource::get_cluster_status)
                .put(resource::update_cluster_status)
                .merge(patch(resource::patch_cluster_status)),
        )
        // Status subresource routes — core v1 namespace-scoped
        .route(
            "/api/v1/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
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
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/autoscaling/v2/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        .route(
            "/apis/autoscaling/v2/namespaces/{namespace}/{resource}/{name}/status",
            get(resource::get_namespaced_status)
                .put(resource::update_namespaced_status)
                .merge(patch(resource::patch_namespaced_status)),
        )
        .route(
            "/apis/autoscaling/v2/{resource}",
            get(resource::list_cluster_resources),
        )
        // networking.k8s.io/v1
        .route(
            "/apis/networking.k8s.io/v1",
            get(discovery::api_networking_v1_resources),
        )
        .route(
            "/apis/networking.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/networking.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        .route(
            "/apis/networking.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/networking.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource),
        )
        // admissionregistration.k8s.io/v1
        .route(
            "/apis/admissionregistration.k8s.io/v1",
            get(discovery::api_admissionregistration_v1_resources),
        )
        .route(
            "/apis/admissionregistration.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/admissionregistration.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource),
        )
        // gateway.networking.k8s.io/v1
        .route(
            "/apis/gateway.networking.k8s.io/v1",
            get(discovery::api_gateway_v1_resources),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources)
                .post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/gateway.networking.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource),
        )
        // apiregistration.k8s.io/v1
        .route(
            "/apis/apiregistration.k8s.io/v1",
            get(discovery::api_apiregistration_v1_resources),
        )
        .route(
            "/apis/apiregistration.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources)
                .post(resource::create_cluster_resource),
        )
        .route(
            "/apis/apiregistration.k8s.io/v1/{resource}/{name}",
            get(resource::get_cluster_resource)
                .put(resource::update_cluster_resource)
                .delete(resource::delete_cluster_resource),
        )
        // apiextensions.k8s.io/v1 — CRD management
        .route(
            "/apis/apiextensions.k8s.io/v1/{resource}",
            get(crd::crd_list_cluster).post(crd::crd_create_cluster),
        )
        .route(
            "/apis/apiextensions.k8s.io/v1/{resource}/{name}",
            get(crd::crd_get_cluster)
                .put(crd::crd_update_cluster)
                .delete(crd::crd_delete_cluster),
        )
        // CRD catch-all routes for dynamic custom resources
        .route(
            "/apis/{group}/{version}/{resource}",
            get(crd::crd_list_cluster).post(crd::crd_create_cluster),
        )
        .route(
            "/apis/{group}/{version}/{resource}/{name}",
            get(crd::crd_get_cluster)
                .put(crd::crd_update_cluster)
                .delete(crd::crd_delete_cluster),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}",
            get(crd::crd_list_ns).post(crd::crd_create_ns),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}",
            get(crd::crd_get_ns)
                .put(crd::crd_update_ns)
                .delete(crd::crd_delete_ns),
        )
        // Dynamic CRD discovery
        .route("/apis/{group}/{version}", get(crd::crd_api_resources))
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
            async move {
                let mut req: axum::extract::Request = req;
                req.extensions_mut().insert(keys);
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
    let storage = Arc::new(ResourceStorage::new(kv));

    // Bootstrap default namespaces
    bootstrap_namespace(&storage, "default").await;
    bootstrap_namespace(&storage, "kube-system").await;
    bootstrap_namespace(&storage, "kube-public").await;
    bootstrap_namespace(&storage, "kube-node-lease").await;

    // Bootstrap RBAC resources
    bootstrap_rbac(&storage).await;

    // Initialize CRD registry and load existing CRDs
    let crd_registry = Arc::new(CrdRegistry::new());
    crd::load_existing_crds(&storage, &crd_registry).await;

    // Initialize auth signing keys
    let signing_keys = SigningKeys::generate();

    // Initialize RBAC engine
    let rbac = Arc::new(RbacEngine::new(storage.clone()));

    let state = AppState {
        storage,
        crd_registry,
    };
    let app = build_router(state, signing_keys, rbac);

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
            info!("kube-apiserver serving HTTPS on {addr}");
            let cfg = crate::tls::server_config(&cert, &key)?;
            crate::tls::serve(listener, app, cfg).await?;
        }
        None => {
            info!("kube-apiserver serving HTTP on {addr} (TLS not configured)");
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}

/// Create a namespace if it doesn't already exist.
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
    let _ = storage.create(&key, ns).await;
}

/// Bootstrap RBAC resources for initial cluster access.
async fn bootstrap_rbac(storage: &ResourceStorage) {
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
    let _ = storage
        .create(
            &ResourceStorage::cluster_key("clusterroles", "cluster-admin"),
            cluster_admin_role,
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
    let _ = storage
        .create(
            &ResourceStorage::cluster_key("clusterrolebindings", "system:masters"),
            masters_binding,
        )
        .await;

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
    let _ = storage
        .create(
            &ResourceStorage::cluster_key("clusterroles", "system:discovery"),
            discovery_role,
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
    let _ = storage
        .create(
            &ResourceStorage::cluster_key("clusterrolebindings", "system:anonymous-discovery"),
            anon_discovery_binding,
        )
        .await;

    // Dev mode: anonymous gets cluster-admin (so kubectl works without certs)
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
    let _ = storage
        .create(
            &ResourceStorage::cluster_key("clusterrolebindings", "system:anonymous-admin"),
            anon_admin_binding,
        )
        .await;
}
