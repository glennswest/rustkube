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
use tracing::info;

/// Build the complete K8s API router.
fn build_router(
    state: AppState,
    signing_keys: SigningKeys,
    rbac: Arc<RbacEngine>,
    anonymous_auth: bool,
) -> Router {
    Router::new()
        // Discovery & health
        .route("/version", get(discovery::version))
        .route("/healthz", get(discovery::healthz))
        .route("/livez", get(discovery::livez))
        .route("/readyz", get(discovery::readyz))
        .route("/api", get(discovery::api_versions))
        // OpenAPI — kubectl apply downloads these to validate manifests;
        // a 404 aborts the apply before any write (#25 follow-up).
        .route("/openapi/v2", get(discovery::openapi_v2))
        .route("/openapi/v3", get(discovery::openapi_v3))
        .route("/openapi/v3/{*path}", get(discovery::openapi_v3_group))
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
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
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
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        .route(
            "/apis/coordination.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources),
        )
        // discovery.k8s.io v1 — EndpointSlices (namespaced). Needed by Cilium /
        // kube-proxy-replacement, which use slices as the modern default (#22).
        .route(
            "/apis/discovery.k8s.io/v1",
            get(discovery::api_discovery_v1_resources),
        )
        .route(
            "/apis/discovery.k8s.io/v1/namespaces/{namespace}/{resource}",
            get(resource::list_namespaced_resources).post(resource::create_namespaced_resource),
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
        // storage.k8s.io v1 — CSI ecosystem (#24): StorageClass, CSIDriver,
        // CSINode, VolumeAttachment (cluster-scoped) + CSIStorageCapacity
        // (namespaced). Plain stored resources driven by the CSI sidecars.
        .route(
            "/apis/storage.k8s.io/v1",
            get(discovery::api_storage_v1_resources),
        )
        .route(
            "/apis/storage.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources).post(resource::create_cluster_resource),
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
            get(resource::list_namespaced_resources).post(resource::create_namespaced_resource),
        )
        .route(
            "/apis/storage.k8s.io/v1/namespaces/{namespace}/{resource}/{name}",
            get(resource::get_namespaced_resource)
                .put(resource::update_namespaced_resource)
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
        )
        // scheduling.k8s.io v1 — PriorityClass (cluster-scoped)
        .route(
            "/apis/scheduling.k8s.io/v1/{resource}",
            get(resource::list_cluster_resources).post(resource::create_cluster_resource),
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
            get(resource::list_cluster_resources).post(resource::create_cluster_resource),
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
            get(resource::get_cluster_status).put(resource::update_cluster_status),
        )
        .route(
            "/apis/certificates.k8s.io/v1/{resource}/{name}/status",
            get(resource::get_cluster_status).put(resource::update_cluster_status),
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
                .delete(resource::delete_namespaced_resource)
                .patch(resource::patch_namespaced_resource),
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
                .delete(resource::delete_cluster_resource)
                .patch(resource::patch_cluster_resource),
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
            get(crd::crd_list_cluster).post(crd::crd_create_cluster),
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
            get(crd::crd_list_ns).post(crd::crd_create_ns),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}",
            get(crd::crd_get_ns)
                .put(crd::crd_update_ns)
                .delete(crd::crd_delete_ns)
                .patch(crd::crd_patch_ns),
        )
        .route(
            "/apis/{group}/{version}/namespaces/{namespace}/{resource}/{name}/status",
            get(crd::crd_get_status_ns)
                .put(crd::crd_update_status_ns)
                .patch(crd::crd_patch_status_ns),
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
    let storage = Arc::new(ResourceStorage::new(kv));

    // Bootstrap default namespaces
    bootstrap_namespace(&storage, "default").await;
    bootstrap_namespace(&storage, "kube-system").await;
    bootstrap_namespace(&storage, "kube-public").await;
    bootstrap_namespace(&storage, "kube-node-lease").await;

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

    // ServiceAccount token signing keys. A real cluster supplies the RSA
    // keypair (--service-account-signing-key-file / --service-account-key-file)
    // so every replica signs and verifies with the same key — tokens then work
    // across apiservers and survive restarts (#11). Without it we fall back to
    // an ephemeral per-process HMAC key, which only works single-replica.
    let signing_keys = match (
        &config.service_account_signing_key,
        &config.service_account_key,
    ) {
        (Some(priv_path), Some(pub_path)) => {
            let priv_pem = std::fs::read(priv_path).map_err(|e| {
                anyhow::anyhow!("reading --service-account-signing-key-file {priv_path:?}: {e}")
            })?;
            let pub_pem = std::fs::read(pub_path).map_err(|e| {
                anyhow::anyhow!("reading --service-account-key-file {pub_path:?}: {e}")
            })?;
            let keys = SigningKeys::from_rsa_pem(&priv_pem, &pub_pem)
                .map_err(|e| anyhow::anyhow!("loading ServiceAccount RSA keypair: {e}"))?;
            tracing::info!(
                "ServiceAccount tokens: RS256 using {priv_path:?} (verify: {pub_path:?})"
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

    // Initialize RBAC engine
    let rbac = Arc::new(RbacEngine::new(storage.clone()));

    let state = AppState {
        storage,
        crd_registry,
    };
    // Prometheus metrics recorder + /metrics endpoint (scraped by ironprom).
    let prom = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("prometheus recorder: {e}"))?;
    metrics::gauge!("apiserver_build_info", "version" => apimachinery::VERSION).set(1.0);

    let app = build_router(state, signing_keys, rbac, config.anonymous_auth)
        .route(
            "/metrics",
            axum::routing::get({
                let h = prom.clone();
                move || {
                    let h = h.clone();
                    async move { h.render() }
                }
            }),
        )
        // Protobuf content negotiation: decode application/vnd.kubernetes.protobuf
        // requests to JSON and re-encode JSON responses when the client asked
        // for protobuf (client-go's default for built-in types) — #32.
        .layer(middleware::from_fn(crate::protobuf_mw::transcode))
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
            let cfg = crate::tls::server_config(&cert, &key, client_ca.as_deref())?;
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

    let mut addresses: Vec<serde_json::Value> = match storage.get(&ep_key).await {
        Ok(ep) => ep["subsets"][0]["addresses"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if addresses
        .iter()
        .any(|a| a["ip"].as_str() == Some(advertise))
    {
        return; // already registered
    }
    addresses.push(json!({ "ip": advertise }));
    addresses.sort_by(|a, b| a["ip"].as_str().unwrap_or("").cmp(b["ip"].as_str().unwrap_or("")));

    let endpoints = json!({
        "apiVersion": "v1",
        "kind": "Endpoints",
        "metadata": { "name": "kubernetes", "namespace": "default" },
        "subsets": [{ "addresses": addresses.clone(), "ports": ports.clone() }]
    });
    if storage.get(&ep_key).await.is_ok() {
        let _ = storage.update(&ep_key, endpoints, None).await;
    } else {
        let _ = storage.create(&ep_key, endpoints).await;
    }

    // Mirror into an EndpointSlice — the modern path Cilium/kube-proxy read.
    let slice_key =
        ResourceStorage::namespaced_key("endpointslices", "default", "kubernetes");
    let slice = json!({
        "apiVersion": "discovery.k8s.io/v1",
        "kind": "EndpointSlice",
        "metadata": {
            "name": "kubernetes",
            "namespace": "default",
            "labels": { "kubernetes.io/service-name": "kubernetes" }
        },
        "addressType": "IPv4",
        "endpoints": addresses.iter().map(|a| json!({
            "addresses": [a["ip"].as_str().unwrap_or("")],
            "conditions": { "ready": true }
        })).collect::<Vec<_>>(),
        "ports": [{ "name": "https", "port": secure_port, "protocol": "TCP" }]
    });
    if storage.get(&slice_key).await.is_ok() {
        let _ = storage.update(&slice_key, slice, None).await;
    } else {
        let _ = storage.create(&slice_key, slice).await;
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

    // ClusterRoleBindings for the control-plane components (they authenticate
    // via their client certs as these users). Bound to cluster-admin for now;
    // can be tightened to the upstream system:kube-* roles later.
    for user in ["system:kube-controller-manager", "system:kube-scheduler"] {
        let binding = json!({
            "apiVersion": "rbac.authorization.k8s.io/v1",
            "kind": "ClusterRoleBinding",
            "metadata": {
                "name": user,
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
                "name": user,
                "apiGroup": "rbac.authorization.k8s.io"
            }]
        });
        let _ = storage
            .create(
                &ResourceStorage::cluster_key("clusterrolebindings", user),
                binding,
            )
            .await;
    }

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
    let _ = storage
        .create(
            &ResourceStorage::cluster_key("clusterroles", "system:node-bootstrapper"),
            bootstrapper_role,
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
        let _ = storage
            .create(
                &ResourceStorage::cluster_key("clusterrolebindings", name),
                binding,
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
        let _ = storage
            .create(
                &ResourceStorage::cluster_key("clusterrolebindings", "system:anonymous-admin"),
                anon_admin_binding,
            )
            .await;
    }
}

/// Derive the `resource` label from a request path, mirroring the labels
/// upstream kube-apiserver attaches (`pods`, `namespaces`, `leases`, …). Keeps
/// cardinality bounded: object names and namespaces are collapsed away, so the
/// label set is the finite list of resource types, not one series per object.
fn resource_label(path: &str) -> &'static str {
    // Known resources appear right after `.../v1/` or `.../{namespace}/`.
    const RESOURCES: &[&str] = &[
        "namespaces", "nodes", "pods", "services", "endpoints", "endpointslices",
        "configmaps", "secrets", "serviceaccounts", "events", "persistentvolumes",
        "persistentvolumeclaims", "deployments", "replicasets", "statefulsets",
        "daemonsets", "jobs", "cronjobs", "leases", "customresourcedefinitions",
        "clusterroles", "clusterrolebindings", "roles", "rolebindings",
        "horizontalpodautoscalers", "storageclasses", "csidrivers", "csinodes",
        "volumeattachments", "csistoragecapacities", "certificatesigningrequests",
        "poddisruptionbudgets", "priorityclasses",
    ];
    for seg in path.split('/') {
        if let Some(r) = RESOURCES.iter().find(|r| **r == seg) {
            return r;
        }
    }
    if path.starts_with("/openapi") {
        "openapi"
    } else if path == "/api" || path.starts_with("/apis") || path == "/version" {
        "discovery"
    } else if path.starts_with("/healthz") || path.starts_with("/livez") || path.starts_with("/readyz") {
        "health"
    } else {
        "other"
    }
}

/// Records the metrics real dashboards/alerts need (#13): request rate by
/// verb/resource/code, request latency histogram, and in-flight requests —
/// bringing the apiserver exporter to the bar the CM/scheduler exporters set.
async fn metrics_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().as_str().to_string();
    let resource = resource_label(req.uri().path());

    metrics::gauge!("apiserver_current_inflight_requests").increment(1.0);
    let started = std::time::Instant::now();

    let response = next.run(req).await;

    let elapsed = started.elapsed().as_secs_f64();
    let code = response.status().as_u16().to_string();
    metrics::gauge!("apiserver_current_inflight_requests").decrement(1.0);

    // Backwards-compatible total (kept for existing scrapes) plus the richer,
    // fully-labeled total and the latency histogram.
    metrics::counter!("apiserver_request_total", "method" => method.clone()).increment(1);
    metrics::counter!(
        "apiserver_request_total_by_labels",
        "verb" => method.clone(),
        "resource" => resource,
        "code" => code,
    )
    .increment(1);
    metrics::histogram!(
        "apiserver_request_duration_seconds",
        "verb" => method,
        "resource" => resource,
    )
    .record(elapsed);

    response
}

#[cfg(test)]
mod tests {
    use super::first_service_ip;

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
